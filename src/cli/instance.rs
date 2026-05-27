//! Instance management for the daemon.
//!
//! Each `Instance` holds a running Camoufox browser with its connection,
//! context, and pages. The `InstanceManager` provides the high-level
//! operations that CLI commands map to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use serde_json::json;

use crate::api::main_frame::{Rect, ScreenshotOptions};
use crate::api::{Browser, BrowserOptions, ContextOptions, MainFrame};
use crate::config::LaunchConfig;
use crate::protocol::client::Connection;
use crate::transport::pipe::PipeTransport;

fn default_executable() -> String {
    std::env::var("CAMOUFOX_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        format!("{home}/.cache/camoufox/camoufox")
    })
}

// ---------------------------------------------------------------------------
// ManagedMainFrame
// ---------------------------------------------------------------------------

/// A `MainFrame` plus its CLI-facing page label tracking. The label lives
/// in the parent `HashMap` key; this struct just owns the frame.
pub struct ManagedMainFrame {
    pub main_frame: MainFrame,
}

// ---------------------------------------------------------------------------
// Instance
// ---------------------------------------------------------------------------

/// A running Camoufox browser instance managed by the daemon.
pub struct Instance {
    pub browser: Browser,
    pub child: Child,
    pub version: Option<String>,
    pub pid: u32,
    _profile_dir: tempfile::TempDir,
    pages: HashMap<String, ManagedMainFrame>,
    page_counter: u32,
}

impl Instance {
    /// Create a new page in this instance's context, fully wired with
    /// session, top frame id, and execution context tracking.
    pub fn create_page(&mut self, context: &crate::api::BrowserContext) -> Result<String, String> {
        let main_frame = context
            .new_main_frame()
            .map_err(|e| format!("failed to create page: {e}"))?;

        self.page_counter += 1;
        let page_id = format!("p{}", self.page_counter);
        self.pages
            .insert(page_id.clone(), ManagedMainFrame { main_frame });
        Ok(page_id)
    }

    /// Navigate a page to a URL.
    ///
    /// `timeout` is forwarded to the protocol layer; if the renderer fails
    /// to respond within `timeout` (e.g. the response was a download), the
    /// call returns a `navigate failed` error containing a `Timeout` kind
    /// rather than hanging the daemon.
    ///
    /// Clears the cached execution context so the next `evaluate` waits for
    /// the post-navigation context; the wait happens inside `MainFrame::evaluate`.
    pub fn navigate(
        &self,
        page_id: &str,
        url: &str,
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        // Force `evaluate` to wait for a fresh post-navigation context.
        *mp.main_frame.execution_context_handle().lock().unwrap() = None;

        mp.main_frame
            .navigate(url, Default::default(), timeout)
            .map_err(|e| format!("navigate failed: {e}"))
    }

    /// Evaluate JavaScript on a page.
    pub fn evaluate(
        &self,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        let result = mp
            .main_frame
            .evaluate(expression, timeout)
            .map_err(|e| format!("evaluate failed: {e}"))?;

        // Unwrap `{result: {value: …}}` to just the value, matching today's
        // CLI output shape.
        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .or_else(|| result.get("value"))
            .cloned()
            .unwrap_or(result);
        Ok(value)
    }

    /// Take a screenshot of a page.
    pub fn screenshot(
        &self,
        page_id: &str,
        format: Option<&str>,
        quality: Option<u32>,
        path: Option<&str>,
        timeout: Duration,
    ) -> Result<(Vec<u8>, String), String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        // Get viewport dimensions via evaluate (which itself waits for the
        // execution context if necessary).
        let dims = mp
            .main_frame
            .evaluate("[window.innerWidth, window.innerHeight]", timeout)
            .map_err(|e| format!("failed to get viewport dimensions: {e}"))?;

        let (width, height) = {
            let arr = dims
                .get("result")
                .and_then(|r| r.get("value"))
                .or_else(|| dims.get("value"))
                .unwrap_or(&dims);
            let w = arr.get(0).and_then(|v| v.as_f64()).unwrap_or(1280.0);
            let h = arr.get(1).and_then(|v| v.as_f64()).unwrap_or(720.0);
            (w, h)
        };

        let mime = match format {
            Some("jpeg") | Some("jpg") => "image/jpeg",
            _ => "image/png",
        };

        let options = ScreenshotOptions {
            mime_type: mime.to_string(),
            clip: Rect {
                x: 0.0,
                y: 0.0,
                width,
                height,
            },
            quality,
            omit_device_scale_factor: None,
        };

        let bytes = mp
            .main_frame
            .screenshot(options)
            .map_err(|e| format!("screenshot failed: {e}"))?;

        let ext = if mime == "image/jpeg" { "jpg" } else { "png" };
        let out_path = match path {
            Some(p) => p.to_string(),
            None => format!("/tmp/screenshot-{page_id}.{ext}"),
        };

