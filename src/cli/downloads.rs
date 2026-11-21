//! Download tracking and logging for browser instances.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::api::main_frame::SubscriptionGuard;
use crate::protocol::client::Connection;

/// State of a download.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Started,
    Completed,
    Canceled,
    Failed,
}

/// An entry in the download log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DownloadEntry {
    pub uuid: String,
    pub url: String,
    pub suggested_filename: String,
    pub page_target_id: String,
    pub page: Option<String>,
    pub state: DownloadState,
    pub error: Option<String>,
    pub path: Option<String>,
}

struct LoggedEntry {
    entry: DownloadEntry,
    seq: u64,
}

struct Inner {
    entries: VecDeque<LoggedEntry>,
    seq: u64,
}

/// A download log scoped to a browser context.
pub struct DownloadLog {
    context_id: String,
    downloads_dir: Option<PathBuf>,
    inner: Mutex<Inner>,
    condvar: Condvar,
}

impl DownloadLog {
    pub fn new(context_id: String, downloads_dir: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            context_id,
            downloads_dir,
            inner: Mutex::new(Inner {
                entries: VecDeque::with_capacity(100),
                seq: 0,
            }),
            condvar: Condvar::new(),
        })
    }

    /// Handle a `Browser.downloadCreated` protocol event.
    pub fn on_created(&self, params: &serde_json::Value) {
        if params.get("browserContextId").and_then(|v| v.as_str()) != Some(&self.context_id) {
            return;
        }

        let Some(uuid) = params.get("uuid").and_then(|v| v.as_str()) else {
            return;
        };
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let suggested_filename = params
            .get("suggestedFileName")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let page_target_id = params
            .get("pageTargetId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.seq += 1;
        let seq = inner.seq;

        let entry = DownloadEntry {
            uuid: uuid.to_string(),
            url,
            suggested_filename,
            page_target_id,
            page: None,
            state: DownloadState::Started,
            error: None,
            path: None,
        };

        if let Some(existing) = inner.entries.iter_mut().find(|e| e.entry.uuid == uuid) {
            existing.entry = entry;
            existing.seq = seq;
        } else {
            if inner.entries.len() >= 100 {
                inner.entries.pop_front();
            }
            inner.entries.push_back(LoggedEntry { entry, seq });
        }

        drop(inner);
        self.condvar.notify_all();
    }

    /// Handle a `Browser.downloadFinished` protocol event.
    pub fn on_finished(&self, params: &serde_json::Value) {
        let Some(uuid) = params.get("uuid").and_then(|v| v.as_str()) else {
            return;
        };

        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !inner.entries.iter().any(|e| e.entry.uuid == uuid) {
            return;
        }

        inner.seq += 1;
        let seq = inner.seq;
        let entry_wrapper = inner
            .entries
            .iter_mut()
            .find(|e| e.entry.uuid == uuid)
            .unwrap();
        entry_wrapper.seq = seq;

        let canceled = params
            .get("canceled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let error = params.get("error").and_then(|v| v.as_str());

        if canceled {
            entry_wrapper.entry.state = DownloadState::Canceled;
            entry_wrapper.entry.path = None;
        } else if let Some(err) = error {
            entry_wrapper.entry.state = DownloadState::Failed;
            entry_wrapper.entry.error = Some(err.to_string());
            entry_wrapper.entry.path = None;
        } else {
            entry_wrapper.entry.state = DownloadState::Completed;
            entry_wrapper.entry.error = None;
            // Never derive the saved path from suggested_filename
            entry_wrapper.entry.path = self
                .downloads_dir
                .as_ref()
                .map(|dir| dir.join(uuid).to_string_lossy().into_owned());
        }

        drop(inner);
        self.condvar.notify_all();
    }

    /// Current sequence number and snapshot of all entries in the log.
    pub fn snapshot(&self) -> (u64, Vec<DownloadEntry>) {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entries = inner.entries.iter().map(|e| e.entry.clone()).collect();
        (inner.seq, entries)
    }

    /// Block until `seq > since` or `timeout`; return the new snapshot.
    pub fn wait_change(&self, since: u64, timeout: Duration) -> (u64, Vec<DownloadEntry>) {
        let start = std::time::Instant::now();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if inner.seq > since {
                let entries = inner.entries.iter().map(|e| e.entry.clone()).collect();
                return (inner.seq, entries);
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                let entries = inner.entries.iter().map(|e| e.entry.clone()).collect();
                return (inner.seq, entries);
            }
            let remaining = timeout - elapsed;
            let (guard, result) = self
                .condvar
                .wait_timeout(inner, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner = guard;
            if result.timed_out() && inner.seq <= since {
                let entries = inner.entries.iter().map(|e| e.entry.clone()).collect();
                return (inner.seq, entries);
            }
        }
    }

    /// Entries created or updated after sequence `seq`.
    pub fn since(&self, seq: u64) -> Vec<DownloadEntry> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .entries
            .iter()
            .filter(|e| e.seq > seq)
            .map(|e| e.entry.clone())
            .collect()
    }

    /// Subscribe to root session download events.
    pub(crate) fn subscribe(self: &Arc<Self>, conn: &Arc<Connection>) -> Vec<SubscriptionGuard> {
        let weak1 = Arc::downgrade(self);
        let id1 = conn.on_event(
            "",
            "Browser.downloadCreated",
            Arc::new(move |event| {
                if let Some(log) = weak1.upgrade() {
                    log.on_created(&event.params);
                }
            }),
        );
        let guard1 =
            SubscriptionGuard::session(Arc::clone(conn), "", "Browser.downloadCreated", id1);

        let weak2 = Arc::downgrade(self);
        let id2 = conn.on_event(
            "",
            "Browser.downloadFinished",
            Arc::new(move |event| {
                if let Some(log) = weak2.upgrade() {
                    log.on_finished(&event.params);
                }
            }),
        );
        let guard2 =
            SubscriptionGuard::session(Arc::clone(conn), "", "Browser.downloadFinished", id2);

        vec![guard1, guard2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn created_then_finished_completes_with_path() {
        let log = DownloadLog::new("ctx1".into(), Some("/tmp/dl".into()));
        log.on_created(&json!({
            "uuid": "u1",
            "browserContextId": "ctx1",
            "pageTargetId": "t1",
            "frameId": "f",
            "url": "http://x/a.pdf",
            "suggestedFileName": "../evil.pdf"
        }));
        log.on_finished(&json!({"uuid": "u1"}));
        let (_, e) = log.snapshot();
        assert_eq!(e[0].state, DownloadState::Completed);
        assert_eq!(e[0].path.as_deref(), Some("/tmp/dl/u1"));
    }

    #[test]
    fn other_context_ignored() {
        let log = DownloadLog::new("ctx1".into(), Some("/tmp/dl".into()));
        log.on_created(&json!({
            "uuid": "u1",
            "browserContextId": "ctx2",
            "pageTargetId": "t1",
            "frameId": "f",
            "url": "http://x/a.pdf",
            "suggestedFileName": "file.pdf"
        }));
        let (_, e) = log.snapshot();
        assert!(e.is_empty());
    }

    #[test]
    fn canceled_and_error_states() {
        let log = DownloadLog::new("c".into(), Some("/tmp/dl".into()));
        log.on_created(&json!({
            "uuid": "u1",
            "browserContextId": "c",
            "pageTargetId": "t1",
            "frameId": "f",
            "url": "http://x/1.pdf",
            "suggestedFileName": "1.pdf"
        }));
        log.on_created(&json!({
            "uuid": "u2",
            "browserContextId": "c",
            "pageTargetId": "t1",
            "frameId": "f",
            "url": "http://x/2.pdf",
            "suggestedFileName": "2.pdf"
        }));
        log.on_finished(&json!({"uuid": "u1", "canceled": true}));
        log.on_finished(&json!({"uuid": "u2", "error": "network failure"}));
        let (_, e) = log.snapshot();
        assert_eq!(e[0].state, DownloadState::Canceled);
        assert_eq!(e[0].path, None);
        assert_eq!(e[1].state, DownloadState::Failed);
        assert_eq!(e[1].error.as_deref(), Some("network failure"));
        assert_eq!(e[1].path, None);
    }

    #[test]
    fn no_dir_means_no_path() {
        let log = DownloadLog::new("c".into(), None);
        log.on_created(&json!({
            "uuid": "u1",
            "browserContextId": "c",
            "pageTargetId": "t1",
            "frameId": "f",
            "url": "http://x/a.pdf",
            "suggestedFileName": "a.pdf"
        }));
        log.on_finished(&json!({"uuid": "u1"}));
        let (_, e) = log.snapshot();
        assert_eq!(e[0].state, DownloadState::Completed);
        assert_eq!(e[0].path, None);
    }

    #[test]
    fn bounded_to_100() {
        let log = DownloadLog::new("c".into(), None);
        for i in 0..150 {
            log.on_created(&json!({
                "uuid": format!("u{i}"),
                "browserContextId": "c",
                "pageTargetId": "t",
                "frameId": "f",
                "url": format!("http://x/{i}.pdf"),
                "suggestedFileName": format!("{i}.pdf")
            }));
        }
        let (_, e) = log.snapshot();
        assert_eq!(e.len(), 100);
        assert_eq!(e[0].uuid, "u50");
        assert_eq!(e[99].uuid, "u149");
    }

    #[test]
    fn wait_change_wakes_on_event() {
        let log = DownloadLog::new("c".into(), None);
        let (seq, _) = log.snapshot();
        let l2 = Arc::clone(&log);
        let h = std::thread::spawn(move || l2.wait_change(seq, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        log.on_created(&json!({
            "uuid": "u",
            "browserContextId": "c",
            "pageTargetId": "t",
            "frameId": "f",
            "url": "u",
            "suggestedFileName": "s"
        }));
        let (seq2, e) = h.join().unwrap();
        assert!(seq2 > seq);
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn wait_change_times_out() {
        let log = DownloadLog::new("c".into(), None);
        let (seq, _) = log.snapshot();
        let start = std::time::Instant::now();
        let (seq2, _) = log.wait_change(seq, Duration::from_millis(100));
        assert_eq!(seq, seq2);
        assert!(start.elapsed() >= Duration::from_millis(90));
    }

    #[test]
    fn since_filters_by_sequence() {
        let log = DownloadLog::new("c".into(), None);
        log.on_created(&json!({
            "uuid": "u1",
            "browserContextId": "c",
            "pageTargetId": "t",
            "frameId": "f",
            "url": "u1",
            "suggestedFileName": "s1"
        }));
        let (seq1, _) = log.snapshot();
        log.on_created(&json!({
            "uuid": "u2",
            "browserContextId": "c",
            "pageTargetId": "t",
            "frameId": "f",
            "url": "u2",
            "suggestedFileName": "s2"
        }));
        let entries = log.since(seq1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uuid, "u2");

        log.on_finished(&json!({"uuid": "u1"}));
        let entries = log.since(seq1);
        assert_eq!(entries.len(), 2);
    }
}
