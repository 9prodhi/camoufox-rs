# How to Automate Browsing with the Camoufox CLI Daemon

This guide shows how to manage the Camoufox Unix socket daemon and run browsing, extraction, interaction, and scraping commands from the terminal or shell scripts.

## Prerequisites

1. Build the release CLI binary:
   ```bash
   cargo build --release --features cli --bin camoufox
   ```
2. Verify binary availability:
   ```bash
   ./target/release/camoufox --help
   ```

---

## 1. Start and verify the daemon

The CLI connects to a background daemon over a Unix domain socket. Start the daemon in a dedicated terminal or service manager:

```bash
./target/release/camoufox serve
```

In another terminal, verify the daemon is responding:

```bash
./target/release/camoufox ping
```

Expected output:
```text
pong (0 instances)
```

With `--json`:
```bash
./target/release/camoufox --json ping
```
```json
{
  "ok": true,
  "data": {
    "instance_count": 0
  }
}
```

### Socket resolution

The daemon and client resolve the socket path in this order:

1. `--socket <path>`, when supplied.
2. `CAMOUFOX_SOCKET`, when set.
3. `$XDG_RUNTIME_DIR/camoufox/daemon.sock`, when `XDG_RUNTIME_DIR` is set.
4. `/tmp/camoufox-<uid>/daemon.sock` otherwise.

Override the socket location with `--socket <path>` on any command:
```bash
./target/release/camoufox --socket /tmp/my-daemon.sock serve
./target/release/camoufox --socket /tmp/my-daemon.sock list
```

Alternatively, export `CAMOUFOX_SOCKET=/tmp/my-daemon.sock` for both terminals.
An explicit `--socket` takes precedence over that environment variable.

### Output and errors

With `--json`, successful responses contain `ok: true` and optional `data`;
failures contain `ok: false`, `error`, and a machine-readable `kind` such as
`selector_not_found`, `timeout`, `dialog_open`, `invalid_argument`, `io`, or
`daemon`. Success omits `kind`. The client backfills missing kinds from older
daemon messages and preserves unknown kinds; treat unknown values as `other`.

Exit codes are 0 for success, 1 for execution or daemon failures, and 2 for
client-side argument validation (`invalid_argument`) or clap parsing errors.
Clap diagnostics remain text even with `--json`. Local path-resolution failures
exit 1 with kind `io`; daemon-side validation failures also exit 1.

---

## 2. Launch browser instances and manage tabs

### Launch an instance
```bash
./target/release/camoufox launch
```
Output (stdout prints the instance ID; stderr reports browser version and pid):
```text
00000001
```

To see the browser window, pass `--headed`:
```bash
./target/release/camoufox launch --headed
```

To configure downloads and dialogs at launch:
```bash
./target/release/camoufox launch --downloads-dir /tmp/my-downloads --dialogs accept
```

### Keep browsing data across launches

```bash
./target/release/camoufox launch --profile-dir /absolute/path/to/profile
```

This uses the browser's default context so cookies, localStorage, IndexedDB, and
other profile data survive relaunch. The directory must be empty or contain
`prefs.js` or `times.json`; missing directories are created with mode 0700 and
existing profiles have group/other access removed. The canonical path appears in
launch JSON and `list`, and cannot be used concurrently by another instance in
the same daemon process. Do not point two daemons at the same profile directory:
Firefox does not support concurrent use and it can corrupt the profile.
Stop or shutdown flushes storage and keeps the directory.

If an older daemon cannot confirm profile support, the client stops the returned
instance and exits 1 with kind `daemon` and restart instructions. If cleanup fails,
the error identifies the instance to stop manually. Restart with `camoufox shutdown`,
then `camoufox serve`, before retrying.

### List instances
Listing instances shows all running processes and their open pages (a freshly launched instance has no pages):
```bash
./target/release/camoufox list
```
Output:
```text
00000001  pid=3511172  version=Firefox/158.0.1-beta.41  pages=[]
```

### Open a new page (tab)
Create the first page on the instance:
```bash
./target/release/camoufox new-page 00000001
```
Output:
```text
p1
```

### List tabs and close tabs
List all open tabs with their URLs and titles:
```bash
./target/release/camoufox tabs 00000001
```
Output:
```text
p1  about:blank  
```

Or after navigating `p1`:
```text
p1  https://example.com/  Example Domain
```

Close a tab:
```bash
./target/release/camoufox close-tab 00000001 p1
```

---

## 3. Navigate with lifecycle synchronization

Navigate a page to a URL:

```bash
./target/release/camoufox navigate 00000001 p1 https://example.com
```

### Wait for page events
By default, `navigate` returns after the Juggler `Page.navigate` protocol acknowledgement. To block until page resources are ready, pass `--wait-until`:

```bash
# Block until full window load event fires (bounded by --timeout seconds):
./target/release/camoufox navigate 00000001 p1 https://example.com --wait-until load --timeout 30

# Or block until DOMContentLoaded:
./target/release/camoufox navigate 00000001 p1 https://example.com --wait-until domcontentloaded --timeout 15
```

Supported `--wait-until` values are `load` and `domcontentloaded`.

### Inspect final HTTP status
Navigate never fails on 4xx/5xx HTTP codes. Use `--json` to inspect `status_code` (which captures the final HTTP status after following all redirects):

```bash
./target/release/camoufox --json navigate 00000001 p1 https://example.com
```
```json
{
  "ok": true,
  "data": {
    "navigation_id": "nav-1",
    "status_code": 200
  }
}
```

---

## 4. Extract content and metadata

### Visible text
Extract `innerText` from the entire page or scoped to a CSS selector:

```bash
./target/release/camoufox text 00000001 p1
./target/release/camoufox text 00000001 p1 --selector "article"
```

### HTML source
Extract `outerHTML`:

```bash
./target/release/camoufox html 00000001 p1 --selector "header"
```

### Links
Extract all `<a href>` elements, resolved to absolute URLs:

```bash
./target/release/camoufox links 00000001 p1
```

### Structured metadata
Extract Open Graph, JSON-LD, or meta tags:

```bash
# Extract all metadata:
./target/release/camoufox data 00000001 p1

# Scoped to Open Graph only:
./target/release/camoufox data 00000001 p1 --og

# Scoped to JSON-LD only:
./target/release/camoufox data 00000001 p1 --jsonld
```

### URL and Title
Print current URL on stdout and title on stderr:

```bash
./target/release/camoufox url 00000001 p1
```

---

## 5. Interact with page elements

Commands like `click`, `hover`, `press`, `type`, and the typing phase of `fill` dispatch trusted browser-level input events (`event.isTrusted === true`). In contrast, `select` uses synthetic JavaScript `change`/`input` events, `scroll` is executed via JavaScript `element.scrollIntoView()`, and `fill`'s clear step uses JavaScript (`isTrusted === false`).

### Wait for an element to appear
Poll for a CSS selector with a timeout (default timeout is 10s):

```bash
./target/release/camoufox wait 00000001 p1 --selector "#results" --timeout 15
```

### Click elements
Click an element by CSS selector (scrolls into view and clicks center):

```bash
./target/release/camoufox click 00000001 p1 "#search-button"
```

Or dispatch a trusted click at raw viewport coordinates `(x, y)`:

```bash
./target/release/camoufox click 00000001 p1 350 220
```

Because coordinate clicks originate from the browser process (`event.isTrusted === true`), they can drive CAPTCHA or Turnstile widgets that reject synthetic JavaScript events.

### Form input: Fill, Type, Press
Fill an input field (clears existing value via JavaScript and types new text with trusted events):

```bash
./target/release/camoufox fill 00000001 p1 "input[name=q]" "camoufox"
```

Send a keyboard press (e.g., Enter, Tab, Escape, ArrowDown):

```bash
./target/release/camoufox press 00000001 p1 Enter
```

Type text into whatever element currently has focus via `Page.insertText` (sent as a single batch string in Juggler, not per-key events):

```bash
./target/release/camoufox type 00000001 p1 "search query"
```

### Upload files

```bash
./target/release/camoufox upload 00000001 p1 'input[type=file]' ./fixture.pdf
./target/release/camoufox upload 00000001 p1 'file-picker >>> input' ./one.txt ./two.txt \
    --frame '#outer' --frame '#inner' --timeout 30
```

Upload replaces the selection and dispatches `input` and `change`, including on
hidden inputs. Relative paths resolve against the client's working directory;
all paths must be regular files and are validated before page access. Multiple
files require `multiple`; directory-picker (`webkitdirectory`) inputs are rejected
with kind `invalid_argument`. Plain output lists basenames; JSON returns
`uploaded`, `selector`, and `files`, plus `dialog` if a handler opens one.

### Hover, Select, Scroll
Hover over an element without clicking:
```bash
./target/release/camoufox hover 00000001 p1 ".dropdown-trigger"
```

Select a dropdown option in `<select>` by value, label, or visible text:
```bash
./target/release/camoufox select 00000001 p1 "select#country" "Canada"
```

Scroll an element into view or scroll to the bottom of the page:
```bash
# Scroll specific element:
./target/release/camoufox scroll 00000001 p1 "#footer"

# Scroll to bottom of page:
./target/release/camoufox scroll 00000001 p1
```

---

## 6. JavaScript evaluation

Evaluate expressions in the page:

```bash
./target/release/camoufox evaluate 00000001 p1 "document.title"
```

By default, `evaluate` runs in an **isolated world**. To read page-defined globals (such as `window.__INITIAL_STATE__`), add `--main-world`:

```bash
./target/release/camoufox evaluate 00000001 p1 --main-world "window.__NEXT_DATA__.props"
```

