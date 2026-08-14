# Browse the Web with Camoufox

Camoufox is a hardened, fingerprint-resistant Firefox fork driven by the
`camoufox` CLI. **Use it instead of `WebSearch` / `WebFetch` for all web
browsing** — it runs a real browser, so it renders JavaScript, keeps cookies,
and can fill forms and click things.

## Setup (do this first)

Point `BROWSE` at your `camoufox` binary. If you installed it (`cargo install
--path . --features cli`) it's on `PATH` as `camoufox`; if you built from
source it's `target/release/camoufox` in the repo. Override with
`$CAMOUFOX_BIN` if it lives elsewhere:

```bash
# Prefer $CAMOUFOX_BIN, then `camoufox` on PATH, then a local release build.
BROWSE="${CAMOUFOX_BIN:-camoufox}"
command -v "$BROWSE" >/dev/null 2>&1 || BROWSE="./target/release/camoufox"
# Build a release binary if none is available (run from the repo root):
[ -x "$BROWSE" ] || command -v "$BROWSE" >/dev/null 2>&1 \
  || cargo build --release --bin camoufox --features cli
```

Use `--release`, not `--debug` — this tool is meant to be reused across
sessions, not rebuilt each time.

## Usage: `/browse <url>`

```bash
BROWSE="${CAMOUFOX_BIN:-camoufox}"
command -v "$BROWSE" >/dev/null 2>&1 || BROWSE="./target/release/camoufox"

# 1. Ensure the daemon is running
$BROWSE ping --json >/dev/null 2>&1 || ($BROWSE serve --foreground >/tmp/camoufox-daemon.log 2>&1 &)
sleep 1

# 2. Reuse an existing browser instance, or launch one
INSTANCE=$($BROWSE list --json | jq -r '.data.instances[0].instance_id // empty')
[ -n "$INSTANCE" ] || INSTANCE=$($BROWSE launch --json | jq -r '.data.instance_id')

# 3. Create a page
PAGE=$($BROWSE new-page "$INSTANCE" --json | jq -r '.data.page_id')

# 4. Navigate, waiting for the load event, and check the HTTP status
$BROWSE navigate "$INSTANCE" "$PAGE" '<url>' --wait-until load --json

# 5. Read the page — clean text, plus structured metadata
$BROWSE text "$INSTANCE" "$PAGE"
$BROWSE data "$INSTANCE" "$PAGE" --og

# 6. (Optional) screenshot the whole viewport or just one element
$BROWSE screenshot "$INSTANCE" "$PAGE" --selector 'article' -o /tmp/shot.png
```

Every command below takes `<instance_id> <page_id>` first (except `launch`,
`list`, `cookies`, `tabs`, `ping`, `shutdown`, which are instance- or
daemon-scoped). Add `--json` to any command for machine-readable output.

## Command reference

### Session

| Command | Description |
|---------|-------------|
| `ping` | Check whether the daemon is up |
| `serve --foreground &` | Start the daemon |
| `launch [--headed]` | Launch a browser, prints `instance_id` |
| `list` | List running instances |
| `new-page <inst>` | Create a page, prints `page_id` |
| `tabs <inst>` | List open pages as `page_id  url  title` |
| `close-tab <inst> <page>` | Close one page |
| `stop <inst>` | Stop one browser instance |
| `shutdown` | Stop the daemon and every instance |

### Reading

| Command | Description |
|---------|-------------|
| `text <inst> <page> [--selector <css>]` | Rendered page text (`innerText`), or just that element. Empty output means the element renders nothing — try a more specific selector, or `html` |
| `html <inst> <page> [--selector <css>]` | `outerHTML` of the selector, or the whole document |
| `links <inst> <page> [--selector <css>]` | Every `<a href>` as `text → href` (absolute URLs) |
| `data <inst> <page> [--og] [--jsonld] [--meta]` | Structured metadata. No flags = all three groups |
| `url <inst> <page>` | Current URL (stdout) and title (stderr) |
| `evaluate <inst> <page> '<js>'` | Run arbitrary JavaScript, prints the result |

`data` groups: `--og` covers `og:` / `twitter:` / `article:` / `fb:` / `al:`
properties, `--jsonld` parses every `application/ld+json` block into real JSON,
`--meta` collects named `<meta>` tags. Every response also carries `title` and
`url` for context.

### Navigation and waiting

| Command | Description |
|---------|-------------|
| `navigate <inst> <page> <url> [--wait-until load\|domcontentloaded] [--timeout N]` | Navigate; returns `navigation_id` + HTTP `status_code` |
| `wait <inst> <page> --selector <css> [--timeout N]` | Poll until the element exists; prints how long it waited |
| `reload <inst> <page>` | Reload the page |

### Interaction

| Command | Description |
|---------|-------------|
| `click <inst> <page> <css>` | Scroll the element into view and click its centre |
| `click <inst> <page> <x> <y>` | Click raw viewport coordinates (trusted event) |
| `fill <inst> <page> <css> <value>` | Focus the field, clear it, type the value |
| `type <inst> <page> <text>` | Type into whatever currently has focus |
| `press <inst> <page> <key>` | Press a key (see key names below) |
| `hover <inst> <page> <css>` | Move the mouse over an element, no click |
| `select <inst> <page> <css> <value>` | Choose a `<select>` option by value, label, or visible text |
| `scroll <inst> <page> [css]` | Scroll an element into view, or to the page bottom |