        std::fs::write(&out_path, &bytes)
            .map_err(|e| format!("failed to write screenshot: {e}"))?;

        Ok((bytes, out_path))
    }

    /// Get page IDs.
    pub fn page_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.pages.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Shut down this instance.
    pub fn stop(self) -> Result<(), String> {
        let Instance {
            browser, mut child, ..
        } = self;
        let _ = browser.close();

        // Wait for graceful exit, kill if needed.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return Ok(()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// InstanceManager
// ---------------------------------------------------------------------------

/// Manages all browser instances for the daemon.
#[derive(Default)]
pub struct InstanceManager {
    instances: HashMap<String, Instance>,
    /// Per-instance context stored separately so we can borrow mutably.
    contexts: HashMap<String, crate::api::BrowserContext>,
    counter: u32,
}

impl InstanceManager {
    pub fn new() -> Self {
        InstanceManager {
            instances: HashMap::new(),
            contexts: HashMap::new(),
            counter: 0,
        }
    }

    /// Launch a new browser instance.
    pub fn launch(
        &mut self,
        headless: Option<bool>,
        executable: Option<&str>,
    ) -> Result<(String, Option<String>, u32), String> {
        let profile_dir =
            tempfile::tempdir().map_err(|e| format!("failed to create temp dir: {e}"))?;

        let config = LaunchConfig {
            executable: PathBuf::from(
                executable
                    .map(|s| s.to_owned())
                    .unwrap_or_else(default_executable),
            ),
            profile_dir: Some(profile_dir.path().to_owned()),
            headless: headless.unwrap_or(true),
            ..Default::default()
        };

        let mut launched = crate::process::unix::spawn(&config)
            .map_err(|e| format!("failed to spawn browser: {e}"))?;

        let pid = launched.child.id();

        let _ = crate::process::readiness::wait_for_ready(&mut launched.child, config.timeout)
            .map_err(|e| format!("browser did not become ready: {e}"))?;

        let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
        let conn = Connection::new(Box::new(transport));
        let session = conn.root_session();
        let browser = Browser::connect(conn, session, BrowserOptions::default())
            .map_err(|e| format!("bootstrap failed: {e}"))?;

        let version = browser.version().map(|s| s.to_owned());

        // Create a default context.
        let context = browser
            .new_context(ContextOptions::default())
            .map_err(|e| format!("failed to create context: {e}"))?;

        // Assign instance ID.
        self.counter += 1;
        let instance_id = format!("{:08x}", self.counter);

        let instance = Instance {
            browser,
            child: launched.child,
            version: version.clone(),
            pid,
            _profile_dir: profile_dir,
            pages: HashMap::new(),
            page_counter: 0,
        };

        self.instances.insert(instance_id.clone(), instance);
        self.contexts.insert(instance_id.clone(), context);

        Ok((instance_id, version, pid))
    }

    /// List all instances.
    pub fn list(&self) -> Vec<serde_json::Value> {
        let mut result = Vec::new();
        for (id, inst) in &self.instances {
            result.push(json!({
                "instance_id": id,
                "pid": inst.pid,
                "version": inst.version,
                "pages": inst.page_ids(),
            }));
        }
        result.sort_by(|a, b| {
            a.get("instance_id")
                .and_then(|v| v.as_str())
                .cmp(&b.get("instance_id").and_then(|v| v.as_str()))
        });
        result
    }

    /// Stop an instance.
    pub fn stop(&mut self, instance_id: &str) -> Result<(), String> {
        let inst = self
            .instances
            .remove(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        self.contexts.remove(instance_id);
        inst.stop()
    }

    /// Create a new page in an instance.
    pub fn new_page(&mut self, instance_id: &str) -> Result<String, String> {
        let context = self
            .contexts
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        // We need a raw pointer dance because we need &mut Instance and &BrowserContext
        // at the same time, but they're in different HashMaps so this is safe.
        let inst = self
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.create_page(context)
    }

    /// Navigate a page.
    pub fn navigate(
        &self,
        instance_id: &str,
        page_id: &str,
        url: &str,
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        let inst = self
            .instances
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.navigate(page_id, url, timeout)
    }

    /// Evaluate JavaScript.
    pub fn evaluate(
        &self,
        instance_id: &str,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let inst = self
            .instances
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.evaluate(page_id, expression, timeout)
    }

    /// Take a screenshot.
    pub fn screenshot(
        &self,
        instance_id: &str,
        page_id: &str,
        format: Option<&str>,
        quality: Option<u32>,
        path: Option<&str>,
        timeout: Duration,
    ) -> Result<(Vec<u8>, String), String> {
        let inst = self
            .instances
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.screenshot(page_id, format, quality, path, timeout)
    }

    /// Number of running instances.
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    /// Shut down all instances.
    pub fn shutdown_all(&mut self) {
        let ids: Vec<String> = self.instances.keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }
}
