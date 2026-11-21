//! Main-world execution-context tracking for one frame.
//!
//!   created(x) ──► live += x, current = x
//!   destroyed(x) ─► live -= x, current = None if current == x
//!   begin_navigation ─► token{prev: current}, current = None
//!   abort_navigation(token) ─► if current is None && prev ∈ live: current = prev
use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Debug, Default)]
struct State {
    current: Option<String>,
    live: HashSet<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ExecContextTracker {
    state: Mutex<State>,
}

#[derive(Debug, Clone)]
pub(crate) struct NavToken {
    prev: Option<String>,
}

impl ExecContextTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn current(&self) -> Option<String> {
        self.lock().current.clone()
    }

    pub(crate) fn on_created(&self, ctx_id: String) {
        let mut s = self.lock();
        s.live.insert(ctx_id.clone());
        s.current = Some(ctx_id);
    }

    pub(crate) fn on_destroyed(&self, ctx_id: &str) {
        let mut s = self.lock();
        s.live.remove(ctx_id);
        if s.current.as_deref() == Some(ctx_id) {
            s.current = None;
        }
    }

    pub(crate) fn begin_navigation(&self) -> NavToken {
        let mut s = self.lock();
        NavToken {
            prev: s.current.take(),
        }
    }

    pub(crate) fn abort_navigation(&self, token: NavToken) {
        let mut s = self.lock();
        if s.current.is_none() {
            if let Some(prev) = token.prev {
                if s.live.contains(&prev) {
                    s.current = Some(prev);
                }
            }
        }
    }

    pub(crate) fn invalidate(&self, ctx_id: &str) {
        let mut s = self.lock();
        if s.current.as_deref() == Some(ctx_id) {
            s.current = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_becomes_current() {
        let t = ExecContextTracker::new();
        t.on_created("a".into());
        assert_eq!(t.current().as_deref(), Some("a"));
    }

    #[test]
    fn abort_restores_previous_when_nothing_new() {
        let t = ExecContextTracker::new();
        t.on_created("a".into());
        let tok = t.begin_navigation();
        assert_eq!(t.current(), None);
        t.abort_navigation(tok);
        assert_eq!(t.current().as_deref(), Some("a"));
    }

    #[test]
    fn abort_keeps_newer_context() {
        let t = ExecContextTracker::new();
        t.on_created("a".into());
        let tok = t.begin_navigation();
        t.on_created("b".into());
        t.abort_navigation(tok);
        assert_eq!(t.current().as_deref(), Some("b"));
    }

    #[test]
    fn abort_skips_destroyed_previous() {
        let t = ExecContextTracker::new();
        t.on_created("a".into());
        let tok = t.begin_navigation();
        t.on_destroyed("a");
        t.abort_navigation(tok);
        assert_eq!(t.current(), None);
    }

    #[test]
    fn context_created_between_begin_and_ack_is_not_wiped() {
        let t = ExecContextTracker::new();
        t.on_created("a".into());
        let _tok = t.begin_navigation();
        t.on_created("b".into()); // arrives before the Page.navigate ack
                                  // navigate must NOT clear after the ack any more
        assert_eq!(t.current().as_deref(), Some("b"));
    }
}
