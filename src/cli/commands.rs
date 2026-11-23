//! Clap CLI subcommand definitions.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Action to take on an open JavaScript dialog.
#[derive(Clone, Copy, ValueEnum)]
pub enum DialogAction {
    Accept,
    Dismiss,
}

/// Automatic JavaScript dialog handling.
#[derive(Clone, Copy, ValueEnum)]
pub enum DialogMode {
    Manual,
    Accept,
    Dismiss,
}

impl From<DialogMode> for crate::api::DialogPolicy {
    fn from(value: DialogMode) -> Self {
        match value {
            DialogMode::Manual => Self::Manual,
            DialogMode::Accept => Self::Accept,
            DialogMode::Dismiss => Self::Dismiss,
        }
    }
}

#[derive(Parser)]
#[command(name = "camoufox", about = "CLI for Camoufox browser automation")]
pub struct Cli {
    /// Output as JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Path to the daemon Unix socket.
    #[arg(long, global = true, env = "CAMOUFOX_SOCKET")]
    pub socket: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the daemon process.
    Serve,

    /// Record a page's HTTP exchanges into a cassette.
    Record {
        #[command(subcommand)]
        action: RecordAction,
    },

    /// Serve a page's HTTP requests from a cassette, aborting misses.
    Replay {
        #[command(subcommand)]
        action: ReplayAction,
    },

    /// Launch a new browser instance.
    Launch {
        /// Run in headed mode (show browser window).
        #[arg(long)]
        headed: bool,

        /// Path to the Camoufox executable.
        #[arg(long)]
        executable: Option<String>,

        /// Directory to save downloads to (must be absolute; defaults to cancel).
        #[arg(long)]
        downloads_dir: Option<PathBuf>,
        /// Disable page Content-Security-Policy (needed for --main-world on strict-CSP sites; detectable by pages)
        #[arg(long)]
        bypass_csp: bool,
        /// JavaScript dialog handling policy.
        #[arg(long, value_enum, default_value = "manual")]
        dialogs: DialogMode,
        /// Absolute path; created or tightened to 0700, canonicalized, must be empty or an existing profile, never deleted on stop.
        #[arg(long)]
        profile_dir: Option<PathBuf>,
    },

    /// Accept or dismiss an open JavaScript dialog.
    Dialog {
        instance_id: String,
        page_id: String,
        #[arg(value_enum)]
        action: DialogAction,
        #[arg(long)]
        text: Option<String>,
    },

    /// List all running browser instances.
    List,

    /// Stop a browser instance.
    Stop {
        /// Instance ID (e.g., 00000001).
        instance_id: String,
    },

    /// Create a new page in a browser instance.
    NewPage {
        /// Instance ID.
        instance_id: String,
    },

    /// Navigate a page to a URL.
    Navigate {
        /// Instance ID.
        instance_id: String,
        /// Page ID (e.g., p1).
        page_id: String,
        /// URL to navigate to.
        url: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// If set, block until the named lifecycle event fires after navigation.
        /// Supported values: load, domcontentloaded.
        /// Absent: return after the Page.navigate ack (existing behavior).
        #[arg(long)]
        wait_until: Option<String>,
    },

