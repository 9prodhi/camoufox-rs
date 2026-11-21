# How to Work with Iframes and Shadow DOM

This guide explains how to target elements inside nested iframes using `--frame` and pierce open shadow-DOM boundaries using the ` >>> ` combinator.

## Prerequisites

- Daemon running with current build:
  ```bash
  ./target/release/camoufox shutdown 2>/dev/null || true
  ./target/release/camoufox serve &
  ```
  Note: An older daemon will silently ignore `--frame`. Always restart the daemon after updating.

---

## 1. Scope commands to iframes with `--frame`

The `--frame <selector>` flag scopes an operation to a child iframe element matching that selector.

`--frame` is supported across 11 CLI subcommands:
- `text`
- `html`
- `links`
- `click`
- `fill`
- `type`
- `press`
- `hover`
- `select`
- `wait`
- `evaluate`

### Extract text from an iframe
```bash
./target/release/camoufox text 00000001 p1 --frame "#ad-frame"
```

To scope to a selector inside the iframe:
```bash
./target/release/camoufox text 00000001 p1 --frame "#ad-frame" --selector "p.disclaimer"
```

### Click an element inside an iframe
```bash
./target/release/camoufox click 00000001 p1 "button.subscribe" --frame "#signup-frame"
```

`click` and `hover` automatically scroll ancestor frames and the target element into view before measuring viewport coordinates. Coordinate math accounts for positive axis-aligned CSS scaling, borders, and padding.

### Fill input fields inside an iframe
```bash
./target/release/camoufox fill 00000001 p1 "input#email" "user@example.com" --frame "#signup-frame"
```

---

## 2. Nested iframes

For nested iframes, repeat `--frame` from outermost to innermost:

```bash
./target/release/camoufox click 00000001 p1 "#submit-btn" \
    --frame "#outer-frame" \
    --frame "#inner-frame"
```

Frame resolution executes in order:
1. Resolves `#outer-frame` in the top document.
2. Resolves `#inner-frame` inside `#outer-frame`'s document context.
3. Targets `#submit-btn` inside `#inner-frame`.

A single `--timeout` budget covers all frame hops, DOM inspection, and input execution.

---

## 3. Keyboard focus rules inside iframes

`type` and `press` dispatch input to the active focused element. When used with `--frame`, they follow strict focus rules:

1. Ancestor iframes are focused outermost-first, and the target iframe window is focused.
2. The active element inside that iframe is checked.
3. If no element is focused (or if the active element is `null`, `body`, or `documentElement`), the command fails:
   ```text
   --frame <sel>: nothing is focused inside the frame; click or fill an element inside it first
   ```

### Correct usage pattern
To type or press keys in an iframe:
```bash
# Step 1: Click or fill the element inside the iframe to focus it:
./target/release/camoufox click 00000001 p1 "input#search" --frame "#frame1"

# Step 2: Now type or press keys:
./target/release/camoufox type 00000001 p1 "my query" --frame "#frame1"
./target/release/camoufox press 00000001 p1 Enter --frame "#frame1"
```

Important rules:
- There is no remembered focus target. If you interact with the top-level page, focus inside the iframe is lost. You must click or fill inside the iframe again before running `type --frame` or `press --frame`.
- Supply the **full** frame chain down to the field. Passing an incomplete chain focuses an outer frame and drops focus from the inner frame.

---

## 4. Pierce open shadow DOM with ` >>> `

To select elements inside an open shadow root, use the ` >>> ` combinator in your CSS selector:

```bash
./target/release/camoufox click 00000001 p1 "user-profile >>> button.edit"
```

### Deeply nested shadow roots
Chain ` >>> ` for multi-level shadow DOM:

```bash
./target/release/camoufox text 00000001 p1 --selector "app-shell >>> nav-drawer >>> menu-item >>> span.label"
```

### How shadow DOM piercing works
- Tokenization uses a CSS-aware parser: `>>>` inside CSS attribute brackets (e.g., `[title="a >>> b"]`), pseudo-classes, quotes, or escapes is treated as plain CSS.
- Host elements are searched depth-first in document order.
- Both absent shadow roots and closed shadow roots report:
  ```text
  element at segment N has no open shadow root (absent or closed; cannot pierce)
  ```

### Combining iframes and shadow DOM
You can combine `--frame` scoping with ` >>> ` piercing:

```bash
./target/release/camoufox click 00000001 p1 "custom-widget >>> button.action" --frame "#content-frame"
```

---

## 5. Waiting for dynamic frames and shadow roots

When elements, frames, or shadow roots load asynchronously, use `wait`:

```bash
./target/release/camoufox wait 00000001 p1 \
    --frame "#deferred-frame" \
    --selector "auth-box >>> input#code" \
    --timeout 20
```

`wait` behavior:
- Retries dynamically created, delayed, or replaced iframes.
- Retries late-attached shadow roots.
- Validates the entire CSS selector syntax upfront before attempting frame resolution.
- Fails immediately on invalid CSS or if a matched target is not an iframe.

---

## 6. Known limitations

1. **Closed shadow roots cannot be pierced**: In Camoufox's isolated world, `element.shadowRoot` returns `null` for `{ mode: 'closed' }`. An absent root cannot be distinguished from a closed one.
2. **Rotated or skewed iframes**: Coordinate transformations for pointer events support translation and positive axis-aligned scaling (`transform: scale(...)`). Rotated or skewed iframes are not supported.
3. **No focus memory**: No background focus instrumentation is injected into pages. Input focus must be explicitly set via `click` or `fill`.

---

## Related documentation

- [CLI Reference](reference-cli.md)
- [How to automate browsing with the CLI daemon](howto-cli-daemon.md)
- [Architecture & Design Rationale](explanation-architecture.md)
