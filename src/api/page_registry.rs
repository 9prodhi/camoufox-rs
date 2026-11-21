//! Persistent page-target and frame tracking per context.
//!
//! Tracks page sessions, main frames, child frames, execution contexts,
//! and JavaScript dialogs for a single `BrowserContext`.
//!
//! # Lock Ordering and Drop Lifecycle
//!
//! Handlers running on the connection reader thread dispatch events outside
//! the connection's internal lock. When event handlers run:
//!
//! ```text
//! Reader Thread:
//!   connection.inner (unlocked during dispatch)
//!     └──> PageRegistry::state lock (held briefly for updates)
//!            └──> notify condvar
//!
//! Caller Thread (new_main_frame / claim / take_popups):
//!   PageRegistry::state lock
//!     └──> Condvar::wait_timeout (releases PageRegistry::state lock while waiting)
//! ```
//!
//! Never call into Connection (including create_session), spawn close workers, or
//! drop subscription guards while holding registry state. Reader handlers never
//! send-and-wait; Page.close is performed by background workers after unlocking.
//!
//! CAUTION on Drop:
//! Dropping a `SubscriptionGuard` acquires `Connection::inner`. Therefore:
//! 1. Event handlers never capture strong `Arc<PageRegistry>` or `Arc<Connection>`
//!    references (they capture `Weak`), breaking reference cycles.
//! 2. `SubscriptionGuard`s and removed session handlers must NEVER be dropped
//!    while holding `Connection::inner` or `PageRegistry::state`. They are always
//!    moved out under the lock and dropped after unlocking.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use crate::api::browser::Session;
use crate::api::dialog::{DialogPolicy, DialogTracker};
use crate::api::exec_ctx::ExecContextTracker;
use crate::api::main_frame::SubscriptionGuard;
use crate::protocol::client::Connection;
use crate::protocol::errors::ProtocolError;
use crate::protocol::EventMessage;

/// Bounded tombstone set for detached target IDs.
///
/// Keeps at most 1024 detached target IDs for O(1) membership check,
/// evicting the oldest entries when the bound is exceeded.
struct DetachedTombstones {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl DetachedTombstones {
    fn new() -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, target_id: String) {
        if self.set.insert(target_id.clone()) {
            self.order.push_back(target_id);
            if self.order.len() > 1024 {
                if let Some(oldest) = self.order.pop_front() {
                    self.set.remove(&oldest);
                }
            }
        }
    }

    fn remove(&mut self, target_id: &str) -> bool {
        self.order.retain(|id| id != target_id);
        self.set.remove(target_id)
    }

    fn contains(&self, target_id: &str) -> bool {
        self.set.contains(target_id)
    }
}

/// Classification state of a tracked page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageState {
    /// Attached with no opener while a `new_page` request is in flight.
    Held,
    /// Site-opened popup or promoted held target.
    Popup,
    /// Claimed by `new_main_frame` or adopted as a popup.
    Claimed,
}

/// Frame information tracked in a page.
#[derive(Debug, Clone)]
pub(crate) struct FrameInfo {
    pub parent_id: Option<String>,
    pub ctx: Arc<ExecContextTracker>,
}

/// A fully-tracked page ready to be converted into a `MainFrame`.
pub(crate) struct TrackedPage {
    pub session_id: String,
    pub target_id: String,
    pub opener_id: Option<String>,
    pub main_frame_id: Option<String>,
    pub main_ctx: Arc<ExecContextTracker>,
    pub dialog: Arc<DialogTracker>,
    pub session: Session,
    pub(crate) session_guards: Vec<SubscriptionGuard>,
}

impl std::fmt::Debug for TrackedPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackedPage")
            .field("session_id", &self.session_id)
            .field("target_id", &self.target_id)
            .field("opener_id", &self.opener_id)
            .field("main_frame_id", &self.main_frame_id)
            .finish()
    }
}

// Popups that are never taken by `take_popups()` have their event handlers cleaned up
// when the page target is closed / detached (via `Browser.detachedFromTarget` dropping
// `session_guards`), or when the `BrowserContext` (and its `PageRegistry`) is dropped.
struct RegisteredPage {
    session_id: String,
    target_id: String,
    opener_id: Option<String>,
    main_frame_id: Option<String>,
    // Preserve the last identity through detach/reattach gaps; readiness still
    // depends on main_frame_id, which is cleared on detach.
    last_main_frame_id: Option<String>,
    main_ctx: Arc<ExecContextTracker>,
    frames: HashMap<String, FrameInfo>,
    detached_frames: VecDeque<String>,
    dialog: Arc<DialogTracker>,
    session: Session,
    state: PageState,
    session_guards: Vec<SubscriptionGuard>,
    attach_seq: u64,
}

struct RegistryState {
    pages: HashMap<String /*session_id*/, RegisteredPage>,
    detached: DetachedTombstones,
    abandoned_targets: DetachedTombstones,
    window_had_failure: bool,
    inflight_guards: HashMap<u64, bool /* failed */>,
    next_guard_id: u64,
    skipped: Vec<(String, Option<String>)>,
    next_seq: u64,
}

/// In-flight guard for `Browser.newPage`.
///
/// While any guard is alive, newly attached targets with no opener are held
/// in `PageState::Held`. When all guards drop, any remaining held targets
/// are promoted to `PageState::Popup` unless marked failed.
pub(crate) struct InflightGuard {
    registry: Arc<PageRegistry>,
    guard_id: u64,
    failed: bool,
    active: bool,
}

impl InflightGuard {
    pub(crate) fn mark_failed(&mut self) {
        self.failed = true;
        self.registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .window_had_failure = true;
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.active {
            self.registry.end_new_page(self.guard_id, self.failed);
        }
    }
}

#[cfg(test)]
type AttachHook = Box<dyn Fn(&str) + Send + Sync>;

