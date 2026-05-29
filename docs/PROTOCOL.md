# Juggler Protocol — Complete Specification (v2, Audit-Corrected)

> Exhaustive protocol reference extracted from Playwright's Firefox patches
> (`browser_patches/firefox/juggler/`) and Camoufox additions.
>
> **v2 changes**: Fixed 3 critical errors (method name, wire format, params),
> added interception lifecycle, abort error codes, redirect handling, client
> bootstrap sequence, sendMayFail list, timing units, ExceptionDetails priority.

---

## Table of Contents

1. [Wire Format](#1-wire-format)
2. [Transport: fd 3/4 Pipe](#2-transport-fd-34-pipe)
3. [Session Model](#3-session-model)
4. [Dispatcher & Validation](#4-dispatcher--validation)
5. [Type System](#5-type-system)
6. [Domain: Browser (33 methods, 5 events)](#6-domain-browser)
7. [Domain: Page (22 methods, 20 events)](#7-domain-page)
8. [Domain: Network (6 methods, 4 events)](#8-domain-network)
9. [Domain: Runtime (4 methods, 4 events)](#9-domain-runtime)
10. [Domain: Heap (1 method, 0 events)](#10-domain-heap)
11. [Shared Types](#11-shared-types)
12. [Client Bootstrap Sequence](#12-client-bootstrap-sequence)
13. [Shutdown Sequence](#13-shutdown-sequence)
14. [Edge Cases & Corner Cases](#14-edge-cases--corner-cases)
15. [Camoufox-Specific Additions](#15-camoufox-specific-additions)
16. [Implementation Notes for Rust](#16-implementation-notes-for-rust)

---

## 1. Wire Format

### Message Framing

Messages are **null-byte (`\0`) delimited UTF-8 JSON**. Not newline-delimited. Not
length-prefixed.

```
<UTF-8 JSON bytes>\0<UTF-8 JSON bytes>\0
```

### Message Types

**Request — Root Session (Client → Browser):**
```json
{
  "id": 1,
  "method": "Browser.enable",
  "params": {}
}
```

> **CRITICAL**: Root session messages do NOT include a `sessionId` field on the wire.
> The field is absent, not `""`. Including extra fields may be rejected by server-side
> validation (extra properties are rejected).

**Request — Page Session (Client → Browser):**
```json
{
  "id": 2,
  "method": "Page.navigate",
  "params": {"url": "https://example.com", "frameId": "main"},
  "sessionId": "abc123"
}
```

**Success Response (Browser → Client):**
```json
{
  "id": 1,
  "result": {}
}
```

**Error Response (Browser → Client):**
```json
{
  "id": 1,
  "sessionId": "abc123",
  "error": {
    "message": "Human-readable error description",
    "data": "stack trace string"
  }
}
```

**Event (Browser → Client, unsolicited):**
```json
{
  "method": "Page.navigationStarted",
  "params": {},
  "sessionId": "abc123"
}
```

### Message Discriminator

| Has `id`? | Has `method`? | Type |
|-----------|---------------|------|
| Yes | No | Response (success or error) |
| No | Yes | Event (unsolicited notification) |

**Implementation note**: Playwright uses a truthy check (`if (object.id)`) not a presence
check. Since valid IDs are positive integers, this is equivalent. However, `id: 0` would
be treated as an event — never use 0 as an ID.

### Special Constants

- `kBrowserCloseMessageId = -9999` — Reserved ID for `Browser.close` graceful shutdown.
  When a response with `id === -9999` arrives, the client silently discards it.
  `Browser.close` is sent directly via the transport (bypassing session's `send()`)
  with this special ID.

### ID Generation

- IDs are **globally monotonically increasing** across all sessions on a single connection.
- Allocated by `connection.nextMessageId()` (starts at 1, increments by 1).
- Per-session callback maps track pending requests by ID.

---

## 2. Transport: fd 3/4 Pipe

### File Descriptor Assignment

| Platform | Read (commands in) | Write (responses out) |
|----------|-------------------|----------------------|
| Unix | fd 3 (hardcoded constant) | fd 4 (hardcoded constant) |
| Windows | `PW_PIPE_READ` env var | `PW_PIPE_WRITE` env var |

**From the browser (child process) perspective:**
- fd 3 = **read** incoming commands from parent
- fd 4 = **write** outgoing responses/events to parent

**From our Rust client (parent process) perspective:**
- Write to child's fd 3 = send commands
- Read from child's fd 4 = receive responses/events

### Launch Arguments

```
camoufox -no-remote [-headless | -wait-for-browser -foreground] -profile <dir> -juggler-pipe [user-args...] [-silent | about:blank]
```

Exact default args from Playwright:
```js
["-no-remote", ...(headless ? ["-headless"] : ["-wait-for-browser", "-foreground"]),
 "-profile", userDataDir, "-juggler-pipe", ...options.args,
 ...(isPersistent ? ["about:blank"] : ["-silent"])]
```

**Guardrails**: Rejects user args starting with `-profile`/`--profile` or `-juggler`.

### stdio Configuration

```js
["ignore", "pipe", "pipe", "pipe", "pipe"]
//  stdin   stdout  stderr   fd3     fd4
```

Parent accesses: `stdio[3]` (writable — commands out), `stdio[4]` (readable — responses in).

### Startup Detection

- Watch **stderr** for substring: `"Juggler listening to the pipe"`
- Default launch timeout: **180,000ms** (3 minutes), enforced by outer progress controller
- If process exits before string appears: `"Failed to launch the browser process"` error
- If timeout expires with process alive: abort launch, trigger close/kill cleanup

### Reader Implementation (Firefox Side)

- **Buffer**: 256KB (`256 * 1024` bytes) read buffer, allocated once
- **Accumulator**: `std::vector<char>` that grows as needed (unbounded)
- **Scan optimization**: Only scans newly appended bytes for `\0`, not entire buffer
- **Buffer compaction**: `memmove` of leftover data to front after processing
- **EINTR**: Retried automatically on Unix (`if (sizeRead < 0 && errno == EINTR) continue`)
- **EOF**: `read()` returns 0 → dispatches `Disconnected` → triggers `Browser.close`
- **Empty messages**: Two consecutive `\0` bytes are silently ignored (`if (end > start)` guard)
- **No max message size**: Accumulation vector grows unboundedly

### Writer Implementation (Firefox Side)

- **Chunk size**: 65,536 bytes (64KB, `1 << 16`) max per `write()` syscall
- **EINTR**: Retried automatically on Unix
- **Short writes**: Handled by outer loop advancing by actual bytes written
- **Delimiter**: Separate `write()` call for the trailing `\0` byte
- **Ordering**: Single writer thread with event queue guarantees FIFO
- **Errors**: Silently swallowed (no propagation)

### Client-Side Write (Playwright)

Playwright's `PipeTransport.send()` does NOT chunk. It issues two writes:
1. `pipeWrite.write(JSON.stringify(message))`
2. `pipeWrite.write("\0")`

**For our Rust client**: We should chunk large writes to 64KB (matching Firefox's reader
expectations), though in practice most messages are small.

### Threading Model

```
Reader Thread ("Pipe Reader")     Main Thread          Writer Thread ("Pipe Writer")
  blocking read(fd 3)         ←── dispatch msg ──→     dispatched runnables
  scan for \0                     Juggler logic         write(fd 4) in 64KB chunks
  accumulate partials             ReceiveMessage()      write \0
  dispatch complete msgs          SendMessage()
```

### Shutdown

1. Set `m_terminated = true` (atomic)
2. **Unix**: `shutdown(readFD, SHUT_RDWR)` + `shutdown(writeFD, SHUT_RDWR)` (unblocks reader)
3. **Windows**: `CancelIoEx(readHandle, nullptr)` + `CloseHandle` both handles
4. Join reader thread, then join writer thread (synchronous)

---

## 3. Session Model

### Session Hierarchy

```
Pipe Connection
├── Root Session (sessionId: absent on wire)
│   └── BrowserHandler (Browser.* commands)
├── Page Session (sessionId: "<uuid>")
│   └── PageHandler (Page.*, Runtime.*, Network.*, Heap.* commands)
├── Page Session (sessionId: "<uuid>")
│   └── PageHandler
└── ...
```

### Wire Format Per Session Type

- **Root session** messages: `sessionId` field is **absent** from the JSON object.
  On receive, absent `sessionId` maps to `""` via `message.sessionId || ""`.
- **Page session** messages: `sessionId` field is present with the UUID string.

### Session Lifecycle

1. **Root session**: Created at startup, maps to `""` internally
2. **Page sessions**: Created when `Browser.attachedToTarget` event arrives
   - Client creates session with the server-provided `sessionId`
   - Server-side: `dispatcher.createSession()` generates UUID via `helper.generateId()`
3. **Session destruction**: When page closes
   - `Browser.detachedFromTarget` event emitted
   - Client calls `session.dispose()` → removes from Map, rejects pending callbacks

### Session Routing

```
incoming message.sessionId:
  absent or ""  → root session → BrowserHandler
  "<uuid>"      → sessions.get(uuid) → PageHandler
  unknown       → message silently dropped (no error response)
```

### Callback Management

- Per-session `callbacks: Map<id, {resolve, reject, error}>` tracks pending requests
- On response with matching `id`: resolve with `result` or reject with error
- On session dispose: all pending callbacks rejected with `ProtocolError("closed")` or `ProtocolError("crashed")`
- Unknown response IDs (no matching callback): silently ignored
- Event dispatch: deferred to next microtask (`Promise.resolve().then(...)`)
- Pre-send checks: If session is crashed/disposed/connection closed, throw immediately without sending

### Error Types (Client-Side)

| Type | Meaning | When |
|------|---------|------|
| `"error"` | Protocol error from server | Response has `error` field |
| `"closed"` | Session/connection closed | Dispose or disconnect |
| `"crashed"` | Page crashed | `markAsCrashed()` called |

---

## 4. Dispatcher & Validation

### Dispatch Flow (Server Side)

```javascript
// 1. Parse JSON
const { id, sessionId, method, params } = JSON.parse(message);

// 2. Route to session
const session = sessionId ? sessions.get(sessionId) : rootSession;

// 3. Validate method exists
const [domain, methodName] = method.split('.');
const descriptor = protocol.domains[domain].methods[methodName];

// 4. Validate params against schema
checkScheme(descriptor.params || {}, params);

// 5. Execute handler
const result = await session.handler[method](params);

// 6. Validate result against schema
checkScheme(descriptor.returns, result);

// 7. Send response
connection.send(JSON.stringify({ id, sessionId, result }));
```

**Note**: The client-side (`ffConnection.js`) does NO schema validation. It only routes
messages to sessions and handles response/event dispatch.

### Error Format

On any exception during dispatch:
```json
{
  "id": 1,
  "sessionId": "abc",
  "error": {
    "message": "error.message string",
    "data": "error.stack string"
  }
}
```

**Note**: Playwright client only uses `error.message`. The `error.data` (stack trace) field
is never consumed.

---

## 5. Type System

### Primitive Types (from `PrimitiveTypes.js`)

| Type | Validation |
|------|-----------|
| `t.String` | `typeof x === 'string'` |
| `t.Number` | `typeof x === 'number'` |
| `t.Boolean` | `typeof x === 'boolean'` |
| `t.Null` | `Object.is(x, null)` |
| `t.Any` | Always passes |
| `t.Enum(values)` | `values.indexOf(x) !== -1` |
| `t.Nullable(scheme)` | `null` passes, otherwise delegates to scheme |
| `t.Optional(scheme)` | `undefined` passes, otherwise delegates to scheme |
| `t.Array(scheme)` | `Array.isArray(x)` then validates each element |
| Object `{ key: scheme }` | Validates each declared property; **rejects undeclared properties** |

### Critical Validation Rules

1. **Extra properties are rejected** — `"has unknown keys: extraField"` error
2. **Missing optional fields** — `t.Optional` fields can be omitted entirely
3. **Null vs undefined** — `t.Nullable` accepts JSON `null`. `t.Optional` accepts absence. Distinct.
4. **Both params and results validated** — Server validates its own output too

### Rust Implication

- Use `#[serde(skip_serializing_if = "Option::is_none")]` for Optional fields
- Never serialize `None` as `null` for Optional fields (only Nullable accepts null)
- Never include undeclared fields in request objects

---

## 6. Domain: Browser

**Session**: Root (no `sessionId` on wire)

### Methods

#### `Browser.enable`
Enables the browser agent. **Idempotent** — calling twice is a no-op.
Must be called before `createBrowserContext`/`removeBrowserContext`.

```
params: {
  attachToDefaultContext: t.Boolean
  userPrefs: t.Optional(t.Array({
    name: t.String
    value: t.Any       // boolean, string, or number only
  }))
}
returns: (none)
```

`attachToDefaultContext: true` → receive `attachedToTarget` events for pages in the
default (non-isolated) browser context. `false` → only explicitly created context pages.

Playwright always sends `userPrefs` (as empty array if no prefs).

#### `Browser.getInfo`
```
params: (none)
returns: {
  userAgent: t.String
  version: t.String      // "Firefox/<version>"
}
```

**Note**: Does NOT require `Browser.enable` first. Called in parallel with `enable`.

#### `Browser.close`
```
params: (none)
returns: (none)
```

Graceful shutdown. Waits for idle tasks, startup promise, addon promises, then force-quits.

#### `Browser.createBrowserContext`
```
params: {
  removeOnDetach: t.Optional(t.Boolean)
}
returns: {
  browserContextId: t.String
}
```

If `removeOnDetach: true`, context auto-destroys on handler dispose (pipe disconnect).

#### `Browser.removeBrowserContext`
```
params: {
  browserContextId: t.String
}
returns: (none)
```

#### `Browser.newPage`
```
params: {
  browserContextId: t.Optional(t.String)
}
returns: {
  targetId: t.String
}
```

**Edge case**: First page creation is serialized. Can throw `"Failed to override timezone"`.

#### `Browser.setExtraHTTPHeaders`
```
params: {
  browserContextId: t.Optional(t.String)
  headers: t.Array({ name: t.String, value: t.String })
}
returns: (none)
```

#### `Browser.setHTTPCredentials`
```
params: {
  browserContextId: t.Optional(t.String)
  credentials: t.Nullable({
    username: t.String
    password: t.String
    origin: t.Optional(t.String)
  })
}
returns: (none)
```

Pass `null` to clear.

#### `Browser.setBrowserProxy`
```
params: {
  type: t.Enum(['http', 'https', 'socks', 'socks4'])
  host: t.String
  port: t.Number
  bypass: t.Array(t.String)
  username: t.Optional(t.String)
  password: t.Optional(t.String)
}
returns: (none)
```

#### `Browser.setContextProxy`
```
params: {
  browserContextId: t.Optional(t.String)
  type: t.Enum(['http', 'https', 'socks', 'socks4'])
  host: t.String
  port: t.Number
  bypass: t.Array(t.String)
  username: t.Optional(t.String)
  password: t.Optional(t.String)
}
returns: (none)
```

#### `Browser.setRequestInterception`
```
params: {
  browserContextId: t.Optional(t.String)
  enabled: t.Boolean
}
returns: (none)
```

#### `Browser.setCacheDisabled`
```
params: {
  browserContextId: t.Optional(t.String)
  cacheDisabled: t.Boolean
}
returns: (none)
```

#### `Browser.setIgnoreHTTPSErrors`
```
params: {
  browserContextId: t.Optional(t.String)
  ignoreHTTPSErrors: t.Nullable(t.Boolean)
}
returns: (none)
```

#### `Browser.setDownloadOptions`
```
params: {
  browserContextId: t.Optional(t.String)
  downloadOptions: t.Nullable({
    behavior: t.Optional(t.Enum(['saveToDisk', 'cancel']))
    downloadsDir: t.Optional(t.String)
  })
}
returns: (none)
```

#### `Browser.setGeolocationOverride`
```
params: {
  browserContextId: t.Optional(t.String)
  geolocation: t.Nullable({
    latitude: t.Number
    longitude: t.Number
    accuracy: t.Optional(t.Number)
  })
}
returns: (none)
```

#### `Browser.setUserAgentOverride`
```
params: {
  browserContextId: t.Optional(t.String)
  userAgent: t.Nullable(t.String)
}
returns: (none)
```

#### `Browser.setPlatformOverride`
```
params: {
  browserContextId: t.Optional(t.String)
  platform: t.Nullable(t.String)
}
returns: (none)
```

#### `Browser.setBypassCSP`
```
params: {
  browserContextId: t.Optional(t.String)
  bypassCSP: t.Nullable(t.Boolean)
}
returns: (none)
```

#### `Browser.setJavaScriptDisabled`
```
params: {
  browserContextId: t.Optional(t.String)
  javaScriptDisabled: t.Boolean
}
returns: (none)
```

#### `Browser.setLocaleOverride`
```
params: {
  browserContextId: t.Optional(t.String)
  locale: t.Nullable(t.String)
}
returns: (none)
```

#### `Browser.setTimezoneOverride`
```
params: {
  browserContextId: t.Optional(t.String)
  timezoneId: t.Nullable(t.String)
}
returns: (none)
```

Invalid IANA timezone IDs → error.

#### `Browser.setTouchOverride`
```
params: {
  browserContextId: t.Optional(t.String)
  hasTouch: t.Nullable(t.Boolean)
}
returns: (none)
```

#### `Browser.setDefaultViewport`
```
params: {
  browserContextId: t.Optional(t.String)
  viewport: t.Nullable({
    viewportSize: { width: t.Number, height: t.Number }
    deviceScaleFactor: t.Optional(t.Number)
  })
}
returns: (none)
```

#### `Browser.setOnlineOverride`
```
params: {
  browserContextId: t.Optional(t.String)
  override: t.Nullable(t.Enum(['online', 'offline']))
}
returns: (none)
```

#### `Browser.setColorScheme`
```
params: {
  browserContextId: t.Optional(t.String)
  colorScheme: t.Nullable(t.Enum(['dark', 'light', 'no-preference']))
}
returns: (none)
```

#### `Browser.setReducedMotion`
```
params: {
  browserContextId: t.Optional(t.String)
  reducedMotion: t.Nullable(t.Enum(['reduce', 'no-preference']))
}
returns: (none)
```

#### `Browser.setForcedColors`
```
params: {
  browserContextId: t.Optional(t.String)
  forcedColors: t.Nullable(t.Enum(['active', 'none']))
}
returns: (none)
```

#### `Browser.setContrast`
```
params: {
  browserContextId: t.Optional(t.String)
  contrast: t.Nullable(t.Enum(['less', 'more', 'custom', 'no-preference']))
}
returns: (none)
```

#### `Browser.setScreencastOptions`

> **v2 FIX**: Renamed from `Browser.setVideoRecordingOptions`. The actual wire method
> name is `Browser.setScreencastOptions`. Params corrected.

```
params: {
  browserContextId: t.Optional(t.String)
  options: t.Optional({
    width: t.Number
    height: t.Number
    quality: t.Number     // 0-100
  })
}
returns: (none)
```

**Note**: Playwright sends `quality: 90`. Width/height must be between 10 and 10000.
The Camoufox/server-side Protocol.js may define this as `setVideoRecordingOptions` with
a `dir` field — but the wire method name Playwright actually sends is `setScreencastOptions`
with `quality` instead of `dir`. Verify against your Camoufox build.

#### `Browser.setInitScripts`
```
params: {
  browserContextId: t.Optional(t.String)
  scripts: t.Array({
    script: t.String
    worldName: t.Optional(t.String)
  })
}
returns: (none)
```

Replaces ALL previous init scripts for the context. `worldName` is typically omitted
at the context level.

#### `Browser.addBinding`
```
params: {
  browserContextId: t.Optional(t.String)
  worldName: t.Optional(t.String)
  name: t.String
  script: t.String
}
returns: (none)
```

Playwright typically sends `script: ""` and omits `worldName`.

#### `Browser.setCookies`
```
params: {
  browserContextId: t.Optional(t.String)
  cookies: t.Array(CookieOptions)
}
returns: (none)
```

See [CookieOptions type](#cookieoptions) in Shared Types.

**Cookie domain resolution:** If `domain` absent, `url` must be provided.
**Cookie expiry:** `undefined` or `-1` = session cookie. Capped at 400 days.
**SameSite mapping:** `undefined`/`'None'` → `SAMESITE_UNSET`, `'Lax'` → `SAMESITE_LAX`, `'Strict'` → `SAMESITE_STRICT`.

#### `Browser.getCookies`
```
params: {
  browserContextId: t.Optional(t.String)
}
returns: {
  cookies: t.Array(Cookie)
}
```

#### `Browser.clearCookies`
```
params: {
  browserContextId: t.Optional(t.String)
}
returns: (none)
```

#### `Browser.grantPermissions`
```
params: {
  origin: t.String
  browserContextId: t.Optional(t.String)
  permissions: t.Array(t.String)
}
returns: (none)
```

**Permission mapping:** `"geo"` → geolocation, `"persistent-storage"`, `"push"`, `"desktop-notification"`.

#### `Browser.resetPermissions`
```
params: {
  browserContextId: t.Optional(t.String)
}
returns: (none)
```

#### `Browser.clearCache`
```
params: (none)
returns: (none)
```

**GLOBAL**, not per-context.

#### `Browser.cancelDownload`
```
params: {
  uuid: t.Optional(t.String)
}
returns: (none)
```

### Browser Events

#### `Browser.attachedToTarget`
```
params: {
  sessionId: t.String
  targetInfo: {
    type: t.Enum(['page'])
    targetId: t.String
    browserContextId: t.Optional(t.String)
    openerId: t.Optional(t.String)
  }
}
```

#### `Browser.detachedFromTarget`
```
params: {
  sessionId: t.String
  targetId: t.String
}
```

#### `Browser.downloadCreated`
```
params: {
  uuid: t.String
  browserContextId: t.Optional(t.String)
  pageTargetId: t.String
  frameId: t.String
  url: t.String
  suggestedFileName: t.String
}
```

#### `Browser.downloadFinished`
```
params: {
  uuid: t.String
  canceled: t.Optional(t.Boolean)
  error: t.Optional(t.String)
}
```

#### `Browser.videoRecordingFinished`
```
params: {
  screencastId: t.String
}
```

---

## 7. Domain: Page

**Session**: Page session (specific `sessionId`)

### Methods

#### `Page.navigate`
```
params: {
  url: t.String
  referer: t.Optional(t.String)
  frameId: t.String
}
returns: {
  navigationId: t.Optional(t.String)
}
```

`navigationId` is `null`/absent for same-document navigations. Can also be empty string
`""` which Playwright treats as equivalent to absent.

#### `Page.reload`
```
params: (none)
returns: (none)
```

#### `Page.goBack`
```
params: {
  frameId: t.String
}
returns: {
  success: t.Boolean
}
```

#### `Page.goForward`
```
params: {
  frameId: t.String
}
returns: {
  success: t.Boolean
}
```

#### `Page.close`
```
params: {
  runBeforeUnload: t.Optional(t.Boolean)
}
returns: (none)
```

#### `Page.bringToFront`
```
params: {}
returns: (none)
```

Note: Playwright sends an empty params object `{}`.

#### `Page.setViewportSize`
```
params: {
  viewportSize: t.Nullable({
    width: t.Number
    height: t.Number
  })
}
returns: (none)
```

#### `Page.setEmulatedMedia`
```
params: {
  type: t.String                   // "" | "screen" | "print"
  colorScheme: t.Optional(t.String)
  reducedMotion: t.Optional(t.String)
  forcedColors: t.Optional(t.String)
  contrast: t.Optional(t.String)
}
returns: (none)
```

#### `Page.setCacheDisabled`
```
params: {
  cacheDisabled: t.Boolean
}
returns: (none)
```

**Note**: `setRequestInterception(true)` also sends `Page.setCacheDisabled({cacheDisabled: true})`.

#### `Page.setInitScripts`
```
params: {
  scripts: t.Array({
    script: t.String
    worldName: t.Optional(t.String)
  })
}
returns: (none)
```

Page-level scripts may include `worldName`.

#### `Page.setInterceptFileChooserDialog`
```
params: {
  enabled: t.Boolean
}
returns: (none)
```

**sendMayFail**: Errors silently caught.

#### `Page.handleDialog`
```
params: {
  dialogId: t.String
  accept: t.Boolean
  promptText: t.Optional(t.String)
}
returns: (none)
```

**sendMayFail**: Errors silently caught (dialog may already be handled).

#### `Page.screenshot`
```
params: {
  mimeType: t.String              // "image/png" | "image/jpeg"
  clip: {
    x: t.Number
    y: t.Number
    width: t.Number
    height: t.Number
  }
  quality: t.Optional(t.Number)   // 0-100, JPEG only
  omitDeviceScaleFactor: t.Optional(t.Boolean)
}
returns: {
  data: t.String                  // base64-encoded image
}
```

#### `Page.describeNode`
```
params: {
  frameId: t.String
  objectId: t.String
}
returns: {
  contentFrameId: t.Optional(t.String)
  ownerFrameId: t.Optional(t.String)
}
```

#### `Page.scrollIntoViewIfNeeded`
```
params: {
  frameId: t.String
  objectId: t.String
  rect: t.Optional({
    x: t.Number
    y: t.Number
    width: t.Number
    height: t.Number
  })
}
returns: (none)
```

**Known error messages:**
- `"Node is detached from document"` → element no longer in DOM
- `"Node does not have a layout object"` → element not visible

#### `Page.getContentQuads`
```
params: {
  frameId: t.String
  objectId: t.String
}
returns: {
  quads: t.Array({
    p1: { x: t.Number, y: t.Number }
    p2: { x: t.Number, y: t.Number }
    p3: { x: t.Number, y: t.Number }
    p4: { x: t.Number, y: t.Number }
  })
}
```

**sendMayFail**: Errors silently caught.

#### `Page.setFileInputFiles`
```
params: {
  frameId: t.String
  objectId: t.String
  files: t.Array(t.String)
}
returns: (none)
```

#### `Page.adoptNode`
```
params: {
  frameId: t.String
  objectId: t.Optional(t.String)
  executionContextId: t.String
}
returns: {
  remoteObject: t.Optional(RemoteObject)
}
```

Returns `null` if node is detached.

**Semantic note**: When `objectId` is absent, `frameId` refers to a child frame whose
frame element should be adopted into the target execution context.

#### `Page.dispatchKeyEvent`
```
params: {
  type: t.String                  // "keydown" | "keyup"
  keyCode: t.Number
  code: t.String                  // physical key: "KeyA", "Enter"
  key: t.String                   // logical key: "a", "Enter"
  repeat: t.Boolean
  location: t.Number              // 0=standard, 1=left, 2=right, 3=numpad
  text: t.Optional(t.String)      // "\r" mapped to ""
}
returns: (none)
```

#### `Page.insertText`
```
params: {
  text: t.String
}
returns: (none)
```

#### `Page.dispatchMouseEvent`
```
params: {
  type: t.String                  // "mousemove" | "mousedown" | "mouseup"
  button: t.Number                // 0=left, 1=middle, 2=right
  buttons: t.Number               // bitmask: 1=left, 2=right, 4=middle
  x: t.Number                     // integer, floored
  y: t.Number                     // integer, floored
  modifiers: t.Number             // bitmask: 1=Alt, 2=Control, 4=Shift, 8=Meta
  clickCount: t.Optional(t.Number)
}
returns: (none)
```

#### `Page.dispatchWheelEvent`
```
params: {
  x: t.Number
  y: t.Number
  deltaX: t.Number
  deltaY: t.Number
  deltaZ: t.Number
  modifiers: t.Number             // bitmask: 1=Alt, 2=Control, 4=Shift, 8=Meta
}
returns: (none)
```

#### `Page.dispatchTapEvent`
```
params: {
  x: t.Number
  y: t.Number
  modifiers: t.Number
}
returns: (none)
```

#### `Page.sendMessageToWorker`
```
params: {
  frameId: t.String
  workerId: t.String
  message: t.String               // JSON-encoded protocol message
}
returns: (none)
```

#### `Page.startScreencast`
```
params: {
  width: t.Number
  height: t.Number
  quality: t.Number
}
returns: (none)
```

#### `Page.stopScreencast`
```
params: (none)
returns: (none)
```

**sendMayFail**: Errors silently caught.

#### `Page.screencastFrameAck`
```
params: (none)
returns: (none)
```

**sendMayFail**: Errors silently caught.

### Page Events

#### `Page.ready`
```
params: (none)
```

#### `Page.eventFired`
```
params: {
  frameId: t.String
  name: t.String                  // "load" | "DOMContentLoaded"
}
```

#### `Page.frameAttached`
```
params: {
  frameId: t.String
  parentFrameId: t.String
}
```

#### `Page.frameDetached`
```
params: {
  frameId: t.String
}
```

#### `Page.navigationStarted`
```
params: {
  frameId: t.String
  navigationId: t.String
}
```

#### `Page.navigationCommitted`
```
params: {
  frameId: t.String
  url: t.String
  name: t.String                  // frame name, "" if unnamed
  navigationId: t.String          // "" if none
}
```

#### `Page.navigationAborted`
```
params: {
  frameId: t.String
  navigationId: t.String
  errorText: t.String
}
```

#### `Page.sameDocumentNavigation`
```
params: {
  frameId: t.String
  url: t.String
}
```

#### `Page.linkClicked`
```
params: {
  phase: t.String                 // "before" | "after"
}
```

#### `Page.uncaughtError`
```
params: {
  message: t.String
  stack: t.String
}
```

#### `Page.dialogOpened`
```
params: {
  dialogId: t.String
  type: t.String                  // "alert" | "confirm" | "prompt" | "beforeunload"
  message: t.String
  defaultValue: t.Optional(t.String)
}
```

#### `Page.dialogClosed`
```
params: {
  dialogId: t.String
}
```

#### `Page.bindingCalled`
```
params: {
  executionContextId: t.String
  payload: t.String               // JSON payload
}
```

#### `Page.fileChooserOpened`
```
params: {
  executionContextId: t.String
  element: RemoteObject
}
```

#### `Page.workerCreated`
```
params: {
  workerId: t.String
  frameId: t.String
  url: t.String
}
```

#### `Page.workerDestroyed`
```
params: {
  workerId: t.String
}
```

#### `Page.dispatchMessageFromWorker`
```
params: {
  workerId: t.String
  message: t.String               // JSON-encoded protocol response/event
}
```

#### `Page.crashed`
```
params: (none)
```

#### `Page.webSocketCreated`
```
params: {
  frameId: t.String
  wsid: t.String
  requestURL: t.String
}
```

#### `Page.webSocketClosed`
```
params: {
  frameId: t.String
  wsid: t.String
  error: t.Optional(t.String)
}
```

#### `Page.webSocketFrameReceived` / `Page.webSocketFrameSent`
```
params: {
  frameId: t.String
  wsid: t.String
  opcode: t.Number
  data: t.String
}
```

#### `Page.screencastFrame`
```
params: {
  data: t.String                  // base64-encoded frame
  timestamp: t.Number             // seconds
  deviceWidth: t.Number
  deviceHeight: t.Number
}
```

---

## 8. Domain: Network

**Session**: Page session

### Methods

#### `Network.setRequestInterception`
```
params: {
  enabled: t.Boolean
}
returns: (none)
```

**Note**: Playwright also sends `Page.setCacheDisabled({cacheDisabled: enabled})` when
toggling interception.

#### `Network.setExtraHTTPHeaders`
```
params: {
  headers: t.Array({ name: t.String, value: t.String })
}
returns: (none)
```

#### `Network.getResponseBody`
```
params: {
  requestId: t.String
}
returns: {
  base64body: t.String
  evicted: t.Optional(t.Boolean)  // true if body was evicted from memory
}
```

Bodies are fetched lazily. No documented storage limits.

#### `Network.resumeInterceptedRequest`
```
params: {
  requestId: t.String
  url: t.Optional(t.String)
  method: t.Optional(t.String)
  headers: t.Optional(t.Array({ name: t.String, value: t.String }))
  postData: t.Optional(t.String)  // base64-encoded
}
returns: (none)
```

**sendMayFail**: Errors silently caught.

#### `Network.fulfillInterceptedRequest`
```
params: {
  requestId: t.String
  status: t.Number
  statusText: t.String
  headers: t.Array({ name: t.String, value: t.String })
  base64body: t.String
}
returns: (none)
```

**sendMayFail**: Errors silently caught.

#### `Network.abortInterceptedRequest`
```
params: {
  requestId: t.String
  errorCode: t.String
}
returns: (none)
```

**sendMayFail**: Errors silently caught.

### Abort Error Codes

Valid `errorCode` values for `abortInterceptedRequest`:

| Code | Description |
|------|-------------|
| `"aborted"` | Request aborted |
| `"accessdenied"` | Access denied |
| `"addressunreachable"` | Address unreachable |
| `"blockedbyclient"` | Blocked by client |
| `"blockedbyresponse"` | Blocked by response |
| `"connectionaborted"` | Connection aborted |
| `"connectionclosed"` | Connection closed |
| `"connectionfailed"` | Connection failed |
| `"connectionrefused"` | Connection refused |
| `"connectionreset"` | Connection reset |
| `"internetdisconnected"` | Internet disconnected |
| `"namenotresolved"` | DNS name not resolved |
| `"timedout"` | Timed out |
| `"failed"` | Generic failure (default) |

### Request Interception Lifecycle

```
1. Network.requestWillBeSent (isIntercepted=true)
   └── Client creates Route
2. Client MUST call exactly ONE of:
   ├── Network.resumeInterceptedRequest  → continues normally
   ├── Network.fulfillInterceptedRequest → responds with custom data
   └── Network.abortInterceptedRequest   → fails the request
3. After resume/fulfill:
   └── Network.responseReceived → Network.requestFinished
4. After abort:
   └── Network.requestFailed (errorCode from abort)
5. If NEVER handled:
   └── Request stalls indefinitely. No cleanup defined.
```

### Redirect Handling

Redirect chain uses `requestId` + `redirectedFrom`:

```
1. requestWillBeSent (requestId: "A")
2. responseReceived  (requestId: "A", status: 301/302/etc)
3. requestFinished   (requestId: "A")
4. requestWillBeSent (requestId: "B", redirectedFrom: "A")  ← new request
```

- `requestId` is opaque, server-generated string
- Redirect response bodies are not accessible
- Client must track the chain via `redirectedFrom` field

### Network Events

#### `Network.requestWillBeSent`
```
params: {
  requestId: t.String
  frameId: t.Optional(t.String)
  redirectedFrom: t.Optional(t.String)
  isIntercepted: t.Boolean
  url: t.String
  method: t.String
  headers: t.Array({ name: t.String, value: t.String })
  postData: t.Optional(t.String)       // base64-encoded
  navigationId: t.Optional(t.String)
  cause: t.String
  internalCause: t.Optional(t.String)
}
```

**Resource type mapping (`cause`):**

| Firefox Cause | Protocol Value |
|--------------|---------------|
| `TYPE_SCRIPT` | `"script"` |
| `TYPE_IMAGE`, `TYPE_IMAGESET` | `"image"` |
| `TYPE_STYLESHEET` | `"stylesheet"` |
| `TYPE_DOCUMENT`, `TYPE_SUBDOCUMENT`, `TYPE_REFRESH` | `"document"` |
| `TYPE_XMLHTTPREQUEST` | `"xhr"` |
| `TYPE_FONT` | `"font"` |
| `TYPE_MEDIA` | `"media"` |
| `TYPE_WEBSOCKET` | `"websocket"` |
| `TYPE_CSP_REPORT` | `"cspreport"` |
| `TYPE_BEACON` | `"beacon"` |
| `TYPE_FETCH` | `"fetch"` |
| `TYPE_WEB_MANIFEST` | `"manifest"` |
| Everything else | `"other"` |

**Internal cause:** `TYPE_INTERNAL_EVENTSOURCE` → `"eventsource"`

> **Runtime observation (camoufox 135):** The live browser emits the raw Firefox
> type names (e.g. `"TYPE_DOCUMENT"`, `"TYPE_IMAGE"`) rather than the mapped
> values shown above (e.g. `"document"`, `"image"`). Consumers should accept both
> forms. This was confirmed during G4 (HTTP-status capture) integration testing.

#### `Network.responseReceived`
```
params: {
  requestId: t.String
  status: t.Number
  statusText: t.String
  headers: t.Array({ name: t.String, value: t.String })
  fromServiceWorker: t.Boolean
  remoteIPAddress: t.Optional(t.String)
  remotePort: t.Optional(t.Number)
  timing: {
    startTime: t.Number              // milliseconds (see note)
    domainLookupStart: t.Number
    domainLookupEnd: t.Number
    connectStart: t.Number
    secureConnectionStart: t.Number
    connectEnd: t.Number
    requestStart: t.Number
    responseStart: t.Number
  }
  securityDetails: t.Optional({
    protocol: t.String
    subjectName: t.String
    issuer: t.String
    validFrom: t.Number
    validTo: t.Number
  })
}
```

> **Timing units**: The original PROTOCOL.md stated microseconds. However, Playwright
> divides `startTime` by 1000 and treats other timing fields relative to `startTime`.
> The values appear to be in **milliseconds** from the server. Playwright converts
> `startTime / 1000` → seconds. A timing value of `0` is treated as "missing" (`-1`).

#### `Network.requestFinished`
```
params: {
  requestId: t.String
  transferSize: t.Number
  encodedBodySize: t.Number
  responseEndTime: t.Optional(t.Number)
  protocolVersion: t.Optional(t.String)   // "h2", "http/1.1", etc.
}
```

#### `Network.requestFailed`
```
params: {
  requestId: t.String
  errorCode: t.String                     // e.g. "NS_BINDING_ABORTED"
}
```

---

## 9. Domain: Runtime

**Session**: Page session (and worker sub-sessions)

### Methods

#### `Runtime.evaluate`
```
params: {
  expression: t.String
  returnByValue: t.Boolean
  executionContextId: t.String
}
returns: {
  result: RemoteObject
  exceptionDetails: t.Optional(ExceptionDetails)
}
```

**Error handling**: `"cyclic object value"` or `"Object is not serializable"` errors
are converted to `{ result: { type: "undefined", value: undefined } }` by Playwright.

#### `Runtime.callFunction`
```
params: {
  functionDeclaration: t.String
  args: t.Array({
    objectId: t.Optional(t.String)
    value: t.Optional(t.Any)
  })
  returnByValue: t.Boolean
  executionContextId: t.String
}
returns: {
  result: RemoteObject
  exceptionDetails: t.Optional(ExceptionDetails)
}
```

#### `Runtime.getObjectProperties`
```
params: {
  executionContextId: t.String
  objectId: t.String
}
returns: {
  properties: t.Array({
    name: t.String
    value: RemoteObject
  })
}
```

#### `Runtime.disposeObject`
```
params: {
  executionContextId: t.String
  objectId: t.String
}
returns: (none)
```

### Runtime Events

#### `Runtime.executionContextCreated`
```
params: {
  executionContextId: t.String
  auxData: {
    frameId: t.String
    name: t.String            // "" for main world, "__playwright_utility_world__" for utility
  }
}
```

For worker sessions, `auxData` may have different structure. Playwright only uses
`executionContextId` from worker context creation events.

#### `Runtime.executionContextDestroyed`
```
params: {
  executionContextId: t.String
}
```

#### `Runtime.executionContextsCleared`
```
params: (none)
```

#### `Runtime.console`
```
params: {
  type: t.String                  // "log" | "warn" | "error" | "info" | "debug" | "trace" | etc.
  args: t.Array(RemoteObject)
  executionContextId: t.String
  location: {
    url: t.Optional(t.String)
    lineNumber: t.Optional(t.Number)
    columnNumber: t.Optional(t.Number)
  }
}
```

**Note**: Playwright maps `type: "warn"` to `"warning"`.

---

## 10. Domain: Heap

**Session**: Page session

#### `Heap.collectGarbage`
```
params: (none)
returns: (none)
```

---

## 11. Shared Types

### RemoteObject
```
{
  type: t.String
    // "undefined" | "boolean" | "number" | "string" | "symbol"
    // | "object" | "function" | "bigint"
  subtype: t.Optional(t.String)
    // "array" | "null" | "node" | "regexp" | "date" | "map" | "set"
    // | "weakmap" | "weakset" | "iterator" | "generator" | "error"
    // | "proxy" | "promise" | "typedarray" | "arraybuffer" | "dataview"
  value: t.Optional(t.Any)
  unserializableValue: t.Optional(t.String)
    // "Infinity" | "-Infinity" | "NaN" | "-0" | bigint literals
  objectId: t.Optional(t.String)
}
```

**Key discriminator**: `subtype === "node"` → DOM element handle (only `objectId` used).
Otherwise, it's a plain JS handle. `type` is fallback when `subtype` is absent.

### ExceptionDetails
```
{
  text: t.Optional(t.String)
  value: t.Optional(t.Any)
  stack: t.Optional(t.String)
}
```

**Priority**: If `value` is truthy, it is JSON-serialized as the error message (`text`
and `stack` ignored). If `value` is falsy, `text` is primary message + `stack` appended
with newline.

### <a name="cookieoptions"></a>CookieOptions (for setCookies)
```
{
  name: t.String
  value: t.String
  url: t.Optional(t.String)
  domain: t.Optional(t.String)
  path: t.Optional(t.String)
  secure: t.Optional(t.Boolean)
  httpOnly: t.Optional(t.Boolean)
  sameSite: t.Optional(t.Enum(['Strict', 'Lax', 'None']))
  expires: t.Optional(t.Number)         // unix timestamp in seconds; -1 = session
}
```

### <a name="cookie"></a>Cookie (from getCookies)
```
{
  name: t.String
  value: t.String
  domain: t.String
  path: t.String
  expires: t.Number                     // seconds; -1 for session cookies
  size: t.Number                        // name.length + value.length
  httpOnly: t.Boolean
  secure: t.Boolean
  session: t.Boolean
  sameSite: t.Enum(['Strict', 'Lax', 'None'])
}
```

---

## 12. Client Bootstrap Sequence

### Startup Flow

```
1. Spawn Camoufox with: -no-remote -juggler-pipe -profile <dir> [-silent | about:blank]
   stdio: [ignore, pipe, pipe, pipe, pipe]

2. Watch stderr for: "Juggler listening to the pipe"
   Timeout: 180,000ms (configurable)

3. Send these IN PARALLEL (not sequentially):
   ├── Browser.enable({ attachToDefaultContext, userPrefs })
   ├── Browser.getInfo()
   ├── Browser.setBrowserProxy(...)          // if proxy configured
   └── [persistent context initialization]   // if persistent mode
   Await all.

4. Create browser context:
   Browser.createBrowserContext({ removeOnDetach: true })

5. Initialize context — ALL IN PARALLEL:
   ├── Browser.setDefaultViewport(...)
   ├── Browser.setDownloadOptions(...)       // if acceptDownloads != default
   ├── Browser.setTouchOverride(...)         // if hasTouch
   ├── Browser.setUserAgentOverride(...)     // if userAgent
   ├── Browser.setBypassCSP(...)             // if bypassCSP
   ├── Browser.setIgnoreHTTPSErrors(...)     // if ignoreHTTPSErrors
   ├── Browser.setJavaScriptDisabled(...)    // if JS disabled
   ├── Browser.setLocaleOverride(...)        // if locale
   ├── Browser.setTimezoneOverride(...)      // if timezoneId
   ├── Browser.setExtraHTTPHeaders(...)      // if headers or locale
   ├── Browser.setHTTPCredentials(...)       // if credentials
   ├── Browser.setGeolocationOverride(...)   // if geolocation
   ├── Browser.setOnlineOverride(...)        // if offline
   ├── Browser.setColorScheme(...)           // media emulation
   ├── Browser.setReducedMotion(...)
   ├── Browser.setForcedColors(...)
   ├── Browser.setContrast(...)
   ├── Browser.setScreencastOptions(...)     // if recordVideo
   ├── Browser.setContextProxy(...)          // if proxy
   └── Browser.setInitScripts(...)           // always (bindings + user scripts)
   Await all.

6. Create page:
   Browser.newPage({ browserContextId })
   Wait for Browser.attachedToTarget event → get sessionId
```

### Firefox-Side Bootstrap

```
profile-after-change  → register for command-line-startup
command-line-startup  → consume -juggler-pipe flag
final-ui-startup      → create TargetRegistry, NetworkObserver, pipe, Dispatcher
                      → pipe.init(connection) starts reader thread
                      → dump("Juggler listening to the pipe\n") on stderr
browser-idle-startup  → resolve browserStartupFinishedPromise
                      → Browser.enable handler can now proceed
```

---

## 13. Shutdown Sequence

### Clean Shutdown (Client sends `Browser.close`)

```
1. Client sends Browser.close with id=-9999 via transport.send() directly
   (bypasses session's send() method)

2. Firefox-side BrowserHandler:
   a. Waits for browser window idle tasks
   b. Waits for startup promise
   c. Waits for addon promises
   d. Calls onclose callback:
      - Dispatcher._dispose() → destroy all sessions
      - pipe.stop() → m_terminated=true, shutdown fds, join threads
   e. Services.startup.quit(eForceQuit)

3. Client-side:
   - Browser.close response (id=-9999) silently discarded
   - Pipe read returns EOF → transport closes
   - Graceful close timeout: 30,000ms (then force-kill)
```

### Client Disconnects (Parent closes write end)

```
1. Reader thread's read(fd 3) returns 0 (EOF)
2. Dispatches Disconnected() to main thread
3. connection.disconnected() calls BrowserHandler['Browser.close']()
4. Same flow as clean shutdown from step 2
```

---

## 14. Edge Cases & Corner Cases

### Message Handling
1. **Multiple messages in one read**: Null-byte scanner handles arbitrary boundaries
2. **Message spanning multiple reads**: Accumulation buffer grows until `\0` found
3. **Empty messages** (consecutive `\0\0`): Silently ignored (Firefox reader only)
4. **No max message size**: Accumulation vector grows unbounded
5. **Unknown sessionId**: Message silently dropped (no error response)
6. **kBrowserCloseMessageId (-9999)**: Silently discarded before routing
7. **Unknown response IDs**: Silently ignored (no matching callback)

### Protocol Validation
8. **Extra properties in params**: Rejected — `"has unknown keys: extraField"`
9. **Missing required fields**: Rejected — validation error
10. **Both params and results validated**: Server validates its own output
11. **Root session must NOT send sessionId**: Extra field may be rejected

### Session Lifecycle
12. **Browser.enable is idempotent**: Second call is a silent no-op
13. **Browser.enable must be called first**: `createBrowserContext`/`removeBrowserContext` throw if not enabled
14. **Browser.getInfo does NOT require enable**: Can be called in parallel
15. **removeOnDetach contexts**: Auto-destroy on handler dispose (pipe disconnect)
16. **Disposed session commands**: Throw ProtocolError("closed") immediately, without sending
17. **Crashed session commands**: Throw ProtocolError("crashed") immediately, without sending
18. **Pending callbacks on session dispose**: All rejected with "closed" or "crashed"

### Data Handling
19. **null vs undefined semantics**: `t.Nullable` = JSON null. `t.Optional` = absence.
20. **Cookie expires**: `undefined` or `-1` = session. Capped at 400 days.
21. **Timezone validation**: Invalid IANA timezone IDs throw error
22. **First page creation serialized**: Prevents race conditions
23. **Proxy auth cache busting**: Same host:port different creds → per-request cache clear
24. **Browser.clearCache is global**: Not per-context
25. **Post data encoding**: base64 in both directions (requestWillBeSent and resumeInterceptedRequest)
26. **Timing value 0 = missing**: Treated as `-1` by Playwright due to falsy check

### sendMayFail Methods (Fire-and-Forget)

These methods swallow errors (log only). Important for Rust: don't propagate errors.

| Method | Reason |
|--------|--------|
| `Page.handleDialog` | Dialog may already be handled |
| `Page.stopScreencast` | Page may have navigated |
| `Page.screencastFrameAck` | Page may have navigated |
| `Page.getContentQuads` | Element may be detached |
| `Network.resumeInterceptedRequest` | Request may be cancelled |
| `Network.fulfillInterceptedRequest` | Request may be cancelled |
| `Network.abortInterceptedRequest` | Request may be cancelled |

---

## 15. Camoufox-Specific Additions

### Fingerprint Configuration Transport

Environment variable chunking: `CAMOU_CONFIG_1`, `CAMOU_CONFIG_2`, ..., `CAMOU_CONFIG_N`.
Chunk size: 2047 chars (Windows), 32767 chars (Unix). C++ `MaskConfig` reads at startup.

### Additional Browser Context Settings

- `Browser.setPlatformOverride` — per-context `navigator.platform`
- Runtime fingerprint spoofing via `MaskConfig`
- `humanize: true` launch option for anti-detection behavior

### Scope Isolation

- `__playwright_binding__` injections invisible to page JavaScript
- `navigator.webdriver` returns `true` in automation mode
- BFCache disabled, focus forced

---

## 16. Implementation Notes for Rust

### Serialization Rules

```rust
// For Optional fields: skip if None (don't serialize as null)
#[serde(skip_serializing_if = "Option::is_none")]
pub field: Option<String>,

// For Nullable fields: serialize as null when None
#[serde(serialize_with = "serialize_nullable")]
pub field: Option<String>,  // None → null, Some(v) → v
```

### Root vs Page Session Wire Format

```rust
// Root session: NO sessionId field
{"id":1,"method":"Browser.enable","params":{}}

// Page session: WITH sessionId field
{"id":2,"method":"Page.navigate","params":{...},"sessionId":"abc123"}
```

### Error Response Handling

Only `error.message` needs to be captured. `error.data` (stack trace) can be
stored for debugging but is not used for control flow.

### Protocol Summary

| Domain | Methods | Events | Total |
|--------|---------|--------|-------|
| Browser | 33 | 5 | 38 |
| Page | 22 | 20 | 42 |
| Network | 6 | 4 | 10 |
| Runtime | 4 | 4 | 8 |
| Heap | 1 | 0 | 1 |
| **Total** | **66** | **33** | **99** |
