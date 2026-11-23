//! Stable error categories for daemon responses.

/// Machine-readable category of a daemon error, independent of its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// No element matches the requested selector.
    SelectorNotFound,
    /// An operation exceeded its deadline.
    Timeout,
    /// A JavaScript dialog is blocking the operation.
    DialogOpen,
    /// A scoped frame was detached.
    FrameDetached,
    /// No element matches a frame selector.
    FrameNotFound,
    /// A frame selector matched an element other than an iframe.
    NotIframe,
    /// A selector cannot access an open shadow root.
    ShadowRoot,
    /// Navigation failed or was diverted into a download.
    Navigation,
    /// The requested browser instance does not exist.
    InstanceNotFound,
    /// The requested page does not exist.
    PageNotFound,
    /// The requested page was closed or crashed.
    PageClosed,
    /// An argument is invalid.
    InvalidArgument,
    /// A filesystem or I/O operation failed.
    Io,
    /// The daemon rejected the request or is shutting down.
    Daemon,
    /// The message does not match a known error shape.
    Other,
}

impl ErrorKind {
    /// Return the stable snake_case wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelectorNotFound => "selector_not_found",
            Self::Timeout => "timeout",
            Self::DialogOpen => "dialog_open",
            Self::FrameDetached => "frame_detached",
            Self::FrameNotFound => "frame_not_found",
            Self::NotIframe => "not_iframe",
            Self::ShadowRoot => "shadow_root",
            Self::Navigation => "navigation",
            Self::InstanceNotFound => "instance_not_found",
            Self::PageNotFound => "page_not_found",
            Self::PageClosed => "page_closed",
            Self::InvalidArgument => "invalid_argument",
            Self::Io => "io",
            Self::Daemon => "daemon",
            Self::Other => "other",
        }
    }

    /// Classify established CLI and protocol message shapes without changing them.
    ///
    /// Rules are anchored at the start of a message, after known frame/operation
    /// wrappers. Specific causes take precedence over the navigation wrapper;
    /// arbitrary script exception text is never searched for error keywords.
    pub fn classify(message: &str) -> Self {
        let mut message = message;
        let mut in_frame = false;
        let mut navigation = false;
        loop {
            if let Some(cause) = message.strip_prefix("--frame ").and_then(frame_cause) {
                message = cause;
                in_frame = true;
                continue;
            }
            // These wrappers are emitted by Instance methods; do not strip
            // arbitrary text ending in "failed", especially script exceptions.
            let wrappers = [
                "navigate",
                "go back",
                "go forward",
                "reload",
                "evaluate",
                "hover",
                "type",
                "press (keydown)",
                "press (keyup)",
                "click (move)",
                "click (down)",
                "click (up)",
                "screenshot",
                "set header",
                "close tab",
                "bootstrap",
                "set_cookies",
                "get_cookies",
            ];
            let Some((operation, cause)) = message.split_once(" failed: ") else {
                break;
            };
            if !wrappers.contains(&operation) {
                break;
            }
            navigation |= matches!(operation, "navigate" | "go back" | "go forward" | "reload");
            message = cause;
        }

        if message == "daemon shutting down"
            || message == "daemon is already running"
            || message == "daemon is not running (start with: camoufox serve)"
            || message == "daemon closed connection without responding"
            || message == "daemon does not support --profile-dir; restart it: camoufox shutdown && camoufox serve"
            || [
                "socket parent directory does not exist: ",
                "failed to connect to daemon: ",
                "failed to send request: ",
                "failed to flush: ",
                "failed to read response: ",
                "invalid response from daemon: ",
                "timed out waiting for daemon after ",
            ]
            .iter()
            .any(|prefix| message.starts_with(prefix))
            || message == "request line too large or missing newline"
            || message.starts_with("invalid request: ")
        {
            return Self::Daemon;
        }
        if message.starts_with("selector not found: ")
            || message
                .strip_prefix("select ")
                .is_some_and(|s| s.ends_with(": selector not found"))
        {
            return Self::SelectorNotFound;
        }
        if message == "timed out"
            || message.starts_with("timed out ")
            || (in_frame && message == "frame has no JavaScript context yet")
            || message.strip_prefix("Request '").is_some_and(|s| {
                s.split_once("' timed out after ")
                    .is_some_and(|(method, _)| !method.is_empty())
            })
        {
            return Self::Timeout;
        }
        let page_detail = message
            .strip_prefix("page ")
            .and_then(|s| s.split_once(' '))
            .filter(|(id, _)| !id.is_empty())
            .map(|(_, detail)| detail);
        let dialog = message
            .strip_prefix("open ")
            .or_else(|| page_detail.and_then(|s| s.strip_prefix("has an open ")));
        if dialog.is_some_and(|s| {
            ["alert", "confirm", "prompt", "beforeunload"]
                .iter()
                .any(|kind| {
                    s.strip_prefix(kind)
                        .is_some_and(|s| s.starts_with(" dialog: "))
                })
        }) {
            return Self::DialogOpen;
        }
        if message == "frame was detached" {
            return Self::FrameDetached;
        }
        if in_frame && message == "no element matches" {
            return Self::FrameNotFound;
        }
        if in_frame && message == "element is not an iframe" {
            return Self::NotIframe;
        }
        if message.starts_with("shadow root: ") {
            return Self::ShadowRoot;
        }
        if message
            .strip_prefix("instance ")
            .and_then(|s| s.strip_suffix(" not found"))
            .is_some_and(|id| !id.is_empty() && !id.chars().any(char::is_whitespace))
        {
            return Self::InstanceNotFound;
        }
        if matches!(page_detail, Some("was closed" | "crashed")) {
            return Self::PageClosed;
        }
        if page_detail == Some("not found") {
            return Self::PageNotFound;
        }
        if message.starts_with("invalid argument: ")
            || message.starts_with("invalid selector ")
            || message.starts_with("invalid selector: ")
            || message.starts_with("unknown key ")
            || message.starts_with("unsupported --wait-until value ")
            || message == "--downloads-dir must be an absolute path"
            || message == "dialog action must be accept or dismiss"
        {
            return Self::InvalidArgument;
        }
        if [
            "io error: ",
            "failed to open screenshot: ",
            "failed to secure screenshot: ",
            "failed to write screenshot: ",
            "failed to create temp dir: ",
            "failed to create downloads dir ",
            "failed to create profile directory ",
            "failed to set permissions on profile directory ",
            "failed to canonicalize profile directory ",
            "failed to inspect profile directory ",
            "failed to create runtime directory: ",
            "cannot inspect runtime directory ",
            "insecure runtime directory ",
        ]
        .iter()
        .any(|prefix| message.starts_with(prefix))
            || message
                .strip_prefix("downloads dir ")
                .is_some_and(|s| s.split_once(" is not writable: ").is_some())
        {
            return Self::Io;
        }
        if navigation || message.starts_with("navigation diverted into download flow (") {
            return Self::Navigation;
        }
        Self::Other
    }
}