/// Persistent registry of page targets and frames within one browser context.
pub(crate) struct PageRegistry {
    context_id: String,
    conn: Weak<Connection>,
    dialog_policy: Mutex<DialogPolicy>,
    state: Mutex<RegistryState>,
    condvar: Condvar,
    root_guards: Mutex<Vec<SubscriptionGuard>>,
    #[cfg(test)]
    pub(crate) on_attach_before_insert: Mutex<Option<AttachHook>>,
    #[cfg(test)]
    before_claim: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl PageRegistry {
    /// Install the registry on a connection for a specific browser context.
    ///
    /// Registers root-session listeners for `Browser.attachedToTarget` and
    /// `Browser.detachedFromTarget`. The registry owns the subscriptions.
    pub(crate) fn install(
        conn: &Arc<Connection>,
        context_id: &str,
        dialog_policy: DialogPolicy,
    ) -> Arc<Self> {
        let registry = Arc::new(Self {
            context_id: context_id.to_owned(),
            conn: Arc::downgrade(conn),
            dialog_policy: Mutex::new(dialog_policy),
            state: Mutex::new(RegistryState {
                pages: HashMap::new(),
                detached: DetachedTombstones::new(),
                abandoned_targets: DetachedTombstones::new(),
                window_had_failure: false,
                inflight_guards: HashMap::new(),
                next_guard_id: 0,
                skipped: Vec::with_capacity(16),
                next_seq: 0,
            }),
            condvar: Condvar::new(),
            root_guards: Mutex::new(Vec::new()),
            #[cfg(test)]
            on_attach_before_insert: Mutex::new(None),
            #[cfg(test)]
            before_claim: Mutex::new(None),
        });

        let mut guards = Vec::new();

        // 1. Browser.attachedToTarget on root session (Weak captures to prevent cycles)
        let reg_attach = Arc::downgrade(&registry);
        let conn_attach = Arc::downgrade(conn);
        let attach_id = conn.on_event(
            "",
            "Browser.attachedToTarget",
            Arc::new(move |event| {
                if let (Some(reg), Some(conn)) = (reg_attach.upgrade(), conn_attach.upgrade()) {
                    reg.handle_attached_to_target(&conn, event);
                }
            }),
        );
        guards.push(SubscriptionGuard::session(
            Arc::clone(conn),
            "",
            "Browser.attachedToTarget",
            attach_id,
        ));

        // 2. Browser.detachedFromTarget on root session
        let reg_detach = Arc::downgrade(&registry);
        let detach_id = conn.on_event(
            "",
            "Browser.detachedFromTarget",
            Arc::new(move |event| {
                if let Some(reg) = reg_detach.upgrade() {
                    reg.handle_detached_from_target(event);
                }
            }),
        );
        guards.push(SubscriptionGuard::session(
            Arc::clone(conn),
            "",
            "Browser.detachedFromTarget",
            detach_id,
        ));

        *registry
            .root_guards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = guards;
        registry
    }

    /// Retrieve the current dialog handling policy.
    pub(crate) fn dialog_policy(&self) -> DialogPolicy {
        *self
            .dialog_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Update the dialog handling policy for newly attached pages.
    pub(crate) fn set_dialog_policy(&self, policy: DialogPolicy) {
        *self
            .dialog_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;
    }

    /// Mark that a `Browser.newPage` command is in flight.
    pub(crate) fn begin_new_page(self: &Arc<Self>) -> InflightGuard {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let guard_id = state.next_guard_id;
        state.next_guard_id += 1;
        state.inflight_guards.insert(guard_id, false);
        InflightGuard {
            registry: Arc::clone(self),
            guard_id,
            failed: false,
            active: true,
        }
    }

    fn end_new_page(self: &Arc<Self>, guard_id: u64, failed: bool) {
        let (sessions_to_close, guards_to_drop) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.inflight_guards.remove(&guard_id);
            let remaining_inflight = !state.inflight_guards.is_empty();

            state.window_had_failure |= failed;
            if !remaining_inflight && std::mem::take(&mut state.window_had_failure) {
                // The entire creation window ended with a failure; close unclaimed Held targets.
                let held_sids: Vec<String> = state
                    .pages
                    .iter()
                    .filter(|(_, p)| p.state == PageState::Held)
                    .map(|(sid, _)| sid.clone())
                    .collect();
                let mut to_close = Vec::new();
                let mut guards = Vec::new();
                for sid in held_sids {
                    if let Some(mut p) = state.pages.remove(&sid) {
                        state.abandoned_targets.insert(p.target_id.clone());
                        to_close.push((p.session, p.target_id));
                        guards.extend(std::mem::take(&mut p.session_guards));
                    }
                }
                (to_close, guards)
            } else {
                if !remaining_inflight {
                    for page in state.pages.values_mut() {
                        if page.state == PageState::Held {
                            page.state = PageState::Popup;
                        }
                    }
                }
                (Vec::new(), Vec::new())
            }
        };

        drop(guards_to_drop);
        for (session, target_id) in sessions_to_close {
            self.close_abandoned(session, target_id);
        }
        self.condvar.notify_all();
    }

    /// Claim a newly created page target.
    ///
    /// Transitions the page target from `Held` or `Popup` to `Claimed` under
    /// the registry lock. Waits until `Page.frameAttached` (main frame) is resolved
    /// or `timeout` expires.
    pub(crate) fn claim(
        &self,
        target_id: &str,
        timeout: Duration,
    ) -> Result<TrackedPage, ProtocolError> {
        if target_id.is_empty() {
            return Err(ProtocolError::closed_with(
                "Browser.newPage",
                "missing targetId in Browser.newPage response",
            ));
        }

        #[cfg(test)]
        {
            let hook = self
                .before_claim
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(hook) = hook {
                hook();
            }
        }

        let deadline = std::time::Instant::now() + timeout;

        // Check connection health before acquiring the registry lock (N8)
        let conn = self
            .conn
            .upgrade()
            .ok_or_else(|| ProtocolError::closed(Some("Browser.attachedToTarget".to_string())))?;
        if conn.rejects_sends() {
            return Err(ProtocolError::closed(Some(
                "Browser.attachedToTarget".to_string(),
            )));
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        loop {
            if state.detached.contains(target_id) {
                return Err(ProtocolError::closed_with(
                    "Browser.attachedToTarget",
                    format!("target {target_id} was detached before claim completed"),
                ));
            }

            let found_sid = state
                .pages
                .iter()
                .find(|(_, p)| p.target_id == target_id)
                .map(|(sid, _)| sid.clone());

            if let Some(sid) = found_sid {
                let page = state.pages.get_mut(&sid).unwrap();
                if page.state == PageState::Claimed {
                    return Err(ProtocolError::closed_with(
                        "Browser.attachedToTarget",
                        format!("target {target_id} has already been claimed"),
                    ));
                }

                if page.main_frame_id.is_some() {
                    page.state = PageState::Claimed;
                    let main_ctx = Arc::clone(&page.main_ctx);
                    return Ok(TrackedPage {
                        session_id: page.session_id.clone(),
                        target_id: page.target_id.clone(),
                        opener_id: page.opener_id.clone(),
                        main_frame_id: page.main_frame_id.clone(),
                        main_ctx,
                        dialog: Arc::clone(&page.dialog),
                        session: page.session.clone(),
                        session_guards: std::mem::take(&mut page.session_guards),
                    });
                }

                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(ProtocolError::closed_with(
                        "Page.frameAttached",
                        format!(
                            "timeout waiting for main frame attach (no parentFrameId) after {:?}",
                            timeout,
                        ),
                    ));
                }
                let slice = remaining.min(Duration::from_millis(100));
                let (new_state, _) = self
                    .condvar
                    .wait_timeout(state, slice)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state = new_state;

                // Re-check connection health outside registry lock (N8)
                drop(state);
                if conn.rejects_sends() {
                    return Err(ProtocolError::closed(Some(
                        "Browser.attachedToTarget".to_string(),
                    )));
                }
                state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            } else {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    let snapshot = state.skipped.clone();
                    let summary: Vec<String> = snapshot
                        .iter()
                        .map(|(k, u)| match u {
                            Some(u) => format!("{{type:'{k}', url:'{u}'}}"),
                            None => format!("{{type:'{k}'}}"),
                        })
                        .collect();
                    let msg = format!(
                        "unknown target {target_id}; timeout waiting for type=='page' attach after {:?}; saw {} skipped: [{}]",
                        timeout,
                        snapshot.len(),
                        summary.join(", "),
                    );
                    return Err(ProtocolError::closed_with("Browser.attachedToTarget", msg));
                }
                let slice = remaining.min(Duration::from_millis(100));
                let (new_state, _) = self
                    .condvar
                    .wait_timeout(state, slice)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state = new_state;

                // Re-check connection health outside registry lock (N8)
                drop(state);
                if conn.rejects_sends() {
                    return Err(ProtocolError::closed(Some(
                        "Browser.attachedToTarget".to_string(),
                    )));
                }
                state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    /// Drain all ready popups (main frame known) and mark them claimed.
    pub(crate) fn take_popups(&self) -> Vec<TrackedPage> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ready: Vec<(u64, TrackedPage)> = Vec::new();
        for page in state.pages.values_mut() {
            if page.state == PageState::Popup && page.main_frame_id.is_some() {
                page.state = PageState::Claimed;
                let main_ctx = Arc::clone(&page.main_ctx);
                ready.push((
                    page.attach_seq,
                    TrackedPage {
                        session_id: page.session_id.clone(),
                        target_id: page.target_id.clone(),
                        opener_id: page.opener_id.clone(),
                        main_frame_id: page.main_frame_id.clone(),
                        main_ctx,
                        dialog: Arc::clone(&page.dialog),
                        session: page.session.clone(),
                        session_guards: std::mem::take(&mut page.session_guards),
                    },
                ));
            }
        }
        ready.sort_by_key(|(seq, _)| *seq);
        ready.into_iter().map(|(_, p)| p).collect()
    }

    /// Record an abandoned target ID so it is never adopted as a popup.
    ///
    /// If the target is already attached, removes it from tracking and sends
    /// `Page.close` in a background thread.
    pub(crate) fn abandon_target(self: &Arc<Self>, target_id: &str) {
        let (session_to_close, guards_to_drop) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            // N4: If target is Claimed, do not touch it!
            let is_claimed = state
                .pages
                .values()
                .any(|p| p.target_id == target_id && p.state == PageState::Claimed);
            if is_claimed || state.detached.contains(target_id) {
                return;
            }

            state.abandoned_targets.insert(target_id.to_string());
            let found_sid = state
                .pages
                .iter()
                .find(|(_, p)| p.target_id == target_id)
                .map(|(sid, _)| sid.clone());
            if let Some(sid) = found_sid {
                if let Some(mut p) = state.pages.remove(&sid) {
                    (Some(p.session), std::mem::take(&mut p.session_guards))
                } else {
                    (None, Vec::new())
                }
            } else {
                (None, Vec::new())
            }
        };
        // Drop session guards outside registry lock (C1/N8)
        drop(guards_to_drop);
        if let Some(session) = session_to_close {
            self.close_abandoned(session, target_id.to_owned());
        }
    }

    fn close_abandoned(self: &Arc<Self>, session: Session, target_id: String) {
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || {
            let _ = session.send("Page.close", serde_json::json!({}));
            if let Some(registry) = weak.upgrade() {
                registry
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .abandoned_targets
                    .remove(&target_id);
            }
        });
    }

    /// Last known main-frame identity while the page exists, including a
    /// detach/reattach gap with no main frame attached. None after page removal.
    pub(crate) fn main_frame_id(&self, session_id: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pages
            .get(session_id)?
            .last_main_frame_id
            .clone()
    }