If the site blocks inline scripts with Content-Security-Policy, relaunch the instance with `--bypass-csp`.

---

## 7. Cookies and HTTP headers

### Export cookies
Export the instance cookie jar (including HttpOnly cookies):

```bash
./target/release/camoufox cookies 00000001
./target/release/camoufox --json cookies 00000001
```

### Set a cookie
```bash
./target/release/camoufox cookie 00000001 p1 "auth_token=secret123" --domain example.com --secure --http-only
```

### Add custom HTTP headers
Headers accumulate across calls for the page session:

```bash
./target/release/camoufox header 00000001 p1 "Accept-Language: fr-FR"
./target/release/camoufox header 00000001 p1 "X-Custom-Header: test"
```

---

## 8. Capture screenshots

Capture full viewport:

```bash
./target/release/camoufox screenshot 00000001 p1 -o /tmp/page.png
```

Capture cropped to a CSS selector (element scrolled into view first):

```bash
./target/release/camoufox screenshot 00000001 p1 --selector "#main-card" -o /tmp/card.png
```

Capture an explicit rectangle `x,y,width,height` in CSS pixels:

```bash
./target/release/camoufox screenshot 00000001 p1 --clip 0,0,800,600 -o /tmp/clip.png
```

Supported formats are `png` (default) and `jpeg` (with optional `--quality <0-100>`).

---

## 9. Stop instances and shut down

Stop a single instance:

```bash
./target/release/camoufox stop 00000001
```
`stop` interrupts in-flight commands and finalizes active cassettes with a shared
15-second budget across the instance's pages. It may disconnect the browser at
the deadline. Persistent profile storage is flushed and its directory is kept.
Finalization failures appear in success data as `cassette_errors` entries with
`page_id`, `mode`, and `error`; plain output warns on stderr and still exits 0.

Shut down the daemon and all running instances:

```bash
./target/release/camoufox shutdown
```
`shutdown` stops browsers in parallel, removes the socket file, and exits. The `<socket>.lock` file is intentionally kept to prevent race conditions during daemon restart.
It uses the same cassette finalization budget per instance and logs finalization failures.

---

## 10. Record and replay HTTP traffic

Start recording before navigation and stop to publish the cassette:

```bash
./target/release/camoufox record start 00000001 p1 --dir /absolute/cassette
./target/release/camoufox navigate 00000001 p1 https://example.com --wait-until load
./target/release/camoufox record stop 00000001 p1
./target/release/camoufox replay start 00000001 p1 --dir /absolute/cassette
./target/release/camoufox navigate 00000001 p1 https://example.com --wait-until load
./target/release/camoufox replay stop 00000001 p1
```

Recording requires an absent or empty absolute directory; existing directories
must be owner-only. Repeat `--url <regex>` to include matching URLs. Stop atomically
publishes `index.json` and reports `exchanges`, `bodies`, and `skipped`. Unreadable
or evicted bodies are skipped; disk write errors prevent publication.

Replay matches method and exact URL, optionally request-body hashes with
`--match-body`. Repeated matches consume responses in order, then reuse the last.
Misses are aborted and reported in `missed` with counts and `missed_total` at stop.
Replay rejects redirect cassettes; keep cassette files unchanged until it stops.
One recording or replay may run per page, including subframes. Popups need their
own sessions; service-worker responses are outside replay interception.

Explicit stop commands work with open dialogs, allow 300 seconds to drain, and
return the last result with `repeated: true` if repeated. Closing a tab also
finalizes its cassette, with a 15-second budget after acquiring its network-operation
lock. Other tabs keep their connections. Finalization failures use the same
`cassette_errors` success data as instance stop.

Cassettes scrub credential headers, but store URLs and decoded bodies verbatim,
which may contain secrets. Replayed HTML and scripts use the origin's real storage;
use trusted cassettes. See the [CLI reference](reference-cli.md#record) for format,
buffer limits, and lifecycle details.

## 11. External subcommands

`camoufox <name> <args>...` runs `camoufox-<name>` from absolute `PATH` entries
when `<name>` is not built-in. Arguments, environment, standard streams, and exit
status pass through. Put global options before the name:

```bash
camoufox --socket /tmp/my-daemon.sock --json custom-tool --flag value
```

The executable receives `CAMOUFOX_SOCKET` as an absolute path and
`CAMOUFOX_JSON=1` with global `--json`; otherwise `CAMOUFOX_JSON` is cleared.
Output is controlled by the executable. A missing command uses clap's standard
error and exit code 2. Built-in commands always take precedence.

---

## Related documentation

- [How to handle iframes and shadow DOM](howto-iframes-and-shadow-dom.md)
- [How to manage downloads and JavaScript dialogs](howto-downloads-and-dialogs.md)
- [CLI Reference](reference-cli.md)
- [Architecture & Security Explanation](explanation-architecture.md)
