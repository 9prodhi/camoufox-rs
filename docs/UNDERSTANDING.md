# Camoufox-rs: Protocol & Transport Understanding

> Technical reference for implementing the Juggler protocol over fd pipe transport in Rust.

---

## Table of Contents

1. [High-Level Architecture](#1-high-level-architecture)
2. [Transport Layer: fd 3/4 Pipe vs WebSocket](#2-transport-layer-fd-34-pipe-vs-websocket)
3. [Wire Protocol: Message Framing](#3-wire-protocol-message-framing)
4. [Juggler Protocol: Message Format](#4-juggler-protocol-message-format)
5. [Session Multiplexing](#5-session-multiplexing)
6. [Protocol Domains](#6-protocol-domains)
7. [Typical Message Exchange](#7-typical-message-exchange)
8. [Threading & I/O Model (Firefox Side)](#8-threading--io-model-firefox-side)
9. [Implementation Constraints](#9-implementation-constraints)
10. [Rust Implementation Strategy](#10-rust-implementation-strategy)

---

## 1. High-Level Architecture

```
┌─────────────────────┐         fd 3 (write→read)        ┌──────────────────────┐
│                     │  ──────────────────────────────►  │                      │
│   Rust Client       │                                   │   Camoufox/Firefox   │
│   (our crate)       │  ◄──────────────────────────────  │   (Juggler engine)   │
│                     │         fd 4 (read←write)         │                      │
└─────────────────────┘                                   └──────────────────────┘
        Parent Process                                          Child Process
```

**Camoufox** is an anti-detect browser built on Firefox, patched with Playwright's
**Juggler** protocol engine. The *only* way to programmatically control it is through
the Juggler protocol. There are two transport options for carrying Juggler messages.

---

## 2. Transport Layer: fd 3/4 Pipe vs WebSocket

### 2.1 fd 3/4 Pipe Transport

**What it is:** When the parent process spawns Camoufox with `-juggler-pipe`, the OS
creates two extra anonymous pipes beyond the standard stdin/stdout/stderr:

| File Descriptor | Parent Process (Rust) | Child Process (Firefox) |
|-----------------|----------------------|------------------------|
| fd 3            | **Write** (send commands) | **Read** (receive commands) |
| fd 4            | **Read** (receive responses/events) | **Write** (send responses/events) |

**How it works:**
- The parent calls `Command::new("camoufox")` with stdio configured as
  `['ignore', 'pipe', 'pipe', 'pipe', 'pipe']` (5 entries: stdin, stdout, stderr, fd3, fd4).
- In Rust, this means using `std::process::Command` with `.stdin(Stdio::null())`
  `.stdout(Stdio::piped())` `.stderr(Stdio::piped())` plus two extra pipes passed
  via platform-specific mechanisms.
- On Unix: child inherits fd 3 and fd 4 automatically when pipes are set up at those
  positions in the stdio array.
- On Windows: Handles passed via `PW_PIPE_READ` / `PW_PIPE_WRITE` environment variables.

**Characteristics:**
- Zero network overhead (kernel pipe buffer, ~64KB on Linux)
- No TCP/TLS handshake, no HTTP upgrade
- No port allocation, no port conflicts
- Inherently local-only (parent-child relationship)
- Lower latency than WebSocket (~microseconds vs ~milliseconds)
- No authentication needed (OS enforces process isolation)
- Simpler attack surface

### 2.2 WebSocket Transport

**What it is:** Camoufox can also expose a WebSocket server on a local port. Clients
connect via `ws://127.0.0.1:<port>/` and exchange the same Juggler JSON messages over
WebSocket frames.

**Characteristics:**
- Requires port allocation (potential conflicts)
- TCP + HTTP upgrade + optional TLS overhead
- Supports remote connections (can be useful, but also a security surface)
- More complex dependency chain (WebSocket library, HTTP, etc.)
- Higher latency than pipes

### 2.3 When to Use Which

| Criteria | fd Pipe | WebSocket |
|----------|---------|-----------|
| **Latency** | Lowest (kernel IPC) | Higher (TCP stack) |
| **Dependencies** | None (OS pipes) | WebSocket crate |
| **Security** | Inherent (process boundary) | Needs auth/binding |
| **Remote control** | No (local only) | Yes |
| **Simplicity** | Simpler | More moving parts |
| **Our use case** | **Best fit** | Unnecessary |

**Decision: fd pipe transport.** We control the process lifecycle, need minimal latency,
want zero network dependencies, and have no remote-control requirement.

---

## 3. Wire Protocol: Message Framing

Messages over the pipe are **null-byte (`\0`) delimited**. Not newline-delimited.
Not length-prefixed. Each message is a complete UTF-8 JSON string terminated by `\0`.

```
<JSON bytes>\0<JSON bytes>\0<JSON bytes>\0
```

### Sending (Client → Firefox via fd 3)

```
Write: {"id":1,"method":"Browser.enable","params":{}}\0
       ^--- UTF-8 JSON --------------------------^^-- delimiter
```

### Receiving (Firefox → Client via fd 4)

```
Read buffer may contain partial/multiple messages:
  {"id":1,"result":{}}\0{"method":"Page.ready","params":{...},"sessionId":"abc"}\0{"id":
  ^--- complete msg ---^^--- complete msg (event) ---------------------------------^^-- partial
```

### Framing Rules

1. **Scan for `\0`** in the read buffer
2. Everything before `\0` is a complete JSON message
3. Everything after `\0` (until next `\0` or end of buffer) is the start of the next message
4. Multiple messages can arrive in a single `read()` call
5. A single message can span multiple `read()` calls (accumulate until `\0`)
6. Messages are always valid UTF-8 JSON

### Write Chunking

Firefox's writer chunks large messages into **65,536-byte** (64KB) `write()` calls.
Our writer should do the same to avoid partial write issues with pipe buffers.

---

## 4. Juggler Protocol: Message Format

The protocol uses a JSON-RPC-like format (not strict JSON-RPC 2.0). Four message types:

### 4.1 Request (Client → Browser)

```json
{
  "id": 1,
  "method": "Browser.enable",
  "params": {},
  "sessionId": ""
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | number | Yes | Monotonically incrementing, unique per connection |
| `method` | string | Yes | Domain-qualified: `"Domain.method"` |
| `params` | object | No | Method-specific parameters (defaults to `{}`) |
| `sessionId` | string | No | `""` or absent = root/browser session |

### 4.2 Success Response (Browser → Client)

```json
{
  "id": 1,
  "sessionId": "",
  "result": { ... }
}
```

### 4.3 Error Response (Browser → Client)

```json
{
  "id": 1,
  "sessionId": "",
  "error": {
    "message": "Human-readable error",
    "data": "stack trace or details"
  }
}
```

### 4.4 Event / Notification (Browser → Client, unsolicited)

```json
{
  "method": "Page.frameAttached",
  "params": { ... },
  "sessionId": "session-id"
}
```

### Key Discriminator

- **Has `id`** → Response (success or error) to a prior request
- **Has `method` but no `id`** → Event/notification (unsolicited)

---

## 5. Session Multiplexing

A single pipe connection carries **multiple sessions** multiplexed by `sessionId`:

```
┌──────────────────────────────────────────────────┐
│                  Pipe Connection                  │
│                                                  │
│  ┌─────────────────┐  ┌─────────────────────┐   │
│  │  Root Session    │  │  Page Session "abc"  │   │
│  │  sessionId: ""   │  │  sessionId: "abc"    │   │
│  │                  │  │                      │   │
│  │  Browser.enable  │  │  Page.navigate       │   │
│  │  Browser.newPage │  │  Runtime.evaluate    │   │
│  │  Browser.close   │  │  Network.*           │   │
│  └─────────────────┘  └─────────────────────┘   │
│                        ┌─────────────────────┐   │
│                        │  Page Session "def"  │   │
│                        │  sessionId: "def"    │   │
│                        └─────────────────────┘   │
└──────────────────────────────────────────────────┘
```

- **Root session** (`sessionId: ""` or absent): Browser-level commands
- **Page sessions**: Created when `Browser.newPage` triggers `attachedToTarget` event
  with a new `sessionId`. All page-specific commands use that `sessionId`.

### Session Lifecycle

1. Client sends `Browser.enable` on root session
2. Client sends `Browser.newPage` on root session
3. Browser emits `attachedToTarget` event with `{sessionId: "abc", ...}`
4. Client now uses `sessionId: "abc"` for all `Page.*`, `Runtime.*`, `Network.*` calls
5. When page closes, browser emits `detachedFromTarget` with `{sessionId: "abc"}`

---

## 6. Protocol Domains

| Domain | Scope | Purpose | Key Methods | Key Events |
|--------|-------|---------|-------------|------------|
| **Browser** | Root session | Browser lifecycle, contexts, cookies | `enable`, `close`, `createBrowserContext`, `removeBrowserContext`, `newPage`, `setCookies`, `getCookies`, `setExtraHTTPHeaders` | `attachedToTarget`, `detachedFromTarget`, `downloadCreated`, `downloadFinished` |
| **Page** | Page session | Navigation, screenshots, input | `navigate`, `goBack`, `goForward`, `reload`, `close`, `screenshot`, `dispatchKeyEvent`, `dispatchMouseEvent`, `handleDialog` | `ready`, `crashed`, `frameAttached`, `frameDetached`, `navigationStarted`, `navigationCommitted`, `dialogOpened`, `dialogClosed` |
| **Network** | Page session | Request interception | `setRequestInterception`, `abortInterceptedRequest`, `resumeInterceptedRequest`, `fulfillInterceptedRequest`, `getResponseBody` | `requestWillBeSent`, `responseReceived`, `requestFinished`, `requestFailed` |
| **Runtime** | Page session | JS execution | `evaluate`, `callFunction`, `disposeObject`, `getObjectProperties` | `executionContextCreated`, `executionContextDestroyed`, `console` |
| **Heap** | Page session | Memory management | `collectGarbage` | *(none)* |

---

## 7. Typical Message Exchange

### Browser Startup Sequence

```
CLIENT → {"id":1,"method":"Browser.enable","params":{}}\0
CLIENT ← {"id":1,"result":{}}\0

CLIENT → {"id":2,"method":"Browser.createBrowserContext","params":{}}\0
CLIENT ← {"id":2,"result":{"browserContextId":"ctx1"}}\0

CLIENT → {"id":3,"method":"Browser.newPage","params":{"browserContextId":"ctx1"}}\0
CLIENT ← {"method":"Browser.attachedToTarget","params":{"sessionId":"page1","targetInfo":{...}}}\0
CLIENT ← {"id":3,"result":{"targetId":"..."}}\0
```

### Page Navigation

```
CLIENT → {"id":4,"method":"Page.navigate","params":{"url":"https://example.com","frameId":"main"},"sessionId":"page1"}\0
CLIENT ← {"method":"Page.navigationStarted","params":{"navigationId":"nav1","frameId":"main","url":"https://example.com"},"sessionId":"page1"}\0
CLIENT ← {"id":4,"sessionId":"page1","result":{"navigationId":"nav1","frameId":"main"}}\0
CLIENT ← {"method":"Page.navigationCommitted","params":{"..."},"sessionId":"page1"}\0
```

### JavaScript Evaluation

```
CLIENT → {"id":5,"method":"Runtime.evaluate","params":{"expression":"document.title","returnByValue":true},"sessionId":"page1"}\0
CLIENT ← {"id":5,"sessionId":"page1","result":{"result":{"type":"string","value":"Example Domain"}}}\0
```

---

## 8. Threading & I/O Model (Firefox Side)

Firefox's `nsRemoteDebuggingPipe.cpp` uses three threads:

```
┌────────────────┐     dispatch      ┌─────────────────┐
│  Reader Thread  │  ──────────────► │   Main Thread    │
│  (reads fd 3)   │   ReceiveMessage │  (Juggler logic) │
└────────────────┘                   └────────┬────────┘
                                              │ SendMessage
                                     ┌────────▼────────┐
                                     │  Writer Thread   │
                                     │  (writes fd 4)   │
                                     └─────────────────┘
```

- **Reader thread**: Blocking `read()` on fd 3, scans for `\0`, dispatches complete
  messages to main thread.
- **Main thread**: Runs Juggler dispatchers, processes requests, emits events.
- **Writer thread**: Receives serialized messages from main thread, writes to fd 4
  in 64KB chunks.
- **Shutdown**: Atomic `mTerminated` flag. Closing the write end of fd 3 causes
  the reader thread's `read()` to return 0, triggering clean shutdown.

---

## 9. Implementation Constraints

| Constraint | Value | Notes |
|------------|-------|-------|
| Max write chunk | 65,536 bytes | Per `write()` syscall |
| Message encoding | UTF-8 | Always valid UTF-8 JSON |
| Message delimiter | `\0` (null byte) | Single byte, not `\n` |
| Pipe buffer size | ~64KB (Linux default) | Kernel managed |
| Concurrent requests | Unlimited | Multiple in-flight, matched by `id` |
| `EINTR` handling | Required | Retry `read()`/`write()` on `EINTR` |
| Graceful shutdown | Close write end of fd 3 | Signals EOF to Firefox reader thread |

---

## 10. Rust Implementation Strategy

### Crate Structure (Zero External Dependencies for Transport)

```
camoufox-rs/
├── Cargo.toml
├── src/
│   ├── lib.rs              # Public API
│   ├── transport/
│   │   ├── mod.rs          # Transport trait
│   │   ├── pipe.rs         # fd 3/4 pipe transport (Unix)
│   │   └── frame.rs        # Null-byte message framing
│   ├── protocol/
│   │   ├── mod.rs
│   │   ├── types.rs        # Request, Response, Event, Error types
│   │   ├── domains.rs      # Browser, Page, Network, Runtime, Heap
│   │   └── session.rs      # Session multiplexer
│   ├── connection.rs       # Connection manager (send/receive/dispatch)
│   ├── launcher.rs         # Process spawning with fd 3/4 setup
│   └── error.rs            # Error types
```

### Key Design Decisions

1. **Transport**: Raw `std::os::unix::io::RawFd` + `std::io::Read`/`Write`.
   No tokio, no async runtime dependency for the core transport.
2. **Framing**: Custom null-byte scanner over a `BufReader`. Zero-copy where possible.
3. **Serialization**: `serde` + `serde_json` (the only external dependency — unavoidable
   for JSON).
4. **Concurrency**: Reader thread + writer thread pattern mirroring Firefox's model.
   Channels (`std::sync::mpsc`) for cross-thread message passing.
5. **Async optional**: Core is sync. Optional `async` feature flag wrapping with
   `tokio::task::spawn_blocking` or native async I/O.

### Process Spawning (Unix)

```rust
// Conceptual — exact API in launcher.rs
use std::os::unix::io::{FromRawFd, RawFd};
use std::process::{Command, Stdio};

// Create two pipe pairs for fd 3 and fd 4
let (r3, w3) = os_pipe::pipe()?;  // fd 3: parent writes, child reads
let (r4, w4) = os_pipe::pipe()?;  // fd 4: child writes, parent reads

let child = Command::new("camoufox")
    .args(["-no-remote", "-juggler-pipe", "-profile", &profile_path])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    // fd 3 and fd 4 passed via pre_exec or explicit dup2
    .spawn()?;

// Parent keeps: w3 (write to child's fd 3) and r4 (read from child's fd 4)
// Parent closes: r3, w4 (child's ends)
```

### Message Framing

```rust
// Read: accumulate until \0
fn read_message(reader: &mut BufReader<impl Read>) -> Result<String> {
    let mut buf = Vec::new();
    reader.read_until(b'\0', &mut buf)?;
    if buf.last() == Some(&b'\0') {
        buf.pop(); // remove delimiter
    }
    String::from_utf8(buf).map_err(|e| Error::InvalidUtf8(e))
}

// Write: JSON bytes + \0, chunked to 64KB
fn write_message(writer: &mut impl Write, msg: &[u8]) -> Result<()> {
    for chunk in msg.chunks(65536) {
        writer.write_all(chunk)?;
    }
    writer.write_all(b"\0")?;
    writer.flush()
}
```

---

## References

| Source | URL |
|--------|-----|
| Playwright `pipeTransport.ts` | `github.com/microsoft/playwright/.../pipeTransport.ts` |
| Playwright `browserType.ts` | `github.com/microsoft/playwright/.../browserType.ts` |
| Playwright `processLauncher.ts` | `github.com/microsoft/playwright/.../processLauncher.ts` |
| Playwright `ffConnection.ts` | `github.com/microsoft/playwright/.../ffConnection.ts` |
| Firefox `nsRemoteDebuggingPipe.cpp` | `github.com/microsoft/playwright/.../nsRemoteDebuggingPipe.cpp` |
| Firefox `Dispatcher.js` | `github.com/microsoft/playwright/.../Dispatcher.js` |
| Firefox `Protocol.js` | `github.com/microsoft/playwright/.../Protocol.js` |
| Camoufox GitHub | `github.com/daijro/camoufox` |
| Camoufox DeepWiki | `deepwiki.com/daijro/camoufox/6.1-juggler-system` |