// scope_message interpolates CSS verbatim. Ignore delimiters inside quoted
// attributes, functional selectors, and escaped identifiers.
fn frame_cause(scoped: &str) -> Option<&str> {
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 0usize;
    for (index, ch) in scoped.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            }
        } else {
            match ch {
                '\'' | '"' => quote = Some(ch),
                '[' | '(' => depth += 1,
                ']' | ')' => depth = depth.saturating_sub(1),
                ':' if depth == 0 => {
                    if let Some(cause) = scoped[index..].strip_prefix(": ") {
                        return Some(cause);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::ErrorKind;

    macro_rules! classification_regression {
        ($name:ident, $message:literal, $kind:ident) => {
            #[test]
            fn $name() {
                assert_eq!(ErrorKind::classify($message), ErrorKind::$kind);
            }
        };
    }

    classification_regression!(
        profile_create_is_io,
        "failed to create profile directory /profile: denied",
        Io
    );
    classification_regression!(
        profile_permissions_are_io,
        "failed to set permissions on profile directory /profile: denied",
        Io
    );
    classification_regression!(
        profile_canonicalize_is_io,
        "failed to canonicalize profile directory /profile: denied",
        Io
    );
    classification_regression!(
        profile_inspect_is_io,
        "failed to inspect profile directory /profile: denied",
        Io
    );
    classification_regression!(
        client_not_running_is_daemon,
        "daemon is not running (start with: camoufox serve)",
        Daemon
    );
    classification_regression!(
        client_socket_parent_is_daemon,
        "socket parent directory does not exist: /missing",
        Daemon
    );
    classification_regression!(
        client_connect_is_daemon,
        "failed to connect to daemon: denied",
        Daemon
    );
    classification_regression!(
        client_send_is_daemon,
        "failed to send request: broken pipe",
        Daemon
    );
    classification_regression!(
        client_flush_is_daemon,
        "failed to flush: broken pipe",
        Daemon
    );
    classification_regression!(
        client_read_is_daemon,
        "failed to read response: reset",
        Daemon
    );
    classification_regression!(
        client_eof_is_daemon,
        "daemon closed connection without responding",
        Daemon
    );
    classification_regression!(
        client_invalid_response_is_daemon,
        "invalid response from daemon: expected value",
        Daemon
    );
    classification_regression!(
        client_timeout_is_daemon,
        "timed out waiting for daemon after 30s (the daemon may still complete the request)",
        Daemon
    );

    #[test]
    fn closed_and_crashed_pages_have_a_distinct_kind() {
        for message in [
            "page p1 was closed",
            "page p2 crashed",
            "evaluate failed: page p1 was closed",
            "--frame #inner: page p2 crashed",
        ] {
            assert_eq!(
                ErrorKind::classify(message).as_str(),
                "page_closed",
                "{message}"
            );
        }
        for message in [
            "page  was closed",
            "page p1 crashed unexpectedly",
            "page script error: page p1 was closed",
        ] {
            assert_eq!(ErrorKind::classify(message), ErrorKind::Other, "{message}");
        }
    }

    #[test]
    fn wrapped_causes_and_precedence() {
        // Wrappers: src/cli/instance.rs scope_message, operation_error,
        // eval_checked_frame, navigate, select_option, and dialog_message.
        // Causes: src/api/main_frame.rs resolve_child_frame and
        // src/protocol/errors.rs ProtocolError::timeout/dialog_opened.
        let cases = [
            ("--frame #outer: --frame #inner: frame was detached", ErrorKind::FrameDetached),
            ("--frame #outer: evaluate failed: timed out waiting for execution context", ErrorKind::Timeout),
            ("navigate failed: Request 'Page.navigate' timed out after 30s", ErrorKind::Timeout),
            ("navigate failed: unsupported --wait-until value \"networkidle\"; supported values are: load, domcontentloaded", ErrorKind::InvalidArgument),
            ("navigate failed: NS_ERROR_UNKNOWN_HOST", ErrorKind::Navigation),
            ("--frame #inner: shadow root: #closed: element at segment 1 has no open shadow root (absent or closed; cannot pierce)", ErrorKind::ShadowRoot),
            ("--frame #inner: frame has no JavaScript context yet", ErrorKind::Timeout),
            ("select #missing: selector not found", ErrorKind::SelectorNotFound),
            ("page p1 has an open prompt dialog: \"hello\" — run `camoufox dialog 00000001 p1 accept|dismiss [--text T]`", ErrorKind::DialogOpen),
            ("selector not found: [title='timed out']", ErrorKind::SelectorNotFound),
            ("timed out after 1s waiting for selector: #never; last transient error: --frame #inner: frame was detached", ErrorKind::Timeout),
            ("invalid request: selector not found: #missing", ErrorKind::Daemon),
            ("invalid argument: a path must be absolute", ErrorKind::InvalidArgument),
            ("io error: Permission denied (os error 13)", ErrorKind::Io),
        ];
        for (message, expected) in cases {
            assert_eq!(ErrorKind::classify(message), expected, "{message}");
        }
    }

    #[test]
    fn frame_selectors_can_contain_colon_space() {
        // src/cli/instance.rs: scope_message interpolates selectors verbatim.
        for selector in [
            r#"iframe[title="note: example"]"#,
            r"#note\: iframe",
            r#":is([title='note: example'], iframe)"#,
        ] {
            let message = format!("--frame {selector}: selector not found: #missing");
            assert_eq!(
                ErrorKind::classify(&message),
                ErrorKind::SelectorNotFound,
                "{message}"
            );
        }
    }

    #[test]
    fn keywords_in_arbitrary_text_do_not_classify() {
        for message in [
            "page script error: Error: selector not found: #missing",
            "evaluate failed: page script error: Error: timed out",
            "--frame #inner: page script error: Error: frame was detached",
            "custom failure: instance 00000001 not found",
            "instance 00000001 not found in this message",
            "instance  not found",
            "page  not found",
            "no element matches",
            "",
        ] {
            assert_eq!(ErrorKind::classify(message), ErrorKind::Other, "{message}");
        }
    }

    #[test]
    fn remaining_prefixes_and_wrappers_classify() {
        let cases = [
            ("daemon is already running", ErrorKind::Daemon),
            (
                "request line too large or missing newline",
                ErrorKind::Daemon,
            ),
            ("go back failed: NS_ERROR_FAILURE", ErrorKind::Navigation),
            ("go forward failed: NS_ERROR_FAILURE", ErrorKind::Navigation),
            (
                "reload failed: frame was detached",
                ErrorKind::FrameDetached,
            ),
            ("press (keydown) failed: timed out", ErrorKind::Timeout),
            (
                "click (up) failed: open confirm dialog: \"sure?\"",
                ErrorKind::DialogOpen,
            ),
            ("open beforeunload dialog: \"\"", ErrorKind::DialogOpen),
            (
                "page p2 has an open alert dialog: \"hi\"",
                ErrorKind::DialogOpen,
            ),
            ("evaluate failed: page script error: boom", ErrorKind::Other),
            ("invalid selector \"[\": bad", ErrorKind::InvalidArgument),
            ("invalid selector: empty", ErrorKind::InvalidArgument),
            ("unknown key \"Nope\"", ErrorKind::InvalidArgument),
            (
                "dialog action must be accept or dismiss",
                ErrorKind::InvalidArgument,
            ),
            ("failed to secure screenshot: denied", ErrorKind::Io),
            ("failed to write screenshot: disk full", ErrorKind::Io),
            ("failed to create temp dir: denied", ErrorKind::Io),
            ("failed to create downloads dir /x: denied", ErrorKind::Io),
            ("failed to create runtime directory: denied", ErrorKind::Io),
            (
                "cannot inspect runtime directory /run: denied",
                ErrorKind::Io,
            ),
            ("insecure runtime directory /run: mode 777", ErrorKind::Io),
            ("downloads dir /x is not writable: denied", ErrorKind::Io),
            ("downloads dir /x", ErrorKind::Other),
            // Messages introduced alongside upload, record and replay.
            (
                "invalid argument: upload path must be absolute: fixture.pdf",
                ErrorKind::InvalidArgument,
            ),
            (
                "invalid argument: record or replay is already active for this page",
                ErrorKind::InvalidArgument,
            ),
            (
                "invalid argument: invalid cassette: unsupported version",
                ErrorKind::InvalidArgument,
            ),
            (
                "io error: response body was evicted during recording",
                ErrorKind::Io,
            ),
            (
                "invalid argument: profile directory already in use by instance 00000001",
                ErrorKind::InvalidArgument,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(ErrorKind::classify(message), expected, "{message}");
        }
    }

    #[test]
    fn context_sensitive_causes_need_their_wrapper() {
        // Frame-only causes are not classified without a `--frame` wrapper.
        assert_eq!(
            ErrorKind::classify("frame has no JavaScript context yet"),
            ErrorKind::Other
        );
        assert_eq!(
            ErrorKind::classify("element is not an iframe"),
            ErrorKind::Other
        );
        // A protocol timeout needs a non-empty method name.
        assert_eq!(
            ErrorKind::classify("Request '' timed out after 30s"),
            ErrorKind::Other
        );
        // An unterminated quoted frame selector never yields a cause.
        assert_eq!(
            ErrorKind::classify("--frame [title='a: selector not found: #x"),
            ErrorKind::Other
        );
        // Nested functional selectors keep their inner `: ` text.
        assert_eq!(
            ErrorKind::classify("--frame :is(:not([title='a: b'])): no element matches"),
            ErrorKind::FrameNotFound
        );
        // Unknown "<op> failed: " prefixes are not stripped.
        assert_eq!(
            ErrorKind::classify("upload failed: timed out"),
            ErrorKind::Other
        );
    }
}
