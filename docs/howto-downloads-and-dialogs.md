# How to Handle Downloads and JavaScript Dialogs

This guide covers managing file downloads and handling JavaScript dialogs (`alert`, `confirm`, `prompt`, `beforeunload`) using either the `camoufox` CLI daemon or the Rust API.

---

## Part 1: Managing File Downloads

Camoufox isolates file downloads to prevent untrusted websites from writing files arbitrarily to the host filesystem.

### 1. Configure a Downloads Directory at Launch

By default, if no downloads directory is specified at launch, Camoufox cancels all incoming downloads automatically. To enable file saving, pass `--downloads-dir` when launching an instance:

```bash
# Start the daemon
camoufox serve &

# Launch an instance with download directory configured
camoufox launch --downloads-dir /tmp/my-downloads
```

When a download starts:
1. The browser saves the file inside `/tmp/my-downloads/<uuid>`, where `<uuid>` is a browser-generated unique identifier.
2. The browser never derives local file paths directly from the server-provided `suggestedFileName`, eliminating path traversal vulnerabilities.
3. Every download event is recorded in the instance's context-scoped download log.

### 2. The Download Lifecycle Pattern

Browser downloads often trigger asynchronously after a button click:

```bash
# Step 1: Click the download trigger element
camoufox click 00000001 p1 "#export-csv-btn"

# Step 2: Wait for download lifecycle events
camoufox downloads 00000001 --wait 15
```

The `downloads` command checks the instance's context-scoped download log. When `--wait <secs>` is passed, it waits on a condition variable outside the per-instance lock until the *next* download sequence event arrives (or timeout expires).

*Important lifecycle notes:*
- `--wait` wakes on the *next* event advance. If a download transitions from `started` to `completed`, `--wait` may return on `started` (where `path` is still null). Automation scripts should poll `downloads` until `state` reaches `completed`, `failed`, or `canceled`.
- If the download finished before `--wait` was invoked, `--wait` will wait for a new event until the timeout expires.
- In addition, `click` responses automatically include a `"downloads": [...]` field if a download was initiated during the click execution.

Sample output format (one line per download, or `no downloads` if empty):
```text
completed  p1  https://example.com/reports/annual.csv  -> /tmp/my-downloads/8a2f1b74-3c6d-4e9f-9a1b-2c3d4e5f6a7b
```

To output structured JSON for programmatic scripts:
```bash
camoufox downloads 00000001 --wait 15 --json
```

### 3. Direct Navigation to Download Attachments

When you instruct a page to navigate directly to an attachment URL (or a link with `Content-Disposition: attachment`), the browser diverts the navigation into a download rather than rendering a new DOM.

In Camoufox:
- The `navigate` command exits with an error indicating that the navigation became a download.
- The existing DOM and URL of the page are preserved intact (the page is not blanked or broken).
- The file is saved to your configured `--downloads-dir`.

In the Rust library, this is represented by `ProtocolErrorKind::NavigationBecameDownload`:

```rust,no_run
# fn doc_test(main_frame: &camoufox::api::main_frame::MainFrame) -> Result<(), Box<dyn std::error::Error>> {
use camoufox::api::main_frame::NavigateOptions;
use camoufox::protocol::errors::ProtocolErrorKind;
use std::time::Duration;

let options = NavigateOptions::default();
match main_frame.navigate("https://example.com/export.zip", options, Duration::from_secs(30)) {
    Ok(outcome) => println!("Navigated with status {:?}", outcome.status_code),
    Err(err) if err.kind == ProtocolErrorKind::NavigationBecameDownload => {
        if let Some(info) = &err.download_info {
            println!("Download started for URL: {}", info.url);
            println!("Download UUID: {:?}", info.download_id);
        }
    }
    Err(e) => eprintln!("Navigation error: {e}"),
}
# Ok(())
# }
```

---

## Part 2: Handling JavaScript Dialogs

When a web page triggers `window.alert()`, `window.confirm()`, `window.prompt()`, or a `beforeunload` dialog, Firefox suspends JavaScript execution and input event processing on that tab until the dialog is dismissed or accepted.

### 1. Launch-Time Dialog Policies

When launching an instance via CLI, you can choose a global dialog policy using `--dialogs <policy>`:

```bash
# Auto-accept all alerts, confirms, and prompts
camoufox launch --dialogs accept

# Auto-dismiss all dialogs (alerts are closed, confirms/prompts are cancelled)
camoufox launch --dialogs dismiss

# Hold dialogs open for manual interaction (default)
camoufox launch --dialogs manual
```

| Policy | `alert()` | `confirm()` | `prompt()` | `beforeunload` |
|---|---|---|---|---|
| `accept` | Accepts | Accepts (true) | Accepts (default value, empty if none) | Accepts |
| `dismiss` | Dismisses (closes) | Dismisses (false) | Dismisses (null) | Dismisses |
| `manual` | Suspends page | Suspends page | Suspends page | Suspends page |

### 2. Manual Resolution via CLI

Under `--dialogs manual`, most commands issued while a dialog is open return immediately with an actionable error message indicating a dialog is open.

*(Exceptions: `tabs` and `close-tab` do not check for dialogs; `tabs --json` exposes the open dialog in its tab object via `"dialog": { ... }`. Additionally, `screenshot --clip` bypasses dialog checks.)*

To inspect and resolve the dialog:

```bash
# Accept an alert or confirm dialog
camoufox dialog 00000001 p1 accept

# Accept a prompt dialog with custom input text
camoufox dialog 00000001 p1 accept --text "Alice"

# Dismiss (cancel) a confirm or prompt dialog
camoufox dialog 00000001 p1 dismiss
```

Once resolved, execution on the page resumes immediately.

### 3. Programmatic Dialog Handling in Rust

In Rust, you can control dialog policies per page via `MainFrame`:

```rust,no_run
# fn doc_test(main_frame: &camoufox::api::main_frame::MainFrame) -> Result<(), Box<dyn std::error::Error>> {
use camoufox::api::dialog::DialogPolicy;

// Switch to manual policy to intercept prompts
main_frame.set_dialog_policy(DialogPolicy::Manual);

// Check if a dialog is currently open
if let Some(dialog) = main_frame.pending_dialog() {
    println!("Dialog kind: {}", dialog.kind);
    println!("Dialog message: {}", dialog.message);
    println!("Default value: {:?}", dialog.default_value);

    // Accept with response text
    main_frame.handle_dialog(&dialog.id, true, Some("User Response"));
}
# Ok(())
# }
```

If a command is in-flight when a dialog opens, the in-flight request fails with `ProtocolErrorKind::DialogOpened`, freeing the caller to resolve the dialog promptly rather than timing out.