    /// Check whether a target ID has been detached.
    #[cfg(any(feature = "cli", test))]
    pub(crate) fn is_detached(&self, target_id: &str) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.detached.contains(target_id)
    }

    /// Look up the execution context tracker for a specific frame of a page.
    pub(crate) fn frame_ctx(
        &self,
        session_id: &str,
        frame_id: &str,
    ) -> Option<Arc<ExecContextTracker>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let page = state.pages.get(session_id)?;
        page.frames.get(frame_id).map(|f| Arc::clone(&f.ctx))
    }

    /// List all frames tracked for a page session as `(frame_id, parent_id)` pairs.
    pub(crate) fn frames(&self, session_id: &str) -> Vec<(String, Option<String>)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.pages.get(session_id) {
            Some(page) => page
                .frames
                .iter()
                .map(|(id, info)| (id.clone(), info.parent_id.clone()))
                .collect(),
            None => Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn page_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pages
            .len()
    }

    fn handle_attached_to_target(self: &Arc<Self>, conn: &Arc<Connection>, event: &EventMessage) {
        let ti = event.params.get("targetInfo");
        let t_type = ti
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let t_ctx = ti
            .and_then(|v| v.get("browserContextId"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                event
                    .params
                    .get("browserContextId")
                    .and_then(|v| v.as_str())
            });
        let t_url = ti
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());

        // C6: Require explicit match with our context id.
        match t_ctx {
            Some(ctx) if ctx == self.context_id => {}
            _ => return,
        }

        if t_type != "page" {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.skipped.len() < 16 {
                state.skipped.push((t_type.to_owned(), t_url));
            }
            return;
        }

        let session_id = event
            .params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let target_id = ti
            .and_then(|v| v.get("targetId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let opener_id = ti
            .and_then(|v| v.get("openerId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());

        if session_id.is_empty() || target_id.is_empty() {
            return;
        }

        // Decide under the registry lock; all connection work happens after unlock.
        let abandoned = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abandoned_targets
            .contains(&target_id);
        if abandoned {
            let session = conn.create_session(session_id);
            self.close_abandoned(session, target_id);
            return;
        }

        let current_policy = *self
            .dialog_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dialog = DialogTracker::new(current_policy);
        let session = conn.create_session(session_id.clone());
        let mut session_guards = Vec::new();

        session_guards.extend(dialog.install(conn, &session));

        // Weak references for per-session handlers (C2)
        let reg_weak = Arc::downgrade(self);
        let sid_frame = session_id.clone();
        let frame_att_id = conn.on_event(
            &session_id,
            "Page.frameAttached",
            Arc::new(move |ev| {
                if let Some(reg) = reg_weak.upgrade() {
                    reg.handle_frame_attached(&sid_frame, ev);
                }
            }),
        );
        session_guards.push(SubscriptionGuard::session(
            Arc::clone(conn),
            &session_id,
            "Page.frameAttached",
            frame_att_id,
        ));

        let reg_weak = Arc::downgrade(self);
        let sid_frame_det = session_id.clone();
        let frame_det_id = conn.on_event(
            &session_id,
            "Page.frameDetached",
            Arc::new(move |ev| {
                if let Some(reg) = reg_weak.upgrade() {
                    reg.handle_frame_detached(&sid_frame_det, ev);
                }
            }),
        );
        session_guards.push(SubscriptionGuard::session(
            Arc::clone(conn),
            &session_id,
            "Page.frameDetached",
            frame_det_id,
        ));

        let reg_weak = Arc::downgrade(self);
        let sid_ctx_created = session_id.clone();
        let ctx_created_id = conn.on_event(
            &session_id,
            "Runtime.executionContextCreated",
            Arc::new(move |ev| {
                if let Some(reg) = reg_weak.upgrade() {
                    reg.handle_exec_context_created(&sid_ctx_created, ev);
                }
            }),
        );
        session_guards.push(SubscriptionGuard::session(
            Arc::clone(conn),
            &session_id,
            "Runtime.executionContextCreated",
            ctx_created_id,
        ));

        let reg_weak = Arc::downgrade(self);
        let sid_ctx_destroyed = session_id.clone();
        let ctx_destroyed_id = conn.on_event(
            &session_id,
            "Runtime.executionContextDestroyed",
            Arc::new(move |ev| {
                if let Some(reg) = reg_weak.upgrade() {
                    reg.handle_exec_context_destroyed(&sid_ctx_destroyed, ev);
                }
            }),
        );
        session_guards.push(SubscriptionGuard::session(
            Arc::clone(conn),
            &session_id,
            "Runtime.executionContextDestroyed",
            ctx_destroyed_id,
        ));

        // Test hook before insert (N2 race testing)
        #[cfg(test)]
        {
            let hook = self
                .on_attach_before_insert
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cb) = hook.as_ref() {
                cb(&target_id);
            }
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // N2: Re-check abandoned_targets under the same lock acquisition that inserts
        if state.abandoned_targets.contains(&target_id) {
            drop(state);
            drop(session_guards);
            self.close_abandoned(session, target_id);
            return;
        }

        let page_state = if opener_id.is_some() {
            PageState::Popup
        } else if !state.inflight_guards.is_empty() {
            PageState::Held
        } else {
            PageState::Popup
        };

        let seq = state.next_seq;
        state.next_seq += 1;

        // A repeated attach replaces the record. Move the old record out so
        // its subscription guards unregister only after releasing state.
        let replaced = state.pages.insert(
            session_id.clone(),
            RegisteredPage {
                session_id,
                target_id,
                opener_id,
                main_frame_id: None,
                last_main_frame_id: None,
                main_ctx: Arc::new(ExecContextTracker::new()),
                frames: HashMap::new(),
                detached_frames: VecDeque::new(),
                dialog,
                session,
                state: page_state,
                session_guards,
                attach_seq: seq,
            },
        );

        drop(state);
        drop(replaced);
        self.condvar.notify_all();
    }

    fn handle_frame_attached(&self, session_id: &str, event: &EventMessage) {
        let parent_id = event
            .params
            .get("parentFrameId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let frame_id = event
            .params
            .get("frameId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if frame_id.is_empty() {
            return;
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(page) = state.pages.get_mut(session_id) {
            page.detached_frames.retain(|id| id != &frame_id);
            if parent_id.is_none() {
                // N6: Main frame attached. Re-use page's main_ctx across attachments.
                page.main_frame_id = Some(frame_id.clone());
                page.last_main_frame_id = Some(frame_id.clone());
                if let Some(existing) = page.frames.get(&frame_id) {
                    if !Arc::ptr_eq(&existing.ctx, &page.main_ctx) {
                        if let Some(curr) = existing.ctx.current() {
                            page.main_ctx.on_created(curr);
                        }
                    }
                }
                let entry = page.frames.entry(frame_id).or_insert_with(|| FrameInfo {
                    parent_id: None,
                    ctx: Arc::clone(&page.main_ctx),
                });
                entry.parent_id = None;
                entry.ctx = Arc::clone(&page.main_ctx);
            } else {
                let entry = page.frames.entry(frame_id).or_insert_with(|| FrameInfo {
                    parent_id: parent_id.clone(),
                    ctx: Arc::new(ExecContextTracker::new()),
                });
                entry.parent_id = parent_id;
            }
        }
        self.condvar.notify_all();
    }

    fn handle_frame_detached(&self, session_id: &str, event: &EventMessage) {
        let frame_id = event
            .params
            .get("frameId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if frame_id.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(page) = state.pages.get_mut(session_id) {
            let mut to_remove = vec![frame_id.to_string()];
            let mut idx = 0;
            while idx < to_remove.len() {
                let parent = to_remove[idx].clone();
                for (fid, info) in &page.frames {
                    if info.parent_id.as_deref() == Some(&parent) && !to_remove.contains(fid) {
                        to_remove.push(fid.clone());
                    }
                }
                idx += 1;
            }
            for fid in to_remove {
                page.frames.remove(&fid);
                // N7: Record detached frame ID to ignore late execution context events
                if !page.detached_frames.contains(&fid) {
                    page.detached_frames.push_back(fid);
                    if page.detached_frames.len() > 64 {
                        page.detached_frames.pop_front();
                    }
                }
            }
            if page.main_frame_id.as_deref() == Some(frame_id) {
                page.main_frame_id = None;
            }
        }
    }

    fn handle_exec_context_created(&self, session_id: &str, event: &EventMessage) {
        let aux = event.params.get("auxData");
        let frame_id = aux
            .and_then(|a| a.get("frameId"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let is_main_world = aux
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
            .map(|n| n.is_empty())
            .unwrap_or(true);
        let ctx_id = event
            .params
            .get("executionContextId")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !is_main_world || frame_id.is_empty() || ctx_id.is_empty() {
            return;
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(page) = state.pages.get_mut(session_id) {
            // N7: Ignore late execution context for a detached frame
            if page.detached_frames.iter().any(|df| df == frame_id) {
                return;
            }

            let entry = page
                .frames
                .entry(frame_id.to_string())
                .or_insert_with(|| FrameInfo {
                    parent_id: None,
                    ctx: if page.main_frame_id.as_deref() == Some(frame_id) {
                        Arc::clone(&page.main_ctx)
                    } else {
                        Arc::new(ExecContextTracker::new())
                    },
                });
            entry.ctx.on_created(ctx_id.to_string());
        }
    }

    fn handle_exec_context_destroyed(&self, session_id: &str, event: &EventMessage) {
        let dead = event
            .params
            .get("executionContextId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if dead.is_empty() {
            return;
        }
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(page) = state.pages.get(session_id) {
            page.main_ctx.on_destroyed(dead);
            for frame in page.frames.values() {
                if !Arc::ptr_eq(&frame.ctx, &page.main_ctx) {
                    frame.ctx.on_destroyed(dead);
                }
            }
        }
    }

    fn handle_detached_from_target(&self, event: &EventMessage) {
        let sid = event
            .params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let tid = event.params.get("targetId").and_then(|v| v.as_str());

        let guards_to_drop = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            let mut our_page = false;
            let mut guards = Vec::new();

            if let Some(mut page) = state.pages.remove(sid) {
                our_page = true;
                // N3: record tombstones only for targets THIS registry tracks or has abandoned
                state.detached.insert(page.target_id.clone());
                // N10: prune abandoned_targets
                state.abandoned_targets.remove(&page.target_id);
                guards = std::mem::take(&mut page.session_guards);
            }

            if let Some(tid) = tid {
                if state.abandoned_targets.remove(tid) {
                    our_page = true;
                    state.detached.insert(tid.to_owned());
                }
            }

            if our_page {
                self.condvar.notify_all();
            }
            guards
        };
        // Drop session guards outside the registry lock (C1/C7/N8)
        drop(guards_to_drop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RawMessage;
    use crate::transport::errors::TransportError;
    use crate::transport::{Transport, TransportReader, TransportWriter};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    type Recorded = Arc<Mutex<Vec<(serde_json::Value, std::thread::ThreadId)>>>;

    struct TestWriter {
        records: Recorded,
        closed: Arc<AtomicBool>,
    }

    impl TransportWriter for TestWriter {
        fn send(&mut self, msg: &serde_json::Value) -> Result<(), TransportError> {
            self.records
                .lock()
                .unwrap()
                .push((msg.clone(), std::thread::current().id()));
            Ok(())
        }
        fn close(&mut self) -> Result<(), TransportError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    struct TestReader {
        incoming_rx: mpsc::Receiver<RawMessage>,
        closed: Arc<AtomicBool>,
    }

    impl TransportReader for TestReader {
        fn receive(&mut self) -> Result<RawMessage, TransportError> {
            match self.incoming_rx.recv() {
                Ok(msg) => Ok(msg),
                Err(_) => {
                    self.closed.store(true, Ordering::SeqCst);
                    Err(TransportError::Closed)
                }
            }
        }
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    struct TestTransport {
        records: Recorded,
        incoming_rx: mpsc::Receiver<RawMessage>,
        closed: Arc<AtomicBool>,
    }

    impl TestTransport {
        fn new() -> (Self, mpsc::Sender<RawMessage>) {
            let (in_tx, in_rx) = mpsc::channel();
            (
                Self {
                    records: Arc::default(),
                    incoming_rx: in_rx,
                    closed: Arc::new(AtomicBool::new(false)),
                },
                in_tx,
            )
        }
    }

    impl Transport for TestTransport {
        fn into_split(self: Box<Self>) -> (Box<dyn TransportWriter>, Box<dyn TransportReader>) {
            (
                Box::new(TestWriter {
                    records: self.records,
                    closed: Arc::clone(&self.closed),
                }),
                Box::new(TestReader {
                    incoming_rx: self.incoming_rx,
                    closed: self.closed,
                }),
            )
        }
    }

    fn wait_for<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    fn sync_reader(conn: &Arc<Connection>, tx: &mpsc::Sender<RawMessage>) -> std::thread::ThreadId {
        let (done_tx, done_rx) = mpsc::channel();
        let id = conn.on_event(
            "",
            "Test.sentinel",
            Arc::new(move |_| {
                done_tx.send(std::thread::current().id()).unwrap();
            }),
        );
        tx.send(RawMessage {
            id: None,
            method: Some("Test.sentinel".into()),
            params: Some(serde_json::json!({})),
            session_id: None,
            result: None,
            error: None,
        })
        .unwrap();
        let reader = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        conn.off_event("", "Test.sentinel", id);
        reader
    }

    fn attached_to_target_msg(
        session_id: &str,
        target_id: &str,
        browser_context_id: &str,
        opener_id: Option<&str>,
    ) -> RawMessage {
        let mut ti = serde_json::json!({
            "type": "page",
            "targetId": target_id,
            "browserContextId": browser_context_id,
        });
        if let Some(op) = opener_id {
            ti["openerId"] = serde_json::json!(op);
        }
        RawMessage {
            id: None,
            method: Some("Browser.attachedToTarget".into()),
            params: Some(serde_json::json!({
                "sessionId": session_id,
                "targetInfo": ti,
            })),
            result: None,
            error: None,
            session_id: None,
        }
    }

    fn frame_attached_msg(
        session_id: &str,
        frame_id: &str,
        parent_frame_id: Option<&str>,
    ) -> RawMessage {
        let mut params = serde_json::json!({
            "frameId": frame_id,
        });
        if let Some(p) = parent_frame_id {
            params["parentFrameId"] = serde_json::json!(p);
        }
        RawMessage {
            id: None,
            method: Some("Page.frameAttached".into()),
            params: Some(params),
            result: None,
            error: None,
            session_id: Some(session_id.into()),
        }
    }

    fn frame_detached_msg(session_id: &str, frame_id: &str) -> RawMessage {
        RawMessage {
            id: None,
            method: Some("Page.frameDetached".into()),
            params: Some(serde_json::json!({
                "frameId": frame_id,
            })),
            result: None,
            error: None,
            session_id: Some(session_id.into()),
        }
    }

    fn exec_context_created_msg(
        session_id: &str,
        frame_id: &str,
        ctx_id: &str,
        name: &str,
    ) -> RawMessage {
        RawMessage {
            id: None,
            method: Some("Runtime.executionContextCreated".into()),
            params: Some(serde_json::json!({
                "executionContextId": ctx_id,
                "auxData": {
                    "frameId": frame_id,
                    "name": name,
                }
            })),
            result: None,
            error: None,
            session_id: Some(session_id.into()),
        }
    }

    fn detached_from_target_msg(session_id: &str, target_id: &str) -> RawMessage {
        RawMessage {
            id: None,
            method: Some("Browser.detachedFromTarget".into()),
            params: Some(serde_json::json!({
                "sessionId": session_id,
                "targetId": target_id,
            })),
            result: None,
            error: None,
            session_id: None,
        }
    }

    #[test]
    fn attach_before_newpage_reply_is_claimed_not_popup() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let g = reg.begin_new_page();
        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        assert_eq!(claimed.target_id, "t1");
        assert_eq!(claimed.main_frame_id.as_deref(), Some("f1"));

        drop(g);
        assert!(reg.take_popups().is_empty());
    }

    #[test]
    fn opener_target_is_popup_immediately() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let _g = reg.begin_new_page();
        in_tx
            .send(attached_to_target_msg("s2", "t2", "ctx-1", Some("t1")))
            .unwrap();
        in_tx.send(frame_attached_msg("s2", "f2", None)).unwrap();

        assert!(wait_for(
            || !reg.take_popups().is_empty(),
            Duration::from_millis(500)
        ));
    }

    #[test]
    fn held_target_becomes_popup_when_inflight_ends() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let g = reg.begin_new_page();
        in_tx
            .send(attached_to_target_msg("s3", "t3", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s3", "f3", None)).unwrap();

        assert!(wait_for(
            || reg.frame_ctx("s3", "f3").is_some(),
            Duration::from_millis(500)
        ));
        assert!(reg.take_popups().is_empty());

        drop(g);
        let popups = reg.take_popups();
        assert_eq!(popups.len(), 1);
        assert_eq!(popups[0].target_id, "t3");
    }

    #[test]
    fn exec_ctx_tracked_for_main_and_child_frames() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f2", Some("f1")))
            .unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f1", "ctx-f1", ""))
            .unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f2", "ctx-f2", ""))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        assert!(wait_for(
            || claimed.main_ctx.current().as_deref() == Some("ctx-f1"),
            Duration::from_millis(500)
        ));

        let f2_ctx = reg.frame_ctx("s1", "f2").expect("f2 ctx tracked");
        assert!(wait_for(
            || f2_ctx.current().as_deref() == Some("ctx-f2"),
            Duration::from_millis(500)
        ));
    }

    #[test]
    fn non_main_world_contexts_ignored() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f1", "ctx-util", "utility"))
            .unwrap();
        // Sentinel event to guarantee reader thread has processed the utility context
        in_tx
            .send(frame_attached_msg("s1", "f-sentinel", Some("f1")))
            .unwrap();
        in_tx
            .send(exec_context_created_msg(
                "s1",
                "f-sentinel",
                "ctx-sentinel",
                "",
            ))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        // Wait until sentinel has been processed
        assert!(wait_for(
            || reg
                .frame_ctx("s1", "f-sentinel")
                .and_then(|c| c.current())
                .as_deref()
                == Some("ctx-sentinel"),
            Duration::from_millis(500)
        ));

        // The utility context must be completely ignored by the main frame tracker
        assert_eq!(claimed.main_ctx.current(), None);
    }

    #[test]
    fn detach_removes_and_unregisters_listeners() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();

        let _claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        assert!(!reg.is_detached("t1"));
        assert_eq!(reg.page_count(), 1);

        in_tx.send(detached_from_target_msg("s1", "t1")).unwrap();
        assert!(wait_for(
            || reg.is_detached("t1") && reg.page_count() == 0,
            Duration::from_millis(500)
        ));

        assert_eq!(conn.event_handler_count_for("s1", "Page.frameAttached"), 0);
        assert_eq!(reg.page_count(), 0);
        assert!(reg.frame_ctx("s1", "f1").is_none());
    }

    #[test]
    fn other_context_ignored() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg_a = PageRegistry::install(&conn, "ctx-a", DialogPolicy::Manual);
        let reg_b = PageRegistry::install(&conn, "ctx-b", DialogPolicy::Manual);

        // 1. Attach with no browserContextId (must be adopted by neither)
        let mut msg_no_ctx = attached_to_target_msg("s_none", "t_none", "", None);
        if let Some(params) = msg_no_ctx.params.as_mut() {
            if let Some(ti) = params.get_mut("targetInfo") {
                ti.as_object_mut().unwrap().remove("browserContextId");
            }
        }
        in_tx.send(msg_no_ctx).unwrap();
        in_tx
            .send(frame_attached_msg("s_none", "f_none", None))
            .unwrap();

        // 2. Attach with ctx-a
        in_tx
            .send(attached_to_target_msg("s_a", "t_a", "ctx-a", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s_a", "f_a", None)).unwrap();

        assert!(wait_for(
            || reg_a.claim("t_a", Duration::from_millis(100)).is_ok(),
            Duration::from_millis(500)
        ));

        sync_reader(&conn, &in_tx);
        assert!(reg_a.take_popups().is_empty());
        assert!(reg_b.take_popups().is_empty());
        for reg in [&reg_a, &reg_b] {
            assert!(!reg
                .state
                .lock()
                .unwrap()
                .pages
                .values()
                .any(|p| p.target_id == "t_none"));
            let err = reg.claim("t_none", Duration::ZERO).unwrap_err();
            assert!(err.to_string().contains("unknown target t_none"));
        }
        assert!(reg_b.claim("t_a", Duration::ZERO).is_err());
    }

    #[test]
    fn c2_registry_and_handlers_freed_when_external_handles_dropped() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let initial_handlers = conn.handler_count();

        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        let weak_reg = Arc::downgrade(&reg);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", Some("opener")))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f1", "ctx-1", ""))
            .unwrap();

        assert!(wait_for(
            || reg
                .frame_ctx("s1", "f1")
                .and_then(|c| c.current())
                .is_some(),
            Duration::from_millis(500)
        ));

        // Drop external handles
        drop(reg);

        // Registry must be freed and connection handler count must return to pre-install value
        assert!(weak_reg.upgrade().is_none());
        assert_eq!(conn.handler_count(), initial_handlers);
    }

    #[test]
    fn c3_subframe_context_before_main_frame_attached_does_not_alias_to_main() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        // Child context arrives BEFORE frameAttached
        in_tx
            .send(exec_context_created_msg("s1", "f-child", "ctx-child", ""))
            .unwrap();
        // Main frameAttached arrives
        in_tx
            .send(frame_attached_msg("s1", "f-main", None))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        assert_ne!(
            claimed.main_ctx.current().as_deref(),
            Some("ctx-child"),
            "main context must not be aliased to child frame's context"
        );
        let child_ctx = reg.frame_ctx("s1", "f-child").expect("child tracked");
        assert_eq!(child_ctx.current().as_deref(), Some("ctx-child"));
    }

    #[test]
    fn c3_main_frame_context_before_frame_attached_is_preserved() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        // Main context arrives BEFORE frameAttached
        in_tx
            .send(exec_context_created_msg("s1", "f-main", "ctx-main", ""))
            .unwrap();
        // Main frameAttached arrives
        in_tx
            .send(frame_attached_msg("s1", "f-main", None))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        assert_eq!(claimed.main_ctx.current().as_deref(), Some("ctx-main"));
    }

    #[test]
    fn c3_child_and_main_contexts_both_orders() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f-main", "ctx-main", ""))
            .unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f-child", "ctx-child", ""))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-main", None))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-child", Some("f-main")))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        assert_eq!(claimed.main_ctx.current().as_deref(), Some("ctx-main"));
        let child_ctx = reg.frame_ctx("s1", "f-child").unwrap();
        assert_eq!(child_ctx.current().as_deref(), Some("ctx-child"));
    }

    #[test]
    fn c4_claim_rejects_already_claimed_or_taken_target() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        // 1. Claim twice:
        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();

        let claimed1 = reg.claim("t1", Duration::from_millis(500));
        assert!(claimed1.is_ok(), "first claim must succeed");
        let claimed2 = reg.claim("t1", Duration::from_millis(50));
        assert!(claimed2.is_err(), "second claim on same target must fail");

        // 2. take_popups() then claim(same):
        in_tx
            .send(attached_to_target_msg("s2", "t2", "ctx-1", Some("t1")))
            .unwrap();
        in_tx.send(frame_attached_msg("s2", "f2", None)).unwrap();

        assert!(wait_for(
            || !reg.take_popups().is_empty(),
            Duration::from_millis(500)
        ));

        let claim_after_take = reg.claim("t2", Duration::from_millis(50));
        assert!(
            claim_after_take.is_err(),
            "claim after take_popups must fail"
        );
    }

    #[test]
    fn c5_claim_returns_closed_when_connection_closes() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let reg_clone = Arc::clone(&reg);
        let start = Instant::now();
        let claim_handle =
            std::thread::spawn(move || reg_clone.claim("t-never", Duration::from_secs(30)));

        // Close the connection
        sync_reader(&conn, &in_tx);
        let _ = conn.close();

        let res = claim_handle.join().unwrap();
        let elapsed = start.elapsed();
        assert!(res.is_err(), "claim must return error on connection close");
        assert!(
            elapsed < Duration::from_secs(1),
            "claim must return promptly on close, elapsed: {elapsed:?}"
        );
    }

    #[test]
    fn c8_detach_between_take_popups_and_from_tracked_fails_promptly() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", Some("opener")))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f1", "ctx-1", ""))
            .unwrap();

        assert!(wait_for(
            || reg
                .frame_ctx("s1", "f1")
                .and_then(|c| c.current())
                .is_some(),
            Duration::from_millis(500)
        ));

        let mut popups = reg.take_popups();
        assert_eq!(popups.len(), 1);
        let popup = popups.remove(0);

        // Detach arrives BEFORE from_tracked is called / command sent
        in_tx.send(detached_from_target_msg("s1", "t1")).unwrap();
        assert!(wait_for(
            || reg.is_detached("t1"),
            Duration::from_millis(500)
        ));

        let mf = crate::api::MainFrame::from_tracked(popup, Arc::clone(&conn), Arc::clone(&reg))
            .unwrap();
        let start = Instant::now();
        let res = mf.evaluate("1 + 1", Duration::from_secs(5));
        let elapsed = start.elapsed();

        assert!(res.is_err(), "evaluate on detached session must fail");
        assert!(
            elapsed < Duration::from_secs(1),
            "command must fail promptly as closed, not time out; elapsed: {elapsed:?}"
        );
    }

    #[test]
    fn c9_concurrent_new_main_frame_calls_get_distinct_pages() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let barrier = Arc::new(std::sync::Barrier::new(3));

        let reg1 = Arc::clone(&reg);
        let b1 = Arc::clone(&barrier);
        let h1 = std::thread::spawn(move || {
            let g1 = reg1.begin_new_page();
            b1.wait();
            let page = reg1.claim("t1", Duration::from_millis(500)).unwrap();
            drop(g1);
            page
        });

        let reg2 = Arc::clone(&reg);
        let b2 = Arc::clone(&barrier);
        let h2 = std::thread::spawn(move || {
            let g2 = reg2.begin_new_page();
            b2.wait();
            let page = reg2.claim("t2", Duration::from_millis(500)).unwrap();
            drop(g2);
            page
        });

        // Main thread waits until both workers began new page
        barrier.wait();

        // Feed events for both pages
        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();

        in_tx
            .send(attached_to_target_msg("s2", "t2", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s2", "f2", None)).unwrap();

        let p1 = h1.join().unwrap();
        let p2 = h2.join().unwrap();

        assert_eq!(p1.target_id, "t1");
        assert_eq!(p2.target_id, "t2");
        assert_ne!(p1.session_id, p2.session_id);
        assert!(reg.take_popups().is_empty());
    }

    #[test]
    fn c9_no_opener_popup_attaching_during_inflight_becomes_popup_after_own_target_claimed() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let g = reg.begin_new_page();

        // 1. Own target attaches
        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();

        // 2. Unrelated no-opener popup attaches while inflight > 0
        in_tx
            .send(attached_to_target_msg("s2", "t2", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s2", "f2", None)).unwrap();

        // Claim own target
        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        assert_eq!(claimed.target_id, "t1");

        assert!(wait_for(
            || reg.frame_ctx("s2", "f2").is_some(),
            Duration::from_millis(500)
        ));

        // End in-flight
        drop(g);

        // Target t2 should now be a Popup
        let popups = reg.take_popups();
        assert_eq!(popups.len(), 1);
        assert_eq!(popups[0].target_id, "t2");
    }

    #[test]
    fn c9_detach_during_claim_returns_error() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        // Detach before main frame attached
        in_tx.send(detached_from_target_msg("s1", "t1")).unwrap();

        let start = Instant::now();
        let err = reg.claim("t1", Duration::from_millis(500)).unwrap_err();
        let elapsed = start.elapsed();
        assert_eq!(err.kind, crate::protocol::errors::ProtocolErrorKind::Closed);
        assert!(err.message.contains("detached before claim completed"));
        assert!(
            elapsed < Duration::from_millis(200),
            "must fail quickly on detach, elapsed: {elapsed:?}"
        );
    }

    #[test]
    fn c9_claim_timeout_waiting_for_type_page_attach() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        // Attach a non-page target to exercise skipped reporting
        let non_page = serde_json::json!({
            "type": "service_worker",
            "targetId": "sw-1",
            "browserContextId": "ctx-1",
            "url": "https://example.com/sw.js",
        });
        in_tx
            .send(RawMessage {
                id: None,
                method: Some("Browser.attachedToTarget".into()),
                params: Some(serde_json::json!({
                    "sessionId": "sw-session",
                    "targetInfo": non_page,
                })),
                result: None,
                error: None,
                session_id: None,
            })
            .unwrap();

        let err = reg.claim("t-never", Duration::from_millis(50)).unwrap_err();
        assert!(err
            .message
            .contains("timeout waiting for type=='page' attach"));
        assert!(err.message.contains("service_worker"));
    }

    #[test]
    fn c9_claim_timeout_waiting_for_main_frame_attach() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        // Never send frame_attached

        let err = reg.claim("t1", Duration::from_millis(500)).unwrap_err();
        assert!(err
            .message
            .contains("timeout waiting for main frame attach (no parentFrameId)"));
    }

    #[test]
    fn f1_abandoned_target_not_promoted_to_popup() {
        let (transport, in_tx) = TestTransport::new();
        let records = Arc::clone(&transport.records);
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let g = reg.begin_new_page();

        // Abandon target before it attaches
        reg.abandon_target("t-abandoned");

        // Now target attaches
        in_tx
            .send(attached_to_target_msg("s1", "t-abandoned", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();

        drop(g);
        let reader = sync_reader(&conn, &in_tx);
        assert!(
            wait_for(
                || records.lock().unwrap().iter().any(|(v, thread)| {
                    v["method"] == "Page.close" && v["sessionId"] == "s1" && *thread != reader
                }),
                Duration::from_secs(2)
            ),
            "abandoned session must be closed off the reader thread"
        );
        assert!(reg.take_popups().is_empty());
    }

    #[test]
    fn f2_frame_detached_removes_descendants() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-main", None))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-child", Some("f-main")))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-grandchild", Some("f-child")))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(claimed, Arc::clone(&conn), Arc::clone(&reg))
            .unwrap();

        assert!(wait_for(
            || mf.frames().len() == 3,
            Duration::from_millis(500)
        ));

        // Detach child frame -> child and grandchild must be removed
        in_tx.send(frame_detached_msg("s1", "f-child")).unwrap();
        assert!(wait_for(
            || mf.frames().len() == 1,
            Duration::from_millis(500)
        ));
        assert_eq!(mf.frames()[0].0, "f-main");
    }

    #[test]
    fn n1_mainframe_outlives_context_still_receives_exec_context_updates() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let g = reg.begin_new_page();
        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f1", "1", ""))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        drop(g);

        let mf = crate::api::MainFrame::from_tracked(claimed, Arc::clone(&conn), Arc::clone(&reg))
            .unwrap();
        assert!(wait_for(
            || mf.execution_context_handle().current() == Some("1".to_string()),
            Duration::from_millis(500)
        ));

        // Drop the external registry handle and root guards (simulating BrowserContext drop)
        drop(reg);

        // Send a navigation / new execution context
        in_tx
            .send(exec_context_created_msg("s1", "f1", "2", ""))
            .unwrap();

        assert!(wait_for(
            || mf.execution_context_handle().current() == Some("2".to_string()),
            Duration::from_millis(500)
        ));
    }

    #[test]
    fn n2_abandon_vs_attach_race_does_not_produce_phantom_popup() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        // Set hook to abandon "t-race" right in the gap before insert
        let reg_hook = Arc::downgrade(&reg);
        *reg.on_attach_before_insert.lock().unwrap() = Some(Box::new(move |tid| {
            if tid == "t-race" {
                reg_hook.upgrade().unwrap().abandon_target("t-race");
            }
        }));

        // Send attach for a popup
        in_tx
            .send(attached_to_target_msg(
                "s-race",
                "t-race",
                "ctx-1",
                Some("t-opener"),
            ))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s-race", "f-race", None))
            .unwrap();

        sync_reader(&conn, &in_tx);

        assert!(
            reg.take_popups().is_empty(),
            "target abandoned during attach gap must not become a popup"
        );
        assert_eq!(reg.page_count(), 0);
    }

    #[test]
    fn n3_tombstones_are_context_isolated_and_closed_pages_stay_closed() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        // Attach our page
        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();
        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(claimed, Arc::clone(&conn), Arc::clone(&reg))
            .unwrap();

        // Detach our page
        in_tx.send(detached_from_target_msg("s1", "t1")).unwrap();
        assert!(wait_for(|| mf.is_closed(), Duration::from_millis(500)));

        // Send >1024 detaches for targets from other contexts
        for i in 0..1050 {
            in_tx
                .send(detached_from_target_msg(
                    &format!("other-s-{i}"),
                    &format!("other-t-{i}"),
                ))
                .unwrap();
        }

        sync_reader(&conn, &in_tx);

        // Our page must STILL be closed!
        assert!(mf.is_closed());
        // And our tombstone was NOT evicted because other contexts' detaches were ignored
        assert!(reg.is_detached("t1"));
    }

    #[test]
    fn n4_abandon_target_does_not_close_claimed_page() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        assert_eq!(claimed.target_id, "t1");

        // Attempting to claim again fails as already claimed
        let second = reg.claim("t1", Duration::from_millis(50));
        assert!(second.is_err());
        assert!(second.unwrap_err().message.contains("already been claimed"));

        // Call abandon_target on the claimed target
        reg.abandon_target("t1");

        // The claimed page must NOT have been removed from tracking!
        assert_eq!(reg.page_count(), 1);
        let state = reg.state.lock().unwrap();
        assert!(!state.abandoned_targets.contains("t1"));
        assert!(state.pages.contains_key("s1"));
    }

    #[test]
    fn n5_failed_new_main_frame_does_not_leave_phantom_popup() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        let mut g = reg.begin_new_page();

        // While in-flight, a target attaches with no opener (e.g. from the failed attempt)
        in_tx
            .send(attached_to_target_msg("s1", "t-failed", "ctx-1", None))
            .unwrap();
        in_tx.send(frame_attached_msg("s1", "f1", None)).unwrap();

        assert!(wait_for(
            || reg.frame_ctx("s1", "f1").is_some(),
            Duration::from_millis(500)
        ));

        // Mark in-flight guard failed (as BrowserContext::new_main_frame does on error)
        g.mark_failed();
        drop(g);

        sync_reader(&conn, &in_tx);

        // Must not be promoted to Popup!
        assert!(reg.take_popups().is_empty());
        assert_eq!(reg.page_count(), 0);
    }

    #[test]
    fn n6_main_frame_detach_and_reattach_reuses_stable_main_ctx() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-main-1", None))
            .unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f-main-1", "10", ""))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(claimed, Arc::clone(&conn), Arc::clone(&reg))
            .unwrap();
        assert!(wait_for(
            || mf.execution_context_handle().current() == Some("10".to_string()),
            Duration::from_millis(500)
        ));

        // Detach main frame f-main-1
        in_tx.send(frame_detached_msg("s1", "f-main-1")).unwrap();

        // New main frame f-main-2 attaches
        in_tx
            .send(frame_attached_msg("s1", "f-main-2", None))
            .unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f-main-2", "20", ""))
            .unwrap();

        assert!(wait_for(
            || mf.execution_context_handle().current() == Some("20".to_string()),
            Duration::from_millis(500)
        ));
    }

    #[test]
    fn n7_late_exec_context_for_detached_frame_is_ignored() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-main", None))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-child", Some("f-main")))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(claimed, Arc::clone(&conn), Arc::clone(&reg))
            .unwrap();

        assert!(wait_for(
            || mf.frames().len() == 2,
            Duration::from_millis(500)
        ));

        // Detach child frame
        in_tx.send(frame_detached_msg("s1", "f-child")).unwrap();
        assert!(wait_for(
            || mf.frames().len() == 1,
            Duration::from_millis(500)
        ));

        // Late exec context created for detached child frame arrives
        in_tx
            .send(exec_context_created_msg("s1", "f-child", "99", ""))
            .unwrap();
        sync_reader(&conn, &in_tx);

        // Frame must NOT have been resurrected!
        assert_eq!(mf.frames().len(), 1);
        assert!(mf.frame_ctx("f-child").is_none());
    }

    #[test]
    fn n11_mainframe_frame_ctx_and_frames_unit_test() {
        let (transport, in_tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);

        in_tx
            .send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-main", None))
            .unwrap();
        in_tx
            .send(frame_attached_msg("s1", "f-sub", Some("f-main")))
            .unwrap();
        in_tx
            .send(exec_context_created_msg("s1", "f-sub", "sub-ctx-1", ""))
            .unwrap();

        let claimed = reg.claim("t1", Duration::from_millis(500)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(claimed, Arc::clone(&conn), Arc::clone(&reg))
            .unwrap();

        assert!(wait_for(
            || mf.frames().len() == 2,
            Duration::from_millis(500)
        ));

        assert!(wait_for(
            || mf.frame_ctx("f-sub").and_then(|c| c.current()) == Some("sub-ctx-1".to_string()),
            Duration::from_millis(500)
        ));
    }
    #[test]
    fn plain_evaluate_follows_main_frame_replacement_during_retry() {
        let (transport, tx) = TestTransport::new();
        let records = transport.records.clone();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "old", None)).unwrap();
        tx.send(exec_context_created_msg("s", "old", "old-ctx", ""))
            .unwrap();
        let tracked = reg.claim("t", Duration::from_secs(2)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(tracked, conn.clone(), reg.clone()).unwrap();
        sync_reader(&conn, &tx);
        std::thread::scope(|scope| {
            let evaluate = scope.spawn(|| mf.evaluate("42", Duration::from_secs(3)));
            let next = |context: &str| {
                assert!(wait_for(
                    || records
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(v, _)| v["method"] == "Runtime.evaluate"
                            && v["params"]["executionContextId"] == context),
                    Duration::from_secs(2)
                ));
                records
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(v, _)| {
                        v["method"] == "Runtime.evaluate"
                            && v["params"]["executionContextId"] == context
                    })
                    .unwrap()
                    .0["id"]
                    .as_i64()
                    .unwrap()
            };
            let id = next("old-ctx");
            tx.send(frame_detached_msg("s", "old")).unwrap();
            tx.send(frame_attached_msg("s", "new", None)).unwrap();
            tx.send(exec_context_created_msg("s", "new", "new-ctx", ""))
                .unwrap();
            tx.send(RawMessage {
                id: Some(id),
                method: None,
                params: None,
                result: None,
                error: Some(crate::protocol::types::ErrorData {
                    message: "Execution context was destroyed".into(),
                    data: None,
                }),
                session_id: Some("s".into()),
            })
            .unwrap();
            assert!(wait_for(
                || evaluate.is_finished()
                    || records
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(v, _)| v["method"] == "Runtime.evaluate"
                            && v["params"]["executionContextId"] == "new-ctx"),
                Duration::from_secs(2)
            ));
            if !evaluate.is_finished() {
                let id = next("new-ctx");
                tx.send(RawMessage {
                    id: Some(id),
                    method: None,
                    params: None,
                    result: Some(serde_json::json!({"result":{"value":42}})),
                    error: None,
                    session_id: Some("s".into()),
                })
                .unwrap();
            }
            assert_eq!(evaluate.join().unwrap().unwrap()["result"]["value"], 42);
        });
    }

    #[test]
    fn nested_frame_without_context_honors_remaining_budget() {
        let (transport, tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "main", None)).unwrap();
        tx.send(frame_attached_msg("s", "outer", Some("main")))
            .unwrap();
        tx.send(frame_attached_msg("s", "inner", Some("outer")))
            .unwrap();
        let tracked = reg.claim("t", Duration::from_secs(2)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(tracked, conn.clone(), reg.clone()).unwrap();
        sync_reader(&conn, &tx);
        let started = Instant::now();
        let err = mf
            .evaluate_in_frame("inner", "1", Duration::from_secs(1))
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_millis(2000),
            "{:?}",
            started.elapsed()
        );
        assert!(
            err.to_string()
                .contains("timed out waiting for execution context"),
            "{err}"
        );
    }

    #[test]
    fn r1_failure_is_remembered_across_entire_window() {
        let (transport, tx) = TestTransport::new();
        let records = transport.records.clone();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        let mut a = reg.begin_new_page();
        let b = reg.begin_new_page();
        a.mark_failed();
        tx.send(attached_to_target_msg(
            "s-failed", "t-failed", "ctx-1", None,
        ))
        .unwrap();
        tx.send(frame_attached_msg("s-failed", "f", None)).unwrap();
        sync_reader(&conn, &tx);
        drop(a);
        drop(b);
        assert!(reg.take_popups().is_empty());
        assert!(wait_for(
            || records
                .lock()
                .unwrap()
                .iter()
                .any(|(v, _)| v["method"] == "Page.close" && v["sessionId"] == "s-failed"),
            Duration::from_secs(2)
        ));
        let next = reg.begin_new_page();
        tx.send(attached_to_target_msg("s-next", "t-next", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s-next", "f-next", None))
            .unwrap();
        sync_reader(&conn, &tx);
        drop(next);
        assert_eq!(reg.take_popups()[0].target_id, "t-next");
    }

    #[test]
    fn r2_same_id_reattach_accepts_main_and_child_contexts() {
        let (transport, tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "main", None)).unwrap();
        let tracked = reg.claim("t", Duration::from_secs(2)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(tracked, conn.clone(), reg.clone()).unwrap();
        for (frame, parent) in [("main", None), ("child", Some("main"))] {
            tx.send(frame_attached_msg("s", frame, parent)).unwrap();
            tx.send(frame_detached_msg("s", frame)).unwrap();
            tx.send(frame_attached_msg("s", frame, parent)).unwrap();
            tx.send(exec_context_created_msg("s", frame, frame, ""))
                .unwrap();
            sync_reader(&conn, &tx);
            assert_eq!(
                mf.frame_ctx(frame).unwrap().current().as_deref(),
                Some(frame)
            );
            if parent.is_none() {
                assert_eq!(
                    mf.execution_context_handle().current().as_deref(),
                    Some(frame)
                );
            }
        }
    }

    #[test]
    fn r3_destroy_after_main_detach_clears_stable_tracker() {
        let (transport, tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "f", None)).unwrap();
        tx.send(exec_context_created_msg("s", "f", "old", ""))
            .unwrap();
        sync_reader(&conn, &tx);
        let page = reg.claim("t", Duration::ZERO).unwrap();
        assert_eq!(page.main_ctx.current().as_deref(), Some("old"));
        tx.send(frame_detached_msg("s", "f")).unwrap();
        tx.send(RawMessage {
            id: None,
            method: Some("Runtime.executionContextDestroyed".into()),
            params: Some(serde_json::json!({"executionContextId": "old"})),
            session_id: Some("s".into()),
            result: None,
            error: None,
        })
        .unwrap();
        sync_reader(&conn, &tx);
        assert_eq!(page.main_ctx.current(), None);
    }

    #[test]
    fn r4_replacement_frame_is_used_by_navigate() {
        let (transport, tx) = TestTransport::new();
        let records = transport.records.clone();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "f-main-1", None)).unwrap();
        let page = reg.claim("t", Duration::from_secs(2)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(page, conn.clone(), reg).unwrap();
        tx.send(frame_detached_msg("s", "f-main-1")).unwrap();
        tx.send(frame_attached_msg("s", "f-main-2", None)).unwrap();
        sync_reader(&conn, &tx);
        assert_eq!(mf.frame_id(), "f-main-2");
        let worker = std::thread::spawn(move || {
            mf.navigate("about:blank", Default::default(), Duration::from_secs(2))
                .unwrap();
            mf
        });
        assert!(wait_for(
            || records
                .lock()
                .unwrap()
                .iter()
                .any(|(v, _)| v["method"] == "Page.navigate"),
            Duration::from_secs(2)
        ));
        let request = records
            .lock()
            .unwrap()
            .iter()
            .find(|(v, _)| v["method"] == "Page.navigate")
            .unwrap()
            .0
            .clone();
        assert_eq!(request["params"]["frameId"], "f-main-2");
        tx.send(RawMessage {
            id: request["id"].as_i64(),
            result: Some(serde_json::json!({})),
            method: None,
            params: None,
            session_id: Some("s".into()),
            error: None,
        })
        .unwrap();
        let mf = worker.join().unwrap();
        tx.send(detached_from_target_msg("s", "t")).unwrap();
        sync_reader(&conn, &tx);
        assert_eq!(mf.frame_id(), "f-main-2");
        assert!(mf.is_closed());
    }

    #[test]
    fn r5_context_drop_preserves_root_cleanup_until_last_frame() {
        let (transport, tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let baseline = conn.handler_count();
        let context =
            crate::api::BrowserContext::new("ctx-1".into(), &conn.root_session(), conn.clone());
        let weak = Arc::downgrade(context.registry());
        tx.send(attached_to_target_msg("s1", "t1", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s1", "f1", None)).unwrap();
        tx.send(attached_to_target_msg("s2", "t2", "ctx-1", None))
            .unwrap();
        sync_reader(&conn, &tx);
        let page = context.registry().claim("t1", Duration::ZERO).unwrap();
        let mf =
            crate::api::MainFrame::from_tracked(page, conn.clone(), context.registry().clone())
                .unwrap();
        drop(context);
        tx.send(detached_from_target_msg("s2", "t2")).unwrap();
        sync_reader(&conn, &tx);
        assert_eq!(weak.upgrade().unwrap().page_count(), 1);
        assert!(weak.upgrade().unwrap().is_detached("t2"));
        drop(mf);
        assert!(weak.upgrade().is_none());
        assert_eq!(conn.handler_count(), baseline);
    }

    #[test]
    fn r7_abandoned_targets_are_bounded_and_pruned() {
        let (transport, tx) = TestTransport::new();
        let records = transport.records.clone();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        for i in 0..1100 {
            reg.abandon_target(&format!("t-{i}"));
        }
        assert_eq!(reg.state.lock().unwrap().abandoned_targets.set.len(), 1024);
        assert!(!reg.state.lock().unwrap().abandoned_targets.contains("t-0"));
        tx.send(detached_from_target_msg("s", "t-1099")).unwrap();
        sync_reader(&conn, &tx);
        reg.abandon_target("t-1099");
        assert!(!reg
            .state
            .lock()
            .unwrap()
            .abandoned_targets
            .contains("t-1099"));
        tx.send(attached_to_target_msg("s", "t-1098", "ctx-1", None))
            .unwrap();
        assert!(wait_for(
            || records
                .lock()
                .unwrap()
                .iter()
                .any(|(v, _)| v["method"] == "Page.close"),
            Duration::from_secs(2)
        ));
        let close = records
            .lock()
            .unwrap()
            .iter()
            .find(|(v, _)| v["method"] == "Page.close")
            .unwrap()
            .0
            .clone();
        tx.send(RawMessage {
            id: close["id"].as_i64(),
            result: Some(serde_json::json!({})),
            method: None,
            params: None,
            session_id: Some("s".into()),
            error: None,
        })
        .unwrap();
        assert!(wait_for(
            || !reg
                .state
                .lock()
                .unwrap()
                .abandoned_targets
                .contains("t-1098"),
            Duration::from_secs(2)
        ));
        tx.send(attached_to_target_msg("s-fail", "t-1097", "ctx-1", None))
            .unwrap();
        assert!(wait_for(
            || records
                .lock()
                .unwrap()
                .iter()
                .any(|(v, _)| v["sessionId"] == "s-fail" && v["method"] == "Page.close"),
            Duration::from_secs(2)
        ));
        let request = records
            .lock()
            .unwrap()
            .iter()
            .find(|(v, _)| v["sessionId"] == "s-fail")
            .unwrap()
            .0
            .clone();
        tx.send(RawMessage {
            id: request["id"].as_i64(),
            method: None,
            params: None,
            result: None,
            session_id: Some("s-fail".into()),
            error: Some(crate::protocol::types::ErrorData {
                message: "close failed".into(),
                data: None,
            }),
        })
        .unwrap();
        assert!(wait_for(
            || !reg
                .state
                .lock()
                .unwrap()
                .abandoned_targets
                .contains("t-1097"),
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn concurrent_real_context_creation_out_of_order() {
        let (transport, tx) = TestTransport::new();
        let records = transport.records.clone();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let context = Arc::new(crate::api::BrowserContext::new(
            "ctx-1".into(),
            &conn.root_session(),
            conn.clone(),
        ));
        let (ready_tx, ready_rx) = mpsc::channel();
        *context.registry().before_claim.lock().unwrap() = Some(Arc::new(move || {
            let (release_tx, release_rx) = mpsc::channel();
            ready_tx.send(release_tx).unwrap();
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("claim was not released");
        }));
        let a_ctx = context.clone();
        let a = std::thread::spawn(move || a_ctx.new_main_frame());
        assert!(wait_for(
            || records
                .lock()
                .unwrap()
                .iter()
                .filter(|(v, _)| v["method"] == "Browser.newPage")
                .count()
                == 1,
            Duration::from_secs(2)
        ));
        let b_ctx = context.clone();
        let b = std::thread::spawn(move || b_ctx.new_main_frame());
        assert!(wait_for(
            || records
                .lock()
                .unwrap()
                .iter()
                .filter(|(v, _)| v["method"] == "Browser.newPage")
                .count()
                == 2,
            Duration::from_secs(2)
        ));
        let requests: Vec<_> = records
            .lock()
            .unwrap()
            .iter()
            .filter(|(v, _)| v["method"] == "Browser.newPage")
            .map(|(v, _)| v.clone())
            .collect();
        for i in [1, 0] {
            tx.send(RawMessage {
                id: requests[i]["id"].as_i64(),
                result: Some(serde_json::json!({"targetId": format!("t{i}")})),
                method: None,
                params: None,
                session_id: None,
                error: None,
            })
            .unwrap();
        }
        let release_a = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first worker did not reach claim");
        let release_b = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second worker did not reach claim");
        // Both calls are held at claim while popup adoption runs.
        for i in [0, 1] {
            tx.send(attached_to_target_msg(
                &format!("s{i}"),
                &format!("t{i}"),
                "ctx-1",
                None,
            ))
            .unwrap();
        }
        for i in [1, 0] {
            tx.send(frame_attached_msg(&format!("s{i}"), &format!("f{i}"), None))
                .unwrap();
        }
        sync_reader(&conn, &tx);
        // Concurrent popup adoption must not steal either in-flight creation.
        assert!(context.take_popups().is_empty());
        release_a.send(()).unwrap();
        release_b.send(()).unwrap();
        assert_eq!(a.join().unwrap().unwrap().target_id(), "t0");
        assert_eq!(b.join().unwrap().unwrap().target_id(), "t1");
        assert!(context.take_popups().is_empty());
    }
    #[test]
    fn r10_from_tracked_rejects_missing_main_frame() {
        let (transport, tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "f", None)).unwrap();
        let mut page = reg.claim("t", Duration::from_secs(2)).unwrap();
        page.main_frame_id = None;
        let err = crate::api::MainFrame::from_tracked(page, conn, reg).unwrap_err();
        assert!(err.to_string().contains("no main frame"));
    }
    #[test]
    fn s1_navigate_keeps_request_frame_for_lifecycle_after_replacement() {
        let (transport, tx) = TestTransport::new();
        let records = transport.records.clone();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "f-old", None)).unwrap();
        let page = reg.claim("t", Duration::from_secs(2)).unwrap();
        let mf = crate::api::MainFrame::from_tracked(page, conn.clone(), reg).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = mf.navigate(
                "about:blank",
                crate::api::NavigateOptions {
                    wait_until: Some("load".into()),
                    ..Default::default()
                },
                Duration::from_secs(2),
            );
            done_tx.send(result).unwrap();
        });
        assert!(wait_for(
            || records
                .lock()
                .unwrap()
                .iter()
                .any(|(v, _)| v["method"] == "Page.navigate"),
            Duration::from_secs(2)
        ));
        let request = records
            .lock()
            .unwrap()
            .iter()
            .find(|(v, _)| v["method"] == "Page.navigate")
            .unwrap()
            .0
            .clone();
        assert_eq!(request["params"]["frameId"], "f-old");
        // The responder replaces the main frame after receiving the request,
        // before acknowledging it and delivering lifecycle events for that request.
        tx.send(frame_detached_msg("s", "f-old")).unwrap();
        tx.send(frame_attached_msg("s", "f-new", None)).unwrap();
        sync_reader(&conn, &tx);
        tx.send(RawMessage {
            id: request["id"].as_i64(),
            result: Some(serde_json::json!({"navigationId": "nav"})),
            method: None,
            params: None,
            session_id: Some("s".into()),
            error: None,
        })
        .unwrap();
        assert!(wait_for(
            || conn.event_handler_count_for("s", "Page.eventFired") == 1,
            Duration::from_secs(2)
        ));
        tx.send(RawMessage {
            id: None,
            result: None,
            method: Some("Page.eventFired".into()),
            params: Some(
                serde_json::json!({"frameId": request["params"]["frameId"], "name": "load"}),
            ),
            session_id: Some("s".into()),
            error: None,
        })
        .unwrap();
        let result = done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("navigate did not finish");
        worker.join().unwrap();
        assert!(
            result.is_ok(),
            "request and lifecycle filter must use the same frame: {result:?}"
        );
    }

    #[test]
    fn s2_duplicate_attach_drops_old_guards_outside_registry_lock() {
        let (transport, tx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let baseline = conn.handler_count();
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "f", None)).unwrap();
        sync_reader(&conn, &tx);
        let checked = Arc::new(AtomicBool::new(false));
        let checked_hook = checked.clone();
        let weak = Arc::downgrade(&reg);
        reg.state
            .lock()
            .unwrap()
            .pages
            .get_mut("s")
            .unwrap()
            .session_guards[0]
            .on_drop = Some(Box::new(move || {
            let reg = weak.upgrade().unwrap();
            assert!(
                reg.state.try_lock().is_ok(),
                "subscription guard dropped under registry lock"
            );
            checked_hook.store(true, Ordering::SeqCst);
        }));
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        tx.send(frame_attached_msg("s", "f", None)).unwrap();
        sync_reader(&conn, &tx);
        assert!(
            checked.load(Ordering::SeqCst),
            "old guard did not drop outside registry lock"
        );
        assert_eq!(reg.page_count(), 1);
        assert_eq!(conn.handler_count(), baseline + 8);
        assert_eq!(conn.event_handler_count_for("s", "Page.frameAttached"), 1);
        tx.send(detached_from_target_msg("s", "t")).unwrap();
        sync_reader(&conn, &tx);
        assert_eq!(reg.page_count(), 0);
        drop(reg);
        assert_eq!(conn.handler_count(), baseline);
    }
    #[test]
    fn s5_poisoned_registry_recovers_on_reader_and_close_completion() {
        let (transport, tx) = TestTransport::new();
        let records = transport.records.clone();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let reg = PageRegistry::install(&conn, "ctx-1", DialogPolicy::Manual);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = reg.state.lock().unwrap();
            panic!("poison registry state");
        }));
        let mut guard = reg.begin_new_page();
        guard.mark_failed();
        drop(guard);
        assert_eq!(reg.main_frame_id("missing"), None);
        reg.abandon_target("t");
        tx.send(attached_to_target_msg("s", "t", "ctx-1", None))
            .unwrap();
        sync_reader(&conn, &tx);
        assert!(wait_for(
            || records
                .lock()
                .unwrap()
                .iter()
                .any(|(v, _)| v["method"] == "Page.close"),
            Duration::from_secs(2)
        ));
        let close = records
            .lock()
            .unwrap()
            .iter()
            .find(|(v, _)| v["method"] == "Page.close")
            .unwrap()
            .0
            .clone();
        tx.send(RawMessage {
            id: close["id"].as_i64(),
            result: Some(serde_json::json!({})),
            session_id: Some("s".into()),
            method: None,
            params: None,
            error: None,
        })
        .unwrap();
        assert!(wait_for(
            || !reg
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .abandoned_targets
                .contains("t"),
            Duration::from_secs(2)
        ));
    }
}
