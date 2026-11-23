# CLI Reference: camoufox

Command-line interface reference for the `camoufox` executable.

The `camoufox` CLI communicates with a background daemon over a Unix domain socket using newline-delimited JSON messages.

---

## Global Options

The following flags are available across all subcommands:

- `--socket <SOCKET>`: Path to the daemon Unix socket (defaults to `CAMOUFOX_SOCKET` when set).
  - If neither the flag nor `CAMOUFOX_SOCKET` is set, defaults to `$XDG_RUNTIME_DIR/camoufox/daemon.sock` when `XDG_RUNTIME_DIR` is set.
  - Otherwise, defaults to `/tmp/camoufox-<uid>/daemon.sock`.
  - For the default path, the runtime directory must be a real directory owned by the current UID with no group or other permissions (`mode & 0o077 == 0`). Symlinks and non-owner access are rejected.
  - If an explicit `--socket <SOCKET>` path is passed, this directory policy is skipped and only an existing parent directory is required.
- `--json`: Output response as JSON instead of formatted text.
- `-h, --help`: Print help.

### JSON Response Envelope

When `--json` is supplied, every command outputs a top-level `DaemonResponse` JSON envelope:

```json
{
  "ok": true,
  "data": { ... }
}
```

On daemon or execution errors, the envelope returns:

```json
{
  "ok": false,
  "error": "error description",
  "kind": "other"
}
```

and the process exits with code `1`. Client-side argument validation errors exit with code `2`.

Every daemon error includes a stable, machine-readable `kind` after `error`,
with no `data` field. Branch on `kind` instead of parsing the human-readable
`error` message. Messages are preserved verbatim; plain mode still prints
`error: <message>` on stderr. Successful responses omit `kind`. Clients can
also read responses from older daemons that omit `kind`; the CLI classifies their
error messages to backfill it. Clients must treat unknown kinds as `other`.

The Rust types `ErrorKind`, `DaemonRequest`, `DaemonResponse`, and `LaunchOptions`
are `#[non_exhaustive]`. Include a wildcard when matching enums, create responses
with their constructors, and build launch options with `Default` plus field
assignment.

| Kind | Meaning |
|---|---|
| `selector_not_found` | No element matches the requested selector. |
| `timeout` | An operation exceeded its deadline. |
| `dialog_open` | A JavaScript dialog is blocking the operation. |
| `frame_detached` | A scoped frame was detached. |
| `frame_not_found` | No element matches a frame selector. |
| `not_iframe` | A frame selector matched an element other than an iframe. |
| `shadow_root` | A selector cannot access an open shadow root. |
| `navigation` | Navigation failed or was diverted into a download. |
| `instance_not_found` | The requested browser instance does not exist. |
| `page_not_found` | The requested page does not exist. |
| `page_closed` | The requested page was closed or crashed. |
| `invalid_argument` | An argument is invalid. |
| `io` | A filesystem or I/O operation failed. |
| `daemon` | The daemon rejected the request or is shutting down. |
| `other` | The error does not match a known category. |

Classification recognizes established message prefixes and frame/operation
wrappers. Specific causes take precedence: a navigation timeout has kind
`timeout`. Unrecognized errors, including arbitrary page script exceptions,
use `other`.

Commands that return a void result (such as `stop` and `shutdown`) return `{"ok": true}` with no `data` field. For commands that produce output data, the "JSON output shape" blocks documented below represent the inner payload of this `data` field.

---

## Subcommands Index

