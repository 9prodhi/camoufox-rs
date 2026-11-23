---
name: camoufox-browse
description: Browse the web with a real, fingerprint-resistant Firefox (Camoufox) through the `camoufox` CLI. Use instead of WebSearch/WebFetch for any web task — searching, reading JS-rendered pages, following links, filling forms, clicking, handling iframes/shadow DOM, dialogs, downloads, cookies, and screenshots.
---

# Browse the Web with Camoufox

Camoufox is a hardened, fingerprint-resistant Firefox fork driven by the
`camoufox` CLI. **Use it instead of `WebSearch` / `WebFetch` for all web
browsing** — it runs a real browser, so it renders JavaScript, keeps cookies,
and can fill forms and click things.

The CLI talks to a long-running **daemon** (`camoufox serve`) over a Unix
socket. The daemon owns browser **instances** (one headless Firefox process
each), and each instance owns **pages** (tabs). Almost every command takes
`<instance_id> <page_id>`.

## Setup (do this first)

Two binaries are involved:

- **The CLI** (`camoufox`). On `PATH` after `cargo install --path . --features
  cli`, or `target/release/camoufox` in the repo. Override with `$CAMOUFOX_CLI`.
- **The Camoufox browser** it drives. Looked up from `$CAMOUFOX_BIN`, else
  `~/.cache/camoufox/camoufox`. Current Camoufox releases (`pip install
  camoufox && python -m camoufox fetch`) install to
  `~/.cache/camoufox/browsers/official/<version>/camoufox` instead, which the
  default lookup does **not** find — so resolve it explicitly:

```bash
# CLI: prefer $CAMOUFOX_CLI, then `camoufox` on PATH, then a local release build.
BROWSE="${CAMOUFOX_CLI:-camoufox}"
command -v "$BROWSE" >/dev/null 2>&1 || BROWSE="./target/release/camoufox"
[ -x "$BROWSE" ] || command -v "$BROWSE" >/dev/null 2>&1 \
  || cargo build --release --bin camoufox --features cli   # from the repo root

# Browser: prefer $CAMOUFOX_BIN, then the legacy path, then the newest fetched release.
[ -n "$CAMOUFOX_BIN" ] || [ ! -x "$HOME/.cache/camoufox/camoufox" ] || CAMOUFOX_BIN="$HOME/.cache/camoufox/camoufox"
[ -n "$CAMOUFOX_BIN" ] || CAMOUFOX_BIN=$(ls -d "$HOME"/.cache/camoufox/browsers/official/*/camoufox 2>/dev/null | sort -V | tail -1)
```

Use `--release`, not a debug build — this tool is meant to be reused across
sessions, not rebuilt each time.

Each Bash tool call starts a fresh shell, so `BROWSE`, `CAMOUFOX_BIN`, `I` and
`P` do not carry over. Re-run the setup lines in each call, or paste the
literal instance/page ids you got back.

## Usage: `/browse <url>`

```bash
# (setup lines from above)

# 1. Ensure the daemon is running. If ping still fails, read the log.
$BROWSE ping >/dev/null 2>&1 || { ($BROWSE serve >/tmp/camoufox-daemon.log 2>&1 &); sleep 1; }
$BROWSE ping >/dev/null 2>&1 || cat /tmp/camoufox-daemon.log

# 2. Reuse an existing browser instance, or launch one. Passing --executable
#    works whether or not the daemon was started with $CAMOUFOX_BIN set.
I=$($BROWSE list --json | jq -r '.data.instances[0].instance_id // empty')
[ -n "$I" ] || I=$($BROWSE launch ${CAMOUFOX_BIN:+--executable "$CAMOUFOX_BIN"} --json | jq -r '.data.instance_id')

# 3. Create a page
P=$($BROWSE new-page "$I" --json | jq -r '.data.page_id')

# 4. Navigate, waiting for the load event, and check the HTTP status
$BROWSE navigate "$I" "$P" '<url>' --wait-until load --timeout 20 --json

# 5. Read the page — clean text, plus structured metadata
$BROWSE text "$I" "$P"
$BROWSE data "$I" "$P" --og

# 6. (Optional) screenshot the whole viewport or just one element
$BROWSE screenshot "$I" "$P" --selector 'article' -o /tmp/shot.png
```

`launch --json` failing with `failed to spawn browser ... No such file or
directory` means the browser binary was not found — set `CAMOUFOX_BIN` as in
Setup and pass `--executable`.