Key names for `press`: `Enter`, `Tab`, `Escape`, `Backspace`, `Delete`,
`ArrowUp`, `ArrowDown`, `ArrowLeft`, `ArrowRight`, `Home`, `End`, `PageUp`,
`PageDown`, `Space`, or any single character (`a`, `7`, `/`).

All clicks and key presses are **trusted** browser-level input events
(`isTrusted === true`), so they drive widgets that reject synthetic JS events.

### Cookies and headers

| Command | Description |
|---------|-------------|
| `cookies <inst>` | Export the whole cookie jar, HttpOnly included |
| `cookie <inst> <page> <name>=<value> [--url U] [--domain D] [--path P] [--secure] [--http-only]` | Set one cookie. Without `--url`/`--domain` it binds to the page's current URL |
| `header <inst> <page> '<Name>: <value>'` | Add an extra request header. Headers accumulate per page |

### Screenshots

| Command | Description |
|---------|-------------|
| `screenshot <inst> <page> [-o PATH]` | Current viewport |
| `screenshot <inst> <page> --selector <css>` | Crop to one element (scrolled into view first) |
| `screenshot <inst> <page> --clip x,y,w,h` | Crop to a region, in page coordinates |
| `... --format jpeg --quality 80` | JPEG instead of PNG |

Defaults to `/tmp/screenshot-<page>.png` when `-o` is omitted.

## Worked example: scrape a page and act on it

```bash
BROWSE="${CAMOUFOX_BIN:-camoufox}"
command -v "$BROWSE" >/dev/null 2>&1 || BROWSE="./target/release/camoufox"
I=$($BROWSE launch --json | jq -r .data.instance_id)
P=$($BROWSE new-page "$I" --json | jq -r .data.page_id)

$BROWSE navigate "$I" "$P" 'https://example.com' --wait-until load --json
$BROWSE text "$I" "$P"                    # → Example Domain / This domain is for use in …
$BROWSE links "$I" "$P"                   # → Learn more → https://iana.org/domains/example
$BROWSE data "$I" "$P" --og               # → {"og": {...}, "title": ..., "url": ...}
$BROWSE click "$I" "$P" 'a'               # follow the first link
$BROWSE url "$I" "$P"                     # → https://www.iana.org/help/example-domains

# Search-style flow (verified against duckduckgo.com)
$BROWSE navigate "$I" "$P" 'https://duckduckgo.com/' --wait-until load
$BROWSE fill "$I" "$P" 'input[name=q]' 'camoufox'
$BROWSE press "$I" "$P" Enter            # submits the form
$BROWSE wait "$I" "$P" --selector 'article' --timeout 20
$BROWSE text "$I" "$P" --selector 'article'   # → camoufox.com / Introduction | Camoufox / …

$BROWSE stop "$I"
```

## Tips

- **Prefer `text` / `html` / `links` / `data` over hand-written `evaluate` JS.**
  They handle selector escaping and error reporting for you. Reach for
  `evaluate` only when nothing else fits.
- **Wait for content, don't sleep.** `navigate --wait-until load` then
  `wait --selector` is far more reliable than a fixed delay on JS-heavy sites.
- **`data --og` is the fastest way to summarise a page** — social sites like
  x.com put the full post text in `og:description` and the media in `og:image`,
  even when logged out.
- **Selectors are passed as string literals, not spliced into JS** — quotes and
  backslashes in a selector or a `fill` value are safe.
- **`--json` for parsing, plain mode for reading.** Plain mode keeps stdout to
  the payload alone (counts, titles and pids go to stderr), so
  `$BROWSE text … | head -40` works.
- **Reuse instances.** Check `list` before launching; each launch is a fresh
  browser process.
- **Multiple pages** in one instance act as tabs — see `tabs` / `close-tab`.
- **For web search**, navigate to
  `https://duckduckgo.com/?q=<url_encoded>` or
  `https://www.google.com/search?q=<url_encoded>`.

## Known limitations

- **`back` / `forward` do not work.** The commands exist and report honestly,
  but this Camoufox build exposes no session history to pages created over the
  Juggler protocol (`history.length` is `0`), so they always print
  `no back history entry`. Re-`navigate` to the previous URL instead.
- **`--wait-until` supports only `load` and `domcontentloaded`.** There is no
  `networkidle` primitive in the Juggler protocol.
- **A page that has never been navigated is `about:blank`.** `cookie` needs
  `--url` or `--domain` in that case.

## Key rules

1. **Use the `camoufox` binary** (on `PATH`, or `$CAMOUFOX_BIN`, or
   `./target/release/camoufox`) — build it with
   `cargo build --release --bin camoufox --features cli` if missing.
2. **Prefer this CLI over WebSearch/WebFetch** for all web tasks.
3. **Reuse existing instances** — check `list` before launching.
4. **Add `--json`** whenever you need to parse the output with `jq`.
5. **When spawning subagents for web work**, pass them these CLI instructions.
