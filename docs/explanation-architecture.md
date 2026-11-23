# Architecture and Design Principles

This document explains the architectural foundations and security model of `camoufox-rs`.

---

## 1. Pipe Transport vs. WebSockets

Most browser automation frameworks (CDP/Puppeteer/Selenium) connect to Chromium or Firefox over local TCP/WebSocket ports (e.g., `ws://127.0.0.1:9222`).

`camoufox-rs` uses Unix pipes (file descriptors 3 and 4) instead:

### Security Advantages
1. **No Local Port Exposure**: TCP ports on `127.0.0.1` are visible to any unprivileged process running on the same machine. An attacker running on the local system (or a malicious script via DNS rebinding) can discover and hijack the debug socket.
2. **Kernel-Enforced Isolation**: Unix pipes are anonymous kernel buffers inherited only across the fork/exec boundary. No other process on the host can tap or inject into the communication channel.

### Performance & Reliability
- Zero TCP/IP stack overhead and zero loopback packet filtering delays.
- Elimination of port collision issues in high-concurrency container environments.
- Clean process lifecycle: when the parent process exits or closes the pipe, the browser detects EOF on file descriptor 3 and exits cleanly.

```text
[ Parent / Daemon Process ]
   │                 ▲
   │ (write to fd 3) │ (read from fd 4)
   ▼                 │
[ Camoufox Child Process ]
```

---

## 2. Daemon Security and IPC Model

The `camoufox serve` daemon manages multiple browser instances through a Unix domain socket at `$XDG_RUNTIME_DIR/camoufox/daemon.sock` (or `/tmp/camoufox-<uid>/daemon.sock`).
`--socket` overrides `CAMOUFOX_SOCKET`, which overrides those defaults. External
subcommands receive the resolved absolute path in `CAMOUFOX_SOCKET`.

### Unix Domain Socket Permissions
1. **Runtime Directory Policy**: For the default socket path, the daemon verifies that the runtime directory is owned by the current UID with no group or other access permissions (`mode & 0o077 == 0`), rejecting symlinks. Custom `--socket <path>` overrides skip this directory check and rely on existing parent directory permissions.
2. **0600 Socket File Mode**: The Unix socket file itself is created with mode `0600` (owner read/write only), ensuring local unprivileged processes cannot connect.
3. **Peer Credential Verification (Linux only)**: On Linux, each incoming socket connection is checked via `SO_PEERCRED`; if the connecting client's UID does not match the daemon's UID, the connection is rejected. On non-Linux platforms (e.g. BSD/macOS), peer credential checking is not performed, and access control rests on the file system permissions described above.

### Requests and extensions

Most commands serialize access to an instance; download waits and cassette
validation, disk I/O, and draining run outside that lock. Cassette operations
use a per-page network-operation lock so unrelated page commands can continue.
JSON failures include a machine-readable `kind` alongside the unchanged `error`
message; success omits it. Execution failures exit 1, client argument validation
exits 2 with `invalid_argument`, and local path-resolution failures exit 1 with
`io`. Clap parsing errors exit 2 with text diagnostics.

Unknown command names dispatch to `camoufox-<name>` in absolute `PATH` entries.
Built-ins take precedence. Arguments, standard streams, and exit status pass
through; global `--json` sets `CAMOUFOX_JSON=1`, otherwise that variable is cleared.
Global flags must precede the external name.

---

## 3. The Juggler Protocol

Juggler is the Firefox automation protocol from Playwright, which Camoufox ships.

### Wire Format
Messages are formatted as NUL-delimited (`\0`) JSON strings. The Rust client enforces a maximum frame size of 256 MiB (`DEFAULT_MAX_FRAME_SIZE`) in `src/codec/nul_json.rs` to guard against unbounded memory allocation.

### Session Multiplexing
- **Root Session**: The base pipe connection operates on the browser root level (commands like `Browser.createBrowserContext`, `Browser.close`). Root session messages do not carry a `sessionId` field on the wire.
- **Target Sessions**: Each open tab/page is assigned a unique `sessionId` by Juggler. Messages addressed to pages include `"sessionId": "<id>"`.
- The `Connection` struct multiplexes outgoing commands synchronously from the caller's thread under a writer `Mutex`. A single background reader thread reads frames from the transport and demultiplexes incoming events to registered callbacks via an internal `EventRouter`.

---

## 4. Execution Contexts: Isolated World vs. Main World

When executing JavaScript on a page, Camoufox supports two execution environments:

### Isolated World (Default)
Juggler provisions an isolated JavaScript compartment for automation scripts:
- It shares the underlying DOM tree with the web page.
- Its JavaScript runtime globals (`window`, `Object.prototype`, functions) are completely isolated from page scripts.
- Page scripts cannot detect automation helper functions or tamper with prototype methods to evade detection.