## Conventions

- **Scope.** Commands take `<inst> <page>` first, except: `serve`, `ping`,
  `list`, `shutdown` (daemon-scoped, no ids) and `launch`; `new-page`, `stop`,
  `tabs`, `cookies`, `downloads` (instance only).
- **Output.** Plain mode prints the payload alone on stdout (counts, titles,
  pids go to stderr), so `$BROWSE text … | head -40` works. Add `--json` to any
  command for `{"ok":true,"data":…}` / `{"ok":false,"error":…}`.
- **Exit codes.** `0` success, `1` any command failure (message on stderr as
  `error: …`), `2` bad arguments.
- **Timeouts.** Most page commands take `--timeout <secs>` (default **30**);
  `wait` defaults to **10**.
- **Global flags.** `--json` and `--socket <path>` work on every command. The
  default socket is `$XDG_RUNTIME_DIR/camoufox/daemon.sock`, else
  `/tmp/camoufox-<uid>/daemon.sock`.

## Command reference

### Session

| Command | Description |
|---------|-------------|
| `ping` | Check whether the daemon is up (`pong (N instances)`); exits 1 if not |
| `serve &` | Start the daemon (foreground process; background it and redirect its output) |
| `launch [--executable PATH] [--headed] [--dialogs manual\|accept\|dismiss] [--downloads-dir ABS] [--bypass-csp]` | Launch a browser (headless unless `--headed`); prints `instance_id` |
| `list` | List running instances with their page ids |
| `new-page <inst>` | Create a page, prints `page_id` |
| `tabs <inst>` | List open pages as `page_id  url  title` — includes popups the site opened |
| `close-tab <inst> <page>` | Close one page |
| `stop <inst>` | Stop one browser instance |
| `shutdown` | Stop the daemon and every instance |

### Reading

| Command | Description |
|---------|-------------|
| `text <inst> <page> [--selector <css>] [--frame <css>]…` | Rendered page text (`innerText`), or just that element. Empty output means the element renders nothing — try a more specific selector, or `html` |
| `html <inst> <page> [--selector <css>] [--frame <css>]…` | `outerHTML` of the selector, or the whole document |
| `links <inst> <page> [--selector <css>] [--frame <css>]…` | Every `<a href>` as `text → href` (absolute URLs) |
| `data <inst> <page> [--og] [--jsonld] [--meta]` | Structured metadata as JSON. No flags = all three groups |
| `url <inst> <page>` | Current URL (stdout) and title (stderr) |
| `evaluate <inst> <page> '<js>' [--frame <css>]…` | Run JavaScript in an **isolated world** and print the result. Sees the DOM but **not** page-defined globals (`window.appState` is `null`) |
| `evaluate --main-world <inst> <page> '<js>'` | Run in the page's own world to read app globals. Sync expressions only (a Promise comes back as `{}`), detectable by the page, blocked by strict CSP |

`data` groups: `--og` covers `og:` / `twitter:` / `article:` / `fb:` / `al:`
properties, `--jsonld` parses every `application/ld+json` block into real JSON,
`--meta` collects named `<meta>` tags. Every response also carries `title` and
`url` for context.

If `--main-world` fails with `main-world script was blocked (likely the page's
Content-Security-Policy)`, relaunch the instance with `launch --bypass-csp`
(also detectable by the page).

### Navigation and waiting

| Command | Description |
|---------|-------------|
| `navigate <inst> <page> <url> [--wait-until load\|domcontentloaded] [--timeout N]` | Navigate; returns `navigation_id` + HTTP `status_code`. Without `--wait-until` it returns as soon as navigation starts |
| `wait <inst> <page> --selector <css> [--timeout N] [--frame <css>]…` | Poll until the element exists (default 10 s); prints `found <sel> after <N>ms` |
| `reload <inst> <page>` | Reload the page |
| `back` / `forward <inst> <page>` | Exist, but see Known limitations |

- **HTTP errors are not command errors.** A 404 or 500 returns `ok` with
  `status_code: 404`; check the status yourself. `status_code` can be `null`.
- **Unreachable hosts hang until the timeout.** With `--wait-until load`, a
  connection failure blocks for the full `--timeout` and then reports `timed out
  waiting for lifecycle event "load"`, even though the browser already shows
  "Problem loading page". Pass a modest `--timeout` and check `url` (the title
  reveals the error page).
