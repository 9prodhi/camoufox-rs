# CLI Reference: camoufox

Command-line interface reference for the `camoufox` executable.

The `camoufox` CLI communicates with a background daemon over a Unix domain socket using newline-delimited JSON messages.

---

## Global Options

The following flags are available across all subcommands:

- `--socket <SOCKET>`: Path to the daemon Unix socket.
  - If omitted, defaults to `$XDG_RUNTIME_DIR/camoufox/daemon.sock` when `XDG_RUNTIME_DIR` is set.
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
  "error": "error description"
}
```

and the process exits with code `1`. Client-side argument validation errors exit with code `2`.

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

Text mode prints one line per instance formatted as `<instance_id>  pid=<pid>  version=<version>  pages=[<pages>]` (or `no running instances` if empty).

JSON output shape:
```json
{
  "instances": [
    {
      "instance_id": "00000001",
      "pid": 3511172,
      "version": "Firefox/158.0.1-beta.41",
      "pages": []
    }
  ]
}
```

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