### Main World and CSP Bypass
When an automation script needs to interact directly with variables or libraries declared in the page's own scripts (e.g. inspecting a frontend framework store):
- `--main-world` evaluates directly in the page's window compartment.
- On websites with strict Content Security Policy (`script-src 'self'`), browser engines block dynamically evaluated script strings.
- Camoufox solves this via `--bypass-csp` (or `ContextOptions::bypass_csp`), which instructs the browser to disable CSP enforcement for that context.

---

## 5. Frame Coordinate Transformation and Input Dispatch

When clicking, hovering, or typing inside nested iframes or piercing through open shadow roots:

### 3-Layer Transformation
1. **Selector Resolution**: The target element is located using CSS tokenization and `>>>` open shadow root traversal.
2. **Geometry Measurement**: The browser measures the bounding client rect of the target element and each ancestor iframe.
3. **Axis-Aligned Transformation**:
   - Outermost-to-innermost scrolling ensures ancestor iframes and target elements are positioned in view.
   - Coordinate offsets accumulate iframe content padding, border offsets, and positive CSS scaling factors (`scaleX`, `scaleY`).
   - The final computed coordinates are dispatched as trusted native input events.

### Focus Rules and Statelessness
Earlier implementations attempted to retain global focus memory across iframe boundaries. This created brittle edge cases where main-world blurs or tick delays resurrected stale focus.

The current architecture is strictly stateless:
- Scoped `type` and `press` verify that an active element is currently focused inside the target frame window.
- If focus was lost to the top-level page, the command rejects with an actionable error prompting the caller to click or fill an element inside the frame first.
- No monkey-patched focus listeners or randomized global variables are injected into pages.

---

## 6. PageRegistry and Popup Adoption

Websites frequently open new tabs via `<a target="_blank">` or JavaScript `window.open()`.

`PageRegistry` automatically tracks these targets:
- Upon target creation, the registry checks `openerId`. If present, the target is adopted immediately as a popup.
- During in-flight `BrowserContext::new_main_frame` calls (protocol `Browser.newPage`), newly attaching targets without openers are tracked in a reservation window to eliminate race conditions between explicit user tabs and site popups.
- Per-session event listeners are registered dynamically and torn down when the target detaches, preventing memory leaks across long-lived browser sessions.

## 7. Persistent Profiles and File Inputs

`launch --profile-dir` uses the browser's default context so cookies, localStorage,
IndexedDB, and other on-disk data survive relaunch. It prepares an absolute empty
directory or existing profile, removes group/other access, and reserves the
canonical path until browser exit. Stop/shutdown flushes storage and retains the
directory. Without this flag, the CLI uses a temporary profile and private context.

The library exposes `Browser::default_context`; page events require
`BrowserOptions::attach_to_default_context: true`. Its context ID is empty and
optional `browserContextId` fields are omitted on the wire. Closing this context
only drops the handle; closing the browser flushes storage and closes its pages.

`upload` resolves file paths against the client's working directory and validates
regular files before accessing the page. CSS/frame/shadow resolution obtains a
file-input object handle, then `MainFrame::set_file_input_files_with_timeout`
sends `Page.setFileInputFiles`. This supports hidden inputs without geometry or
focus and dispatches `input` and `change`. The non-timeout API uses 60 seconds.

## 8. Network Cassettes

Recording passively observes page HTTP(S) events, including subframes, and stores
decoded response bodies by SHA-256. Stop atomically publishes a format-v1 index;
write failures prevent publication. Replay validates the index and body hashes
before enabling interception, matches method and exact URL (optionally request-body
hash), and aborts misses. It disables page caching while active and drains accepted
requests before restoring networking and caching at stop. Redirect cassettes are
rejected because redirected requests bypass interception. Popups require separate
sessions, and service-worker responses are outside replay interception.

Tab cleanup finalizes cassettes with a 15-second budget after acquiring the page's
network-operation lock, preserving other tabs' connections. Instance stop/shutdown
share 15 seconds across pages and may disconnect the browser at expiry.
`close-tab` and `stop` report failures as `cassette_errors` in success data and warn
on stderr without changing exit code 0; popup cleanup and shutdown log failures.
Explicit cassette stop commands allow 300 seconds to drain.

Cassettes omit request headers/raw bodies and scrub credential response headers,
but URLs and decoded bodies remain verbatim and may contain secrets. Replay strips
persistent-state headers; served scripts still run with the origin's real storage.
See the [CLI reference](reference-cli.md#record) for format and scope details.