- **Reading commands do not wait.** `text --selector '#x'` fails at once with
  `selector not found: #x` if the element has not rendered yet. Use `wait` first.

### Interaction

| Command | Description |
|---------|-------------|
| `click <inst> <page> <css> [--frame <css>]…` | Scroll the element into view and click its centre |
| `click <inst> <page> <x> <y>` | Click raw viewport coordinates (trusted event; `--frame` ignored) |
| `fill <inst> <page> <css> <value> [--frame <css>]…` | Focus the field, clear it, type the value |
| `type <inst> <page> <text> [--frame <css>]…` | Type into whatever currently has focus |
| `press <inst> <page> <key> [--frame <css>]…` | Press a key (see key names below) |
| `hover <inst> <page> <css> [--frame <css>]…` | Move the mouse over an element, no click |
| `select <inst> <page> <css> <value> [--frame <css>]…` | Choose a `<select>` option by value, label, or visible text |
| `scroll <inst> <page> [css]` | Scroll an element into view, or to the page bottom |

Key names for `press`: `Enter` (or `Return`), `Tab`, `Escape`, `Backspace`,
`Delete`, `ArrowUp`, `ArrowDown`, `ArrowLeft`, `ArrowRight`, `Home`, `End`,
`PageUp`, `PageDown`, `Space`, or any single character (`a`, `7`, `/`).

All clicks and key presses are **trusted** browser-level input events
(`isTrusted === true`), so they drive widgets that reject synthetic JS events.
Interaction results are JSON. They may carry a `"dialog"` key (the action
opened a JS dialog) or a `"downloads"` key (it started a download).

### Iframes and shadow DOM

Plain `text` / `links` / selectors only see the top document. To reach inside:

- **`--frame <css>`** scopes a command to the iframe matching the selector.
  Repeat it for nested frames: `--frame '#outer' --frame '#inner'`. Supported by
  `text`, `html`, `links`, `click`, `fill`, `type`, `press`, `hover`, `select`,
  `wait`, `evaluate` — **not** by `scroll`, `screenshot`, `data`, `url`.
  `type`/`press` with `--frame` type into whatever is already focused inside
  that frame, so `click` or `fill` the field first.
- **`>>>`** pierces open shadow roots in any selector: `'my-app >>> button.submit'`,
  chains like `'a >>> b >>> c'` work. Closed shadow roots cannot be pierced.

```bash
$BROWSE text "$I" "$P" --frame 'iframe#checkout' --selector '.total'
$BROWSE fill "$I" "$P" '#card' '4242…' --frame 'iframe#checkout'
$BROWSE click "$I" "$P" 'cookie-banner >>> button.accept'
```

### Dialogs (alert / confirm / prompt / beforeunload)

The default dialog policy is `manual`. When a click or key press opens a
dialog, the command succeeds and returns a `"dialog"` object — but **every
later command on that page fails** with:

```
error: page p1 has an open alert dialog: "hi" — run `camoufox dialog 00000001 p1 accept|dismiss [--text T]`
```

Resolve it with `dialog <inst> <page> accept|dismiss [--text T]` (`--text` is
the reply to a `prompt`). To skip this entirely, launch with
`--dialogs accept` or `--dialogs dismiss`.

### Downloads

Downloads are **cancelled** unless the instance was launched with
`--downloads-dir <absolute path>`. Then:

| Command | Description |
|---------|-------------|
| `downloads <inst> [--wait SECS]` | List downloads as `<state>  <page>  <url>  -> <path>`. `--wait` blocks until the next download event; re-run until `completed` |

Files are saved under a UUID name (the site's filename is `suggested_filename`
in `--json`). Navigating straight to a file URL also downloads it and leaves
the page on its previous document.

### Cookies and headers

| Command | Description |
|---------|-------------|
| `cookies <inst>` | Export the cookie jar, HttpOnly included. Plain mode prints `name=value`; use `--json` for domain/path/flags |
| `cookie <inst> <page> <name>=<value> [--url U \| --domain D] [--path P] [--secure] [--http-only]` | Set one cookie. Without `--url`/`--domain` it binds to the page's current URL |
| `header <inst> <page> '<Name>: <value>'` | Add an extra request header. Headers accumulate per page; an empty value clears one |

### Screenshots

| Command | Description |
|---------|-------------|
| `screenshot <inst> <page> [-o PATH]` | Current viewport |
| `screenshot <inst> <page> --selector <css>` | Crop to one element (scrolled into view first) |
| `screenshot <inst> <page> --clip x,y,w,h` | Crop to a region, in page coordinates |
| `... --format jpeg --quality 80` | JPEG instead of PNG |

