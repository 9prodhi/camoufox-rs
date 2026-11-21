//! CSS and shadow-DOM piercing selector helpers.
use super::main_frame::escape_js_json_literal as js_literal;

use std::cell::RefCell;
use std::hash::BuildHasher;

pub(crate) fn random_token() -> String {
    let random = std::collections::hash_map::RandomState::new();
    format!("{:016x}{:016x}", random.hash_one(0u8), random.hash_one(1u8))
}

thread_local! { static SELECTOR_NONCE: RefCell<Option<String>> = const { RefCell::new(None) }; }

/// Operation-local marker scope. Nested API helpers share the command nonce;
/// RAII restores the previous scope on errors and unwinding as well as success.
pub(crate) struct SelectorScope(Option<String>);
impl SelectorScope {
    pub(crate) fn enter() -> Self {
        Self(SELECTOR_NONCE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let previous = slot.clone();
            if slot.is_none() {
                *slot = Some(random_token());
            }
            previous
        }))
    }
}
impl Drop for SelectorScope {
    fn drop(&mut self) {
        SELECTOR_NONCE.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

fn marker() -> Option<String> {
    SELECTOR_NONCE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|nonce| format!("CamoufoxShadowRootError[{nonce}]: "))
    })
}

/// Split only top-level, unescaped piercing operators. Plain CSS is preserved verbatim.
pub fn tokenize(sel: &str) -> Result<Vec<&str>, &'static str> {
    let bytes = sel.as_bytes();
    let (mut quote, mut square, mut paren, mut start, mut i) = (None, 0usize, 0usize, 0, 0);
    let mut parts = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        if quote.is_none() && bytes[i..].starts_with(b"/*") {
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b'\\' {
            i += 2;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else {
            match b {
                b'\'' | b'"' => quote = Some(b),
                b'[' => square += 1,
                b']' => square = square.saturating_sub(1),
                b'(' => paren += 1,
                b')' => paren = paren.saturating_sub(1),
                b'>' if square == 0 && paren == 0 && bytes[i..].starts_with(b">>>") => {
                    parts.push(trim_unescaped(&sel[start..i]));
                    i += 3;
                    start = i;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if parts.is_empty() {
        return Ok(vec![sel]);
    }
    parts.push(trim_unescaped(&sel[start..]));
    if parts.iter().any(|s| s.is_empty()) {
        return Err("invalid selector: empty segment around >>>");
    }
    Ok(parts)
}

fn css_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

fn trim_unescaped(s: &str) -> &str {
    let s = s.trim_start_matches(css_whitespace);
    let mut end = s.len();
    while let Some(c) = s[..end].chars().next_back() {
        if !css_whitespace(c) {
            break;
        }
        let pos = end - c.len_utf8();
        if s[..pos].bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1 {
            break;
        }
        end = pos;
    }
    &s[..end]
}

/// Extract protocol exception details, preserving non-Error thrown values.
pub(crate) fn exception_message(exc: &serde_json::Value) -> String {
    let description = exc
        .pointer("/exception/description")
        .and_then(|v| v.as_str());
    if let Some(s) = description {
        return s.to_owned();
    }
    if let Some(v) = exc.get("value") {
        return v
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string());
    }
    exc.get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("script error")
        .to_owned()
}

/// Our generated errors have a distinctive name, never a substring of a user error.
pub(crate) fn shadow_error(msg: &str) -> Option<String> {
    let prefix = marker()?;
    msg.strip_prefix(&prefix)
        .map(|s| s.lines().next().unwrap_or(s).to_owned())
}

/// Only a marker minted for this command identifies our generated shadow error.
pub(crate) fn selector_exception(
    exc: &serde_json::Value,
) -> crate::protocol::errors::ProtocolError {
    use crate::protocol::errors::{ProtocolError, ProtocolErrorKind};
    let msg = exception_message(exc);
    let (kind, message) = match shadow_error(&msg) {
        Some(detail) => (
            ProtocolErrorKind::ShadowRoot,
            format!("shadow root: {detail}"),
        ),
        None => (
            ProtocolErrorKind::ScriptException,
            format!("page script error: {msg}"),
        ),
    };
    ProtocolError::new(kind, Some("Runtime.evaluate".into()), message)
}

/// Build a query expression; search all matching hosts depth-first in document order.
pub fn js_query(sel: &str) -> String {
    let parts = match tokenize(sel) {
        Ok(parts) => parts,
        Err(e) => return format!("(() => {{ throw new Error({}); }})()", js_literal(e)),
    };
    if parts.len() == 1 {
        return format!("document.querySelector({})", js_literal(sel));
    }
    let parts = parts
        .iter()
        .map(|s| js_literal(s))
        .collect::<Vec<_>>()
        .join(",");
    let selector = js_literal(sel);
    let marker = js_literal(
        &marker().unwrap_or_else(|| format!("CamoufoxShadowRootError[{}]: ", random_token())),
    );
    format!("(() => {{ const parts = [{parts}]; let missing = 0; \
      function find(root, i) {{ if (i === parts.length - 1) return root.querySelector(parts[i]); \
      for (const host of root.querySelectorAll(parts[i])) {{ \
        const shadow = host.shadowRoot; if (!shadow) {{ missing = i + 1; continue; }} \
        const el = find(shadow, i + 1); if (el) return el; \
      }} return null; }} const el = find(document, 0); \
      if (!el && missing) {{ const e = new Error({marker} + {selector} + ': element at segment ' + missing + ' has no open shadow root (absent or closed; cannot pierce)'); \
      e.name = 'CamoufoxShadowRootError'; throw e; }} return el; }})()")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokenizer_preserves_css() {
        for s in [
            "a[title=\"x >>> y\"]",
            "[x='>>>']",
            ":is([x=\">>>\"])",
            ":foo(>>>)",
            "[x=>>>]",
            "'x >>> y'",
            r"#ends\ ",
            r"a\>>>b",
            "  #a  ",
        ] {
            assert_eq!(tokenize(s), Ok(vec![s]));
            assert_eq!(
                js_query(s),
                format!("document.querySelector({})", js_literal(s))
            );
        }
    }
    #[test]
    fn tokenizer_segments() {
        assert_eq!(tokenize("  .h >>> #second  "), Ok(vec![".h", "#second"]));
        assert_eq!(
            tokenize(r"#ends\  >>> button"),
            Ok(vec![r"#ends\ ", "button"])
        );
        assert_eq!(
            tokenize(r"[x=\>>>] >>> :is(a,b) >>> c"),
            Ok(vec![r"[x=\>>>]", ":is(a,b)", "c"])
        );
        for s in [">>> a", "a >>>", "a >>> >>> b"] {
            assert_eq!(
                tokenize(s),
                Err("invalid selector: empty segment around >>>")
            );
        }
    }
    #[test]
    fn tokenizer_comments_and_css_whitespace() {
        for css in [
            "#host/* >>> */",
            "a/*' >>> ] ) */[title='x']",
            "/* >>> */ #host",
        ] {
            assert_eq!(tokenize(css), Ok(vec![css]));
            assert_eq!(
                js_query(css),
                format!("document.querySelector({})", js_literal(css))
            );
        }
        assert_eq!(
            tokenize("#host/* >>> */ >>> #target"),
            Ok(vec!["#host/* >>> */", "#target"])
        );
        assert_eq!(
            tokenize(" \t#host\u{a0} >>> \u{a0}#target\u{a0} \r\n\x0c"),
            Ok(vec!["#host\u{a0}", "\u{a0}#target\u{a0}"])
        );
    }

    #[test]
    fn shadow_marker_is_command_local() {
        use crate::protocol::errors::ProtocolErrorKind;
        let first;
        {
            let _scope = SelectorScope::enter();
            first = format!("{}no root", marker().unwrap());
            assert_eq!(
                selector_exception(&serde_json::json!({"value":first})).kind,
                ProtocolErrorKind::ShadowRoot
            );
            assert_eq!(
                selector_exception(&serde_json::json!({"value":"CamoufoxShadowRootError: mine"}))
                    .kind,
                ProtocolErrorKind::ScriptException
            );
        }
        let _next_command = SelectorScope::enter();
        assert_eq!(
            selector_exception(&serde_json::json!({"value":first})).kind,
            ProtocolErrorKind::ScriptException
        );
    }

    #[test]
    fn exception_values_and_marker() {
        assert_eq!(exception_message(&serde_json::json!({"value":42})), "42");
        assert_eq!(
            exception_message(&serde_json::json!({"value":{"a":1}})),
            "{\"a\":1}"
        );
        assert!(shadow_error("Error: closed shadow root at segment 1").is_none());
    }
}