| Subcommand | Description |
|---|---|
| [`serve`](#serve) | Start the daemon process |
| [`launch`](#launch) | Launch a new browser instance |
| [`dialog`](#dialog) | Accept or dismiss an open JavaScript dialog |
| [`list`](#list) | List all running browser instances |
| [`stop`](#stop) | Stop a browser instance |
| [`new-page`](#new-page) | Create a new page in a browser instance |
| [`navigate`](#navigate) | Navigate a page to a URL |
| [`evaluate`](#evaluate) | Evaluate JavaScript on a page |
| [`click`](#click) | Click an element by CSS selector, or at viewport coordinates (x, y) |
| [`screenshot`](#screenshot) | Take a screenshot of a page |
| [`shutdown`](#shutdown) | Shut down the daemon and all browser instances |
| [`ping`](#ping) | Ping the daemon |
| [`cookies`](#cookies) | Export all cookies for a browser instance (including HttpOnly) |
| [`text`](#text) | Print the page's visible text (`innerText`), optionally scoped to a selector |
| [`html`](#html) | Print page HTML: `outerHTML` of a selector, or the whole document |
| [`links`](#links) | List every `<a href>` on the page as `text → href` |
| [`data`](#data) | Extract structured page metadata (Open Graph / JSON-LD / meta tags) |
| [`url`](#url) | Print the page's current URL and title |
| [`back`](#back) | Go back one entry in the page's session history |
| [`forward`](#forward) | Go forward one entry in the page's session history |
| [`reload`](#reload) | Reload the page |
| [`wait`](#wait) | Poll until an element matching a CSS selector exists |
| [`cookie`](#cookie) | Set a cookie for the instance's browser context: `name=value` |
| [`header`](#header) | Set an extra HTTP request header for a page: `Name: value` |
| [`fill`](#fill) | Focus the element matching a selector, clear it, and type a value into it |
| [`upload`](#upload) | Set files on a file input, including hidden inputs |
| [`type`](#type) | Type text into whatever element currently has focus |
| [`press`](#press) | Press a named key (Enter, Tab, Escape, ArrowDown, a, 1, …) |
| [`hover`](#hover) | Move the mouse over the element matching a selector (no click) |
| [`select`](#select) | Choose an option in a `<select>` by value, label, or visible text |
| [`scroll`](#scroll) | Scroll an element into view, or scroll to the bottom of the page |
| [`tabs`](#tabs) | List the open pages (tabs) of an instance with their URLs and titles |
| [`close-tab`](#close-tab) | Close a page (tab) |
| [`downloads`](#downloads) | View or wait for downloads on an instance |
| [`record`](#record) | Record a page's HTTP exchanges into a cassette |
| [`replay`](#replay) | Serve a page's requests from a cassette offline |
| [External subcommands](#external-subcommands) | Run `camoufox-<name>` executables from `PATH` |

---

## Subcommand Specifications

### `serve`

Start the daemon process.

```text
Usage: camoufox serve [OPTIONS]
```

- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON instead of human-readable text.

### `launch`

Launch a new browser instance managed by the daemon. Returns a generated instance ID (e.g. `00000001`).

```text
Usage: camoufox launch [OPTIONS]
```

- `--headed`: Run in headed mode (show browser window). Default is headless.
- `--executable <EXECUTABLE>`: Path to the Camoufox executable. If omitted, resolves via `CAMOUFOX_BIN`, then the active or newest install under `~/.cache/camoufox/browsers/` (from `camoufox fetch`), then `~/.cache/camoufox/camoufox`.
- `--downloads-dir <DOWNLOADS_DIR>`: Directory to save downloads to (must be absolute; defaults to cancel).
- `--bypass-csp`: Disable page Content-Security-Policy (needed for `--main-world` on strict-CSP sites; detectable by pages).
- `--dialogs <DIALOGS>`: JavaScript dialog handling policy (`manual`, `accept`, `dismiss`) [default: `manual`].
- `--profile-dir <PROFILE_DIR>`: Absolute path; created or tightened to 0700, canonicalized, must be empty or an existing profile, never deleted on stop. An existing profile contains `prefs.js` or `times.json`. Tightening clears only group/other read, write, and execute permissions, preserving special permission bits. Persistent profiles use the browser's default context: cookies, localStorage, IndexedDB, and other profile data survive relaunch. Stop or shutdown gracefully flushes storage before returning. A profile is reserved during launch and remains in use until the browser exits; concurrent launches on the same canonical directory are refused. Persistent profiles are never deleted on stop or shutdown. Without this flag, launches use a fresh temporary profile and private context.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "instance_id": "00000001",
  "version": "Firefox/158.0.1-beta.41",
  "pid": 12345
}
```

For a persistent launch, the success data also contains `"profile_dir"` with the
canonical directory path. If an older daemon omits that confirmation after
`--profile-dir`, the client exits 1 with kind `daemon` and error
`daemon does not support --profile-dir; restart it: camoufox shutdown && camoufox serve`.
Restart the daemon before retrying the persistent launch.

### `dialog`

Accept or dismiss an open JavaScript dialog.

```text
Usage: camoufox dialog [OPTIONS] <INSTANCE_ID> <PAGE_ID> <ACTION>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID (e.g. `p1`).
- `<ACTION>`: Action to take on the dialog (`accept` or `dismiss`).
- `--text <TEXT>`: Response text to submit when accepting a prompt dialog.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "dialog_action": "accept",
  "dialog": {
    "id": "1",
    "kind": "alert",
    "message": "Hello world",
    "default_value": ""
  }
}
```

### `list`

List all running browser instances.

```text
Usage: camoufox list [OPTIONS]
```

- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

Text mode prints one line per instance formatted as `<instance_id>  pid=<pid>  version=<version>  pages=[<pages>]` (appended with `profile_dir=<profile_dir>` when persistent, or `no running instances` if empty).

JSON output shape:
```json
{
  "instances": [
    {
      "instance_id": "00000001",
      "pid": 3511172,
      "version": "Firefox/158.0.1-beta.41",
      "pages": [],
      "profile_dir": "/path/to/profile"
    }
  ]
}
```
Note: `profile_dir` is present only when the instance was launched with a persistent profile directory (`--profile-dir`).

### `stop`

Stop a browser instance.

```text
Usage: camoufox stop [OPTIONS] <INSTANCE_ID>
```

- `<INSTANCE_ID>`: Instance ID (e.g., `00000001`).
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

Text mode prints `ok`. On success, `--json` returns `{"ok": true}` with no inner data payload.

### `new-page`

Create a new page in a browser instance.

```text
Usage: camoufox new-page [OPTIONS] <INSTANCE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "page_id": "p1"
}
```

### `navigate`

Navigate a page to a URL.

```text
Usage: camoufox navigate [OPTIONS] <INSTANCE_ID> <PAGE_ID> <URL>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID (e.g., `p1`).
- `<URL>`: URL to navigate to.
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--wait-until <WAIT_UNTIL>`: If set, block until the named lifecycle event fires after navigation. Supported values: `load`, `domcontentloaded`.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "navigation_id": "1",
  "status_code": 200
}
```

### `evaluate`

Evaluate JavaScript on a page.

```text
Usage: camoufox evaluate [OPTIONS] <INSTANCE_ID> <PAGE_ID> <EXPRESSION>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `<EXPRESSION>`: JavaScript expression to evaluate.
- `--main-world`: Run in the page's main world so page-defined globals are visible (sync expressions only; detectable by the page; blocked by strict CSP).
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "result": "Example Domain"
}
```

### `click`

Click an element by CSS selector, or at viewport coordinates (x, y).

```text
Usage: camoufox click [OPTIONS] <INSTANCE_ID> <PAGE_ID> <TARGET> [Y]
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID (e.g., `p1`).
- `<TARGET>`: CSS selector, or the integer X coordinate (`i32`) when an integer Y coordinate follows. Supports ` >>> ` shadow piercing.
- `[Y]`: Integer Y coordinate (`i32`, viewport pixels). When present, `<TARGET>` is parsed as the integer X coordinate.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes). Ignored in coordinate click mode.
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`]. Ignored in coordinate click mode (which uses a fixed 60s deadline).
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "clicked": true,
  "selector": "button#submit",
  "x": 100,
  "y": 200
}
```

*Notes:*
- In coordinate click mode (`camoufox click <id> <page> <x> <y>`), the `"selector"` key is omitted.
- If a JavaScript dialog was opened as a consequence of the click, a `"dialog": {...}` key is appended.
- If file downloads were initiated during the click, a `"downloads": [...]` list is included.

### `screenshot`

Take a screenshot of a page.

```text
Usage: camoufox screenshot [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `-o, --output <OUTPUT>`: Output file path. If omitted, defaults to `screenshot-<instance_id>-<page_id>.<ext>` inside the daemon runtime directory (e.g. `$XDG_RUNTIME_DIR/camoufox/screenshot-00000001-p1.png`).
- `--format <FORMAT>`: Image format: `png` or `jpeg` [default: `png`].
- `--quality <QUALITY>`: JPEG quality (`0-100`).
- `--selector <SELECTOR>`: Crop to the element matching this CSS selector.
- `--clip <CLIP>`: Clip region as `x,y,width,height` in CSS pixels. Mutually exclusive with `--selector`.
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "bytes": 54321,
  "path": "/run/user/1000/camoufox/screenshot-00000001-p1.png",
  "clip": {
    "x": 0.0,
    "y": 0.0,
    "width": 1280.0,
    "height": 720.0
  }
}
```

### `shutdown`

Shut down the daemon and all browser instances in parallel.

```text
Usage: camoufox shutdown [OPTIONS]
```

- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

Text mode prints `ok`. On success, `--json` returns `{"ok": true}` with no inner data payload. The socket file is removed; the `<socket>.lock` file is intentionally kept to prevent daemon startup race conditions.

### `ping`

Ping the daemon.

```text
Usage: camoufox ping [OPTIONS]
```

- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "instance_count": 1
}
```

### `cookies`

Export all cookies for a browser instance (including HttpOnly).

```text
Usage: camoufox cookies [OPTIONS] <INSTANCE_ID>
```

- `<INSTANCE_ID>`: Instance ID (e.g., `00000001`).
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "cookies": [
    {
      "name": "session",
      "value": "abc",
      "domain": "example.com",
      "path": "/",
      "httpOnly": true,
      "secure": true
    }
  ]
}
```

### `text`

Print the page's visible text (`innerText`), optionally scoped to a selector.

```text
Usage: camoufox text [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--selector <SELECTOR>`: Restrict extraction to the first element matching this CSS selector.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "text": "Hello world"
}
```

### `html`

Print page HTML: `outerHTML` of a selector, or the whole document.

```text
Usage: camoufox html [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--selector <SELECTOR>`: Return the `outerHTML` of the first element matching this CSS selector.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "html": "<html><body><h1>Title</h1></body></html>"
}
```

### `links`

List every `<a href>` on the page as `text → href`.

```text
Usage: camoufox links [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--selector <SELECTOR>`: Only collect links inside the element matching this CSS selector.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "links": [
    {
      "text": "Documentation",
      "href": "https://example.com/docs"
    }
  ]
}
```

### `data`

Extract structured page metadata (Open Graph / JSON-LD / meta tags). With no flags, all three groups are returned.

```text
Usage: camoufox data [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--og`: Include Open Graph (`og:` / `twitter:`) tags.
- `--jsonld`: Include `application/ld+json` blocks.
- `--meta`: Include named `<meta>` tags.
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "title": "Test Page",
  "url": "https://example.com/",
  "og": {
    "og:title": "OG Title"
  },
  "meta": {
    "description": "Page description"
  },
  "jsonld": []
}
```

*Notes:*
- `title` and `url` are always included in the returned object.
- Metadata keys in `og` retain their full prefix. The `--og` flag extracts all Open Graph and social tags matching `/^(og|twitter|article|fb|al):/i` (including `twitter:`, `article:`, `fb:`, and `al:` tags).

### `url`

Print the page's current URL and title.

```text
Usage: camoufox url [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "url": "https://example.com/",
  "title": "Example Domain"
}
```

### `back`

Go back one entry in the page's session history.

```text
Usage: camoufox back [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "navigated": true,
  "direction": "back"
}
```

### `forward`

Go forward one entry in the page's session history.

```text
Usage: camoufox forward [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "navigated": true,
  "direction": "forward"
}
```

### `reload`

Reload the page.

```text
Usage: camoufox reload [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "reloaded": true
}
```

### `wait`

Poll until an element matching a CSS selector exists.

```text
Usage: camoufox wait [OPTIONS] --selector <SELECTOR> <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--selector <SELECTOR>`: CSS selector to wait for (required). Supports ` >>> ` shadow piercing.
- `--timeout <TIMEOUT>`: Give up after this many seconds [default: `10`].
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "found": true,
  "selector": "#content",
  "waited_ms": 150
}
```

### `cookie`

Set a cookie for the instance's browser context: `name=value`.

```text
Usage: camoufox cookie [OPTIONS] <INSTANCE_ID> <PAGE_ID> <PAIR>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID (used to derive cookie URL when neither `--url` nor `--domain` is given).
- `<PAIR>`: Cookie as `name=value`.
- `--url <URL>`: Associate the cookie with this URL instead of the page's current URL.
- `--domain <DOMAIN>`: Cookie domain (mutually exclusive with `--url`).
- `--path <PATH>`: Cookie path.
- `--secure`: Mark the cookie Secure.
- `--http-only`: Mark the cookie HttpOnly.
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "cookie_set": "session_id",
  "domain": null,
  "url": "https://example.com/",
  "value": "xyz123"
}
```

`--domain` and `--url` are mutually exclusive: with `--domain` the response has `"url": null`; with `--url` (or the page-URL default) it has `"domain": null`.

### `header`

Set an extra HTTP request header for a page: `Name: value`. Headers accumulate across calls for the lifetime of the page.

```text
Usage: camoufox header [OPTIONS] <INSTANCE_ID> <PAGE_ID> <PAIR>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `<PAIR>`: Header as `Name: value`.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "header_set": "Authorization",
  "headers": [
    {
      "name": "Authorization",
      "value": "Bearer token"
    }
  ]
}
```

### `fill`

Focus the element matching a selector, clear it, and type a value into it.

```text
Usage: camoufox fill [OPTIONS] <INSTANCE_ID> <PAGE_ID> <SELECTOR> <VALUE>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `<SELECTOR>`: CSS selector of the input, textarea, or contenteditable.
- `<VALUE>`: Value to type.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "filled": true,
  "selector": "input#username",
  "tag": "input",
  "value": "admin"
}
```

### `upload`

Set the files of an `<input type="file">`, including hidden inputs. The browser
dispatches `input` and `change` events after updating the file list.

```text
Usage: camoufox upload [OPTIONS] <INSTANCE_ID> <PAGE_ID> <SELECTOR> <PATH>...
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `<SELECTOR>`: CSS selector of the file input. Supports ` >>> ` to pierce open shadow roots.
- `<PATH>...`: One or more regular files. Relative paths resolve against the client's working directory. Multiple files require the input's `multiple` attribute. Each upload replaces the previous selection.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Overall command timeout in seconds [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

Plain output prints one uploaded basename per line. JSON output shape (inside `data`):

```json
{
  "uploaded": true,
  "selector": "input#file",
  "files": ["fixture.pdf"]
}
```

```bash
camoufox upload <instance_id> <page_id> '#file' ./fixture.pdf
camoufox upload <instance_id> <page_id> '#host >>> input' ./one.txt ./two.txt \
    --frame '#outer' --frame '#inner'
```

Missing selectors report `selector not found: <selector>`. Non-file elements,
missing or non-regular paths, and multiple files on a single-file input report
`invalid argument: ...`. File paths are checked before accessing the page.

### `type`

Type text into whatever element currently has focus.

```text
Usage: camoufox type [OPTIONS] <INSTANCE_ID> <PAGE_ID> <TEXT>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `<TEXT>`: Text to insert.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Overall command timeout in seconds [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "typed": "my-text"
}
```

### `press`

Press a named key (`Enter`, `Tab`, `Escape`, `ArrowDown`, `a`, `1`, ...).

```text
Usage: camoufox press [OPTIONS] <INSTANCE_ID> <PAGE_ID> <KEY>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `<KEY>`: Key name.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Overall command timeout in seconds [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "pressed": "Enter"
}
```

If the key press opens a JavaScript dialog, the response also carries a `"dialog": {...}` object describing it.

### `hover`

Move the mouse over the element matching a selector (no click).

```text
Usage: camoufox hover [OPTIONS] <INSTANCE_ID> <PAGE_ID> <SELECTOR>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `<SELECTOR>`: CSS selector.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "hovered": true,
  "selector": "a.menu",
  "x": 50,
  "y": 120
}
```

### `select`

Choose an option in a `<select>` by value, label, or visible text.

```text
Usage: camoufox select [OPTIONS] <INSTANCE_ID> <PAGE_ID> <SELECTOR> <VALUE>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `<SELECTOR>`: CSS selector of the `<select>` element.
- `<VALUE>`: Option value, label, or text to select.
- `--frame <FRAMES>`: Scope to an iframe matching this selector (repeatable for nested iframes).
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "selected": true,
  "selector": "select#country",
  "option_value": "US",
  "option_text": "United States"
}
```

### `scroll`

Scroll an element into view, or scroll to the bottom of the page.

```text
Usage: camoufox scroll [OPTIONS] <INSTANCE_ID> <PAGE_ID> [SELECTOR]
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `[SELECTOR]`: CSS selector to scroll into view. Omit to scroll to the page bottom.
- `--timeout <TIMEOUT>`: Timeout in seconds for waiting for execution context [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "scrollX": 0,
  "scrollY": 2460.5,
  "scrollHeight": 3142
}
```

### `tabs`

List the open pages (tabs) of an instance with their URLs and titles.

```text
Usage: camoufox tabs [OPTIONS] <INSTANCE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `--timeout <TIMEOUT>`: Per-page timeout in seconds for reading each tab's URL/title [default: `30`].
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "tabs": [
    {
      "opener": null,
      "page_id": "p1",
      "title": "Example Domain",
      "url": "https://example.com/"
    }
  ]
}
```

*Notes:*
- Each tab item contains `page_id`, `url`, `title`, and `opener` (or `null`).
- An item may also contain `"crashed": true` if the page process terminated abnormally, or `"dialog": {...}` if an unhandled JavaScript dialog is currently open.
- When a page has not navigated or is unreadable, `url` and `title` can be `null` or empty strings.

### `close-tab`

Close a page (tab).

```text
Usage: camoufox close-tab [OPTIONS] <INSTANCE_ID> <PAGE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `<PAGE_ID>`: Page ID.
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "closed_page": "p1"
}
```

### `downloads`

View or wait for downloads on an instance.

```text
Usage: camoufox downloads [OPTIONS] <INSTANCE_ID>
```

- `<INSTANCE_ID>`: Instance ID.
- `--wait <WAIT>`: Optional timeout in seconds to wait for download events. Returns on the *next* download event (start or finish) or after the timeout; re-run until the entry is `completed`, `failed`, or `canceled` (see [Downloads and Dialogs](howto-downloads-and-dialogs.md)).
- `--socket <SOCKET>`: Path to the daemon Unix socket.
- `--json`: Output as JSON.

JSON output shape:
```json
{
  "downloads": [
    {
      "uuid": "8a2f1b74-3c6d-4e9f-9a1b-2c3d4e5f6a7b",
      "url": "https://example.com/file.zip",
      "suggested_filename": "file.zip",
      "page_target_id": "target-456",
      "page": "p1",
      "state": "completed",
      "error": null,
      "path": "/tmp/downloads/8a2f1b74-3c6d-4e9f-9a1b-2c3d4e5f6a7b"
    }
  ]
}
```
Each entry in the `downloads` array is a `DownloadEntry` object.

### `record`

Capture HTTP exchanges for a page, including its subframes, scripts, images,
and XHR/fetch requests.

```text
camoufox record start <INSTANCE_ID> <PAGE_ID> --dir <ABS_DIR> [--url <REGEX>]...
camoufox record stop <INSTANCE_ID> <PAGE_ID>
```

`--dir` must be absolute and either absent or empty. Recording refuses every
non-empty directory, including existing cassettes. New cassette directories and
`bodies/` are created with mode 0700; body files and `index.json` use mode 0600.
An existing directory must be a real directory owned by the current user with
no group or other permissions; insecure directories return `invalid argument: ...`.
Repeat `--url` to include a request when its URL matches any regex; omit it to include all HTTP(S) URLs.
Invalid regexes are rejected before creating the cassette directory.

Recording observes page traffic without changing requests or caching.
Failed requests and requests still in flight when recording stops are excluded.
Responses whose bodies cannot be read are omitted from the cassette and reported
in `skipped` with their method, URL, and reason; the remaining index is published.
Camoufox buffers at most 10 MB per response and 100 MB of response bodies in total.
Larger responses are marked evicted, and the oldest buffered bodies are evicted
when the total limit is exceeded. Cassette write errors remain fatal and prevent
index publication. Redirects with a `Location` header are stored with an empty
body because their response bodies are unavailable.

Start returns `{"recording":true,"dir":"/absolute/cassette"}`. Stop atomically
publishes `index.json` using a temporary file and rename, and returns:

```json
{"dir":"/absolute/cassette","exchanges":8,"bodies":6,"skipped":[]}
```

### `replay`

Serve a page's HTTP requests entirely from a cassette. The original server can
be stopped before replay begins.

```text
camoufox replay start <INSTANCE_ID> <PAGE_ID> --dir <ABS_DIR> [--match-body]
camoufox replay stop <INSTANCE_ID> <PAGE_ID>
```

Juggler bypasses interception for requests following redirects. To preserve
offline behavior, replay rejects cassettes containing redirect responses
(301/302/303/307/308 with `Location`) before enabling interception. Recording
these exchanges remains supported.

Replay validates the format, version, metadata, and every referenced body's
SHA-256 and length before enabling interception. Invalid cassettes return
`invalid argument: ...`. An unsupported format or version returns
`invalid argument: unsupported cassette format/version` before validating the
version-specific schema. Body validation streams from disk; bodies are read on
demand during replay. Keep cassette files available and unchanged until replay
stops: a missing or changed body aborts the request and makes replay stop report
an I/O error.

Matching uses the HTTP method and exact URL, including the query string.
`--match-body` also compares the SHA-256 of the request body, distinguishing
no body (`null`) from an empty body. For `multipart/form-data`, every occurrence
of the declared boundary is replaced with a fixed token before hashing, so
identical form submissions match despite randomly generated boundaries. Other
request bodies are hashed as raw bytes. Replay decodes and hashes request bodies
only when `--match-body` is enabled. Older v1 multipart hashes can still match
byte-identical requests. Repeated requests consume responses in
recorded request order; after the final occurrence, that response is reused.
Matched requests receive the recorded status, headers, and decoded body.
`Content-Encoding`, `Transfer-Encoding`, and the original `Content-Length` are
removed; `Content-Length` is set to the decoded byte length. Replay also removes
`Cache-Control`, `Expires`, `Pragma`, `Strict-Transport-Security`, `Alt-Svc`,
`Service-Worker-Allowed`, `Clear-Site-Data`, and `Set-Cookie`, then adds
`Cache-Control: no-store` to avoid persisting response state. Unmatched requests
are aborted and included in the stop result, grouped by method and URL with a
`count`. The list contains at most 1000 distinct entries; `missed_total` counts
all unmatched requests, including those beyond the list limit. Replay disables
the page's cache while interception is active. Stop first resolves requests
accepted before its stop boundary, then disables interception and restores caching; later requests
resume normal network behavior.

Start returns `{"replaying":true,"dir":"/absolute/cassette"}`. Stop returns:

```json
{"served":8,"missed_total":2,"missed":[{"method":"GET","url":"https://example.com/missing","count":2}]}
```

Only one recording or replay may be active per page. Starting another, or
stopping the wrong mode, returns `invalid argument: ...`. A stop on a page that
has already finished that mode returns its last result with `"repeated":true`;
a stop on a page that has never run that mode returns `invalid argument: ...`.
Both stop commands work while a JavaScript dialog is open and allow up to 300
seconds for draining responses. Closing a page, stopping its instance, or shutting
down the daemon finalizes an active recording before disconnecting the browser,
provided cassette writes succeed. Abruptly killing the daemon cannot finalize it.

Record and replay each apply to one page, including that page's subframes.
Popups opened from it are not recorded or replayed. Requests handled by a service
worker are not intercepted, so replay cannot replace their responses.

Cassettes are trusted input. Served HTML and scripts run with the origin's real
storage, including cookies, local storage, and service workers. Suppressing
persistent response headers does not isolate served content from that storage.

Both commands support the global `--socket` and `--json` options. JSON output
wraps the documented payload in the usual `{"ok":true,"data":...}` response.

#### Cassette format (v1)

```json
{
  "format": "camoufox-cassette",
  "version": 1,
  "exchanges": [{
    "seq": 0,
    "method": "GET",
    "url": "https://example.com/data",
    "request_body_sha256": null,
    "status": 200,
    "headers": [["Content-Type", "application/json"]],
    "body_sha256": "<64 lowercase hex characters>",
    "body_len": 42
  }]
}
```

Bodies are decoded bytes, stored once per SHA-256 at `bodies/<sha256-hex>`.
The following response headers are removed case-insensitively during recording:
`Set-Cookie`, `Cookie`, `Authorization`, `Proxy-Authorization`, `Proxy-Authenticate`,
`WWW-Authenticate`, `Authentication-Info`, `X-CSRF-Token`, `X-XSRF-Token`,
`X-Auth-Token`, `X-Api-Key`, and `X-Amz-Security-Token`. Request headers and raw
request bodies are not persisted; only request-body hashes are stored.
URLs, including query strings and `Location` targets, and decoded response bodies
are stored verbatim and may contain secrets. Header scrubbing does not remove
secrets from those URLs or bodies.

### External Subcommands

`camoufox` supports git/cargo-style external subcommands. When an unrecognized command name `<name>` is invoked, `camoufox` searches for an executable binary named `camoufox-<name>` on `PATH`:

```bash
camoufox custom-tool --flag arg
```

If found, `camoufox` executes the binary with the remaining arguments, inheriting standard input/output/error, passing the current environment, and setting:

- `CAMOUFOX_SOCKET`: The absolute daemon socket path (respecting global `--socket` and its environment default). Relative socket paths resolve against the client's working directory; filesystem path bytes are preserved.
- `CAMOUFOX_JSON=1`: Set when the global `--json` flag is given. Place global options before the external subcommand name; arguments after the name pass through to the executable.

The exit code mirrors the child process exit code (or `128 + signal` if terminated by a signal on Unix).

Only absolute `PATH` entries are searched; relative and empty entries are skipped.

Built-in command names always win over external binaries. Subcommand names containing path separators (such as `/`) are rejected as unrecognized subcommands. If no matching executable is found on `PATH`, `camoufox` exits with status 2 and clap's standard unrecognized subcommand error.