Without `-o`, the file goes to the daemon's private runtime directory
(`$XDG_RUNTIME_DIR/camoufox/` or `/tmp/camoufox-<uid>/`) as
`screenshot-<inst>-<page>.png`; the command prints the path. A relative `-o`
resolves against your current directory.

## Worked example: scrape a page and act on it

```bash
# (setup lines from above)
I=$($BROWSE launch ${CAMOUFOX_BIN:+--executable "$CAMOUFOX_BIN"} --json | jq -r .data.instance_id)
P=$($BROWSE new-page "$I" --json | jq -r .data.page_id)

$BROWSE navigate "$I" "$P" 'https://example.com' --wait-until load --json
$BROWSE text "$I" "$P"                    # → Example Domain / This domain is for use in …
$BROWSE links "$I" "$P"                   # → Learn more → https://iana.org/domains/example
$BROWSE data "$I" "$P" --og               # → {"og": {...}, "title": ..., "url": ...}
$BROWSE click "$I" "$P" 'a'               # follow the first link
$BROWSE url "$I" "$P"                     # → https://www.iana.org/help/example-domains

# Search-style flow
$BROWSE navigate "$I" "$P" 'https://duckduckgo.com/' --wait-until load
$BROWSE fill "$I" "$P" 'input[name=q]' 'camoufox'
$BROWSE press "$I" "$P" Enter            # submits the form
$BROWSE wait "$I" "$P" --selector 'article' --timeout 20
$BROWSE text "$I" "$P" --selector 'article'

$BROWSE stop "$I"
```

## Tips

- **Prefer `text` / `html` / `links` / `data` over hand-written `evaluate` JS.**
  They handle selector escaping, iframes, shadow DOM and error reporting for
  you. Reach for `evaluate` only when nothing else fits.
- **Wait for content, don't sleep.** `navigate --wait-until load` then
  `wait --selector` is far more reliable than a fixed delay on JS-heavy sites.
- **`data --og` is the fastest way to summarise a page** — many sites put a
  full summary in `og:description` and the lead image in `og:image`.
- **Selectors are passed as string literals, not spliced into JS** — quotes and
  backslashes in a selector or a `fill` value are safe.
- **A page that seems frozen** usually has an open dialog — the error message
  says so; run `dialog … accept|dismiss`.
- **Clicked something and nothing changed?** The site may have opened a new
  tab. Run `tabs` and switch to the new `page_id`.
- **Reuse instances.** Check `list` before launching; each launch is a fresh
  browser process. Instance ids restart from `00000001` when the daemon restarts.
- **For web search**, navigate to `https://duckduckgo.com/?q=<url_encoded>`
  (Google often serves consent or captcha pages to automated browsers).
- **After upgrading the CLI, restart the daemon** (`shutdown`, then `serve`).
  There is no client/daemon version check, and an old daemon silently ignores
  newer flags such as `--frame`.

## Known limitations

- **`back` / `forward` do not work.** The commands exist, but this Camoufox
  build exposes no session history to pages created over the Juggler protocol,
  so they print `no back history entry` — and still exit `0`. Re-`navigate` to
  the previous URL instead.
- **`--wait-until` supports only `load` and `domcontentloaded`.** There is no
  `networkidle` primitive in the Juggler protocol.
- **A page that has never been navigated is `about:blank`.** `cookie` needs
  `--url` or `--domain` in that case.
- **Requests to one instance run one at a time**, so a long `wait` blocks other
  commands on that instance. Use a second instance for parallel work.
- **Detectable features.** `evaluate --main-world` and `launch --bypass-csp`
  can both be noticed by the page; avoid them on sites with bot detection.

## Key rules

1. **Use the `camoufox` CLI** (on `PATH`, or `$CAMOUFOX_CLI`, or
   `./target/release/camoufox`) and point it at the browser with
   `$CAMOUFOX_BIN` / `launch --executable`.
2. **Prefer this CLI over WebSearch/WebFetch** for all web tasks.
3. **Reuse existing instances** — check `list` before launching.
4. **Add `--json`** whenever you need to parse the output with `jq`.
5. **Handle dialogs** — if a command reports an open dialog, resolve it before
   anything else.
6. **When spawning subagents for web work**, pass them these CLI instructions.