    /// Evaluate JavaScript on a page.
    Evaluate {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// JavaScript expression to evaluate.
        expression: String,
        /// Run in the page's main world so page-defined globals are visible (sync expressions only; detectable by the page; blocked by strict CSP)
        #[arg(long)]
        main_world: bool,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    /// Click an element by CSS selector, or at viewport coordinates (x, y).
    ///
    /// `click <instance> <page> <selector>` resolves the selector, scrolls it
    /// into view and clicks its centre. `click <instance> <page> <x> <y>`
    /// (two numeric args) dispatches a click at raw viewport coordinates.
    Click {
        /// Instance ID.
        instance_id: String,
        /// Page ID (e.g., p1).
        page_id: String,
        /// CSS selector, or the X coordinate when a Y coordinate follows.
        target: String,
        /// Y coordinate (viewport pixels). When present, `target` is the X coordinate.
        y: Option<i32>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    /// Take a screenshot of a page.
    Screenshot {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Output file path.
        #[arg(short = 'o', long = "output")]
        output: Option<String>,
        /// Image format: png or jpeg.
        #[arg(long, default_value = "png")]
        format: String,
        /// JPEG quality (0-100).
        #[arg(long)]
        quality: Option<u32>,
        /// Crop to the element matching this CSS selector.
        #[arg(long, conflicts_with = "clip")]
        selector: Option<String>,
        /// Clip region as `x,y,width,height` in CSS pixels.
        #[arg(long, conflicts_with = "selector")]
        clip: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Shut down the daemon and all browser instances.
    Shutdown,

    /// Ping the daemon.
    Ping,

    /// Export all cookies for a browser instance (including HttpOnly).
    Cookies {
        /// Instance ID (e.g., 00000001).
        instance_id: String,
    },

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------
    /// Print the page's visible text (`innerText`), optionally scoped to a selector.
    Text {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Restrict extraction to the first element matching this CSS selector.
        #[arg(long)]
        selector: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    /// Print page HTML: `outerHTML` of a selector, or the whole document.
    Html {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Return the `outerHTML` of the first element matching this CSS selector.
        #[arg(long)]
        selector: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    /// List every `<a href>` on the page as `text → href`.
    Links {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Only collect links inside the element matching this CSS selector.
        #[arg(long)]
        selector: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    /// Extract structured page metadata (Open Graph / JSON-LD / meta tags).
    ///
    /// With no flags, all three groups are returned.
    Data {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Include Open Graph (`og:` / `twitter:`) tags.
        #[arg(long)]
        og: bool,
        /// Include `application/ld+json` blocks.
        #[arg(long)]
        jsonld: bool,
        /// Include named `<meta>` tags.
        #[arg(long)]
        meta: bool,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    // -----------------------------------------------------------------------
    // Navigation / waiting
    // -----------------------------------------------------------------------
    /// Print the page's current URL and title.
    Url {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Go back one entry in the page's session history.
    Back {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
    },

    /// Go forward one entry in the page's session history.
    Forward {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
    },

    /// Reload the page.
    Reload {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
    },

    /// Poll until an element matching a CSS selector exists.
    Wait {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector to wait for.
        #[arg(long)]
        selector: String,
        /// Give up after this many seconds.
        #[arg(long, default_value = "10")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    // -----------------------------------------------------------------------
    // Cookies / headers
    // -----------------------------------------------------------------------
    /// Set a cookie for the instance's browser context: `name=value`.
    Cookie {
        /// Instance ID.
        instance_id: String,
        /// Page ID (used to derive the cookie URL when neither --url nor --domain is given).
        page_id: String,
        /// Cookie as `name=value`.
        pair: String,
        /// Associate the cookie with this URL instead of the page's current URL.
        #[arg(long)]
        url: Option<String>,
        /// Cookie domain (mutually exclusive with --url).
        #[arg(long, conflicts_with = "url")]
        domain: Option<String>,
        /// Cookie path.
        #[arg(long)]
        path: Option<String>,
        /// Mark the cookie Secure.
        #[arg(long)]
        secure: bool,
        /// Mark the cookie HttpOnly.
        #[arg(long)]
        http_only: bool,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Set an extra HTTP request header for a page: `Name: value`.
    ///
    /// Headers accumulate across calls for the lifetime of the page.
    Header {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Header as `Name: value`.
        pair: String,
    },

    // -----------------------------------------------------------------------
    // Interaction
    // -----------------------------------------------------------------------
    /// Focus the element matching a selector, clear it, and type a value into it.
    Fill {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector of the input/textarea/contenteditable.
        selector: String,
        /// Value to type.
        value: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    /// Set files on a file input, including hidden inputs.
    Upload {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector of the file input. Supports ` >>> ` shadow piercing.
        selector: String,
        /// Files to upload, resolved against the client's working directory.
        #[arg(required = true, num_args = 1.., value_name = "PATH")]
        paths: Vec<String>,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
        /// Overall command timeout in seconds.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Type text into whatever element currently has focus.
    Type {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Text to insert.
        text: String,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
        /// Overall command timeout in seconds.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Press a named key (Enter, Tab, Escape, ArrowDown, a, 1, …).
    Press {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Key name.
        key: String,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
        /// Overall command timeout in seconds.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Move the mouse over the element matching a selector (no click).
    Hover {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector.
        selector: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    /// Choose an option in a `<select>` by value, label, or visible text.
    Select {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector of the `<select>`.
        selector: String,
        /// Option value / label / text to select.
        value: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Scope to an iframe matching this selector (repeatable for nested iframes).
        #[arg(long = "frame", action = clap::ArgAction::Append)]
        frames: Vec<String>,
    },

    /// Scroll an element into view, or scroll to the bottom of the page.
    Scroll {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector to scroll into view. Omit to scroll to the page bottom.
        selector: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    // -----------------------------------------------------------------------
    // Tabs
    // -----------------------------------------------------------------------
    /// List the open pages (tabs) of an instance with their URLs and titles.
    Tabs {
        /// Instance ID.
        instance_id: String,
        /// Per-page timeout in seconds for reading each tab's URL/title.
        /// A tab that can't be read within this budget reports null.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Close a page (tab).
    CloseTab {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
    },

    /// View or wait for downloads on an instance.
    Downloads {
        /// Instance ID.
        instance_id: String,
        /// Optional timeout in seconds to wait for download events.
        #[arg(long)]
        wait: Option<u64>,
    },

    /// External subcommand: `camoufox <name> [args...]` runs `camoufox-<name>` from PATH.
    ///
    /// Built-in subcommand names always win. When `<name>` is not a built-in,
    /// the binary `camoufox-<name>` is looked up on `PATH` and executed with the
    /// remaining arguments, inheriting stdio, the current environment, and
    /// `CAMOUFOX_SOCKET` set to the absolute daemon socket path, and
    /// `CAMOUFOX_JSON=1` when the global `--json` flag is given.
    /// Exit code mirrors the child (signal → 128+n on Unix).
    ///
    /// Names containing path separators are treated as not found.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Look up an external subcommand executable on `PATH`.
///
/// Returns `Some(path)` if `camoufox-<name>` is found on `PATH` and is an
/// executable file. Returns `None` if `name` is empty, contains path
/// separators, or is not found on `PATH`. Relative and empty `PATH` entries
/// are skipped.
pub fn find_external_subcommand(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains(std::path::is_separator) {
        return None;
    }

    let bin_name = format!("camoufox-{name}");
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(&bin_name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = candidate.metadata() {
                if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
                    return Some(candidate);
                }
            }
        }
        #[cfg(not(unix))]
        {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_external_subcommand_rejects_empty_and_separators() {
        use std::os::unix::fs::PermissionsExt;
        // Isolate PATH in a child test process so parallel tests keep their
        // environment. Both separator names lead to real executables if the
        // lookup guard is removed.
        const FIXTURE: &str = "CAMOUFOX_SEPARATOR_FIXTURE";
        if let Some(dir) = std::env::var_os(FIXTURE) {
            let dir = PathBuf::from(dir);
            for name in ["foo/bar", "../cmd"] {
                let candidate = dir.join(format!("camoufox-{name}"));
                assert!(candidate.is_file(), "{}", candidate.display());
                assert!(find_external_subcommand(name).is_none(), "{name}");
            }
            assert!(find_external_subcommand("").is_none());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for name in ["camoufox-foo/bar", "camoufox-../cmd"] {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "#!/bin/sh\necho SEPARATOR_EXECUTED\nexit 42\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env("PATH", dir.path())
            .env(FIXTURE, dir.path())
            .args([
                "--exact",
                "cli::commands::tests::find_external_subcommand_rejects_empty_and_separators",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[cfg(test)]
mod cassette_command_tests {
    use super::*;

    #[test]
    fn record_replay_contract_commands_parse() {
        let cli = Cli::try_parse_from([
            "camoufox",
            "record",
            "start",
            "i",
            "p1",
            "--dir",
            "/tmp/cassette",
            "--url",
            "example",
            "--url",
            "data",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Command::Record { action: RecordAction::Start { urls, .. } } if urls == ["example", "data"])
        );
        let cli = Cli::try_parse_from([
            "camoufox",
            "replay",
            "start",
            "i",
            "p1",
            "--dir",
            "/tmp/cassette",
            "--match-body",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Replay {
                action: ReplayAction::Start {
                    match_body: true,
                    ..
                }
            }
        ));
        assert!(Cli::try_parse_from(["camoufox", "record", "stop", "i", "p1"]).is_ok());
        assert!(Cli::try_parse_from(["camoufox", "replay", "stop", "i", "p1"]).is_ok());
        assert!(Cli::try_parse_from(["camoufox", "record", "start", "i", "p1"]).is_err());
    }
}

/// Start or stop recording a page's HTTP exchanges.
#[derive(Subcommand)]
pub enum RecordAction {
    /// Begin passive recording into an empty absolute directory.
    Start {
        instance_id: String,
        page_id: String,
        /// Absolute cassette directory; must be empty or absent.
        #[arg(long)]
        dir: PathBuf,
        /// Include URLs matching any of these regexes (all URLs by default).
        #[arg(long = "url", action = clap::ArgAction::Append)]
        urls: Vec<String>,
    },
    /// Finalize the cassette and return exchange and body counts.
    Stop {
        instance_id: String,
        page_id: String,
    },
}

/// Start or stop offline replay of a page's HTTP requests.
#[derive(Subcommand)]
pub enum ReplayAction {
    /// Validate a cassette and enable interception for this page.
    Start {
        instance_id: String,
        page_id: String,
        /// Absolute cassette directory containing index.json and bodies/.
        #[arg(long)]
        dir: PathBuf,
        /// Include the raw request body hash in matching.
        #[arg(long)]
        match_body: bool,
    },
    /// Disable interception and return served count and missed requests.
    Stop {
        instance_id: String,
        page_id: String,
    },
}
