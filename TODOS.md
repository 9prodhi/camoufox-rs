# Project Roadmap and Verified Follow-Ups

This document tracks known architectural limitations and verified future work items for `camoufox-rs`.

---

## 1. Closed Shadow Roots

- **Status**: Documented limitation.
- **Context**: In standard DOM, shadow roots created with `{ mode: 'closed' }` do not expose `element.shadowRoot` to JavaScript outside the component. In the isolated world, absent shadow roots and closed shadow roots cannot reliably be distinguished without internal browser engine hooks.
- **Current Behavior**: The `>>>` combinator returns an error: `element at segment N has no open shadow root (absent or closed; cannot pierce)` and wait retries.
- **Follow-up**: Investigate if Juggler or Camoufox internal C++ bindings can expose closed shadow roots for automation inspection.

---

## 2. Rotated and Skewed Iframe Coordinate Transforms

- **Status**: Documented limitation.
- **Context**: Coordinate transformation for scoped pointer events (`click`, `hover`) currently accounts for translation, padding, border width, and axis-aligned scaling factors (`scaleX`, `scaleY`).
- **Current Behavior**: If an iframe or its ancestors have non-axis-aligned CSS transformations (such as `transform: rotate(...)` or `transform: skew(...)`), calculated click coordinates will not match the visual element position.
- **Follow-up**: Implement full 2D affine transformation matrix decomposition for nested frame coordinate calculation.

---

## 3. Protocol-Version Handshake Between Client and Daemon

- **Status**: Quality-of-life improvement.
- **Context**: When a user upgrades `camoufox-rs` client binaries without restarting an active background daemon (`camoufox serve`), new IPC fields (such as `frames: Vec<String>`) may be ignored or rejected by the older daemon process.
- **Current Behavior**: Users must manually run `camoufox shutdown` followed by `camoufox serve` after upgrading.
- **Follow-up**: Add a protocol handshake during initial socket connection (`ping` or hello frame) that negotiates the IPC protocol version and warns the user if the client and daemon versions diverge.

---

## 4. Session History Navigation on `Browser.newPage`

- **Status**: Upstream Juggler limitation.
- **Context**: `Page.goBack` and `Page.goForward` protocol methods are wired to `camoufox back` and `camoufox forward`. However, in pages initialized via `Browser.newPage`, Camoufox/Firefox does not populate standard session history (`history.length === 0`).
- **Current Behavior**: The commands report that no session history entry exists.
- **Follow-up**: Investigate Juggler session history initialization flags or track an in-client navigation stack as a fallback.
