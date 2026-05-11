use std::sync::atomic::{AtomicI64, Ordering};

use crate::protocol::types::MessageId;

// ---------------------------------------------------------------------------
// Connection-level state machine
// ---------------------------------------------------------------------------

/// Connection-level state.
///
/// Represents the lifecycle of the pipe connection to the Camoufox browser.
/// Transitions are strictly forward (no backwards transitions).
///
/// ```text
/// Connected ──► Enabling ──► Ready ──► Closing ──► Closed
///     │                        │                      ▲
///     └────────────────────────┴──────────────────────┘
///                     (abnormal close)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Transport is up, but `Browser.enable` has not been sent yet.
    Connected,
    /// `Browser.enable` has been sent, awaiting response.
    Enabling,
    /// `Browser.enable` succeeded. Normal operation — commands can be sent.
    Ready,
    /// `Browser.close` has been sent, awaiting graceful shutdown.
    Closing,
    /// Connection is fully closed (transport down). Terminal state.
    Closed,
}

impl ConnectionState {
    /// Returns `true` if the connection is in a terminal state.
    #[inline]
    pub fn is_closed(&self) -> bool {
        *self == ConnectionState::Closed
    }

    /// Returns `true` if normal commands can be sent (connection is `Ready`).
    #[inline]
    pub fn is_ready(&self) -> bool {
        *self == ConnectionState::Ready
    }

    /// Attempt a state transition. Returns `Ok(new_state)` on success,
    /// or `Err(message)` if the transition is invalid.
    ///
    /// Valid transitions:
    /// - `Connected → Enabling`
    /// - `Enabling → Ready`
    /// - `Ready → Closing`
    /// - `Closing → Closed`
    /// - `Connected → Closed` (abnormal: transport died before init)
    /// - `Enabling → Closed` (abnormal: transport died during enable)
    /// - `Ready → Closed` (abnormal: transport died during operation)
    pub fn transition(self, to: ConnectionState) -> Result<ConnectionState, String> {
        let valid = match (self, to) {
            // Normal forward transitions
            (ConnectionState::Connected, ConnectionState::Enabling) => true,
            (ConnectionState::Enabling, ConnectionState::Ready) => true,
            (ConnectionState::Ready, ConnectionState::Closing) => true,
            (ConnectionState::Closing, ConnectionState::Closed) => true,

            // Abnormal close from any non-terminal state
            (ConnectionState::Connected, ConnectionState::Closed) => true,
            (ConnectionState::Enabling, ConnectionState::Closed) => true,
            (ConnectionState::Ready, ConnectionState::Closed) => true,

            // Everything else is invalid
            _ => false,
        };

        if valid {
            Ok(to)
        } else {
            Err(format!(
                "invalid connection state transition: {self:?} → {to:?}"
            ))
        }
    }
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Connected => write!(f, "Connected"),
            ConnectionState::Enabling => write!(f, "Enabling"),
            ConnectionState::Ready => write!(f, "Ready"),
            ConnectionState::Closing => write!(f, "Closing"),
            ConnectionState::Closed => write!(f, "Closed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-session state machine
// ---------------------------------------------------------------------------

/// Per-session state.
///
/// Each page session (and the root session) tracks its own lifecycle state.
/// Used for pre-send guards: commands to crashed/disposed sessions are
/// rejected immediately without sending (PROTOCOL.md Section 14, items 16-17).
///
/// ```text
/// Active ──► Crashed
/// Active ──► Disposed
/// Crashed ──► Disposed
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Session is active and can accept commands.
    Active,
    /// Session has crashed (page crash). Commands are rejected with
    /// `ProtocolErrorKind::Crashed`. May still transition to `Disposed`.
    Crashed,
    /// Session is disposed (page closed, or connection closed). Terminal
    /// state. Commands are rejected with `ProtocolErrorKind::Closed`.
    Disposed,
}

impl SessionState {
    /// Returns `true` if commands can be sent on this session.
    #[inline]
    pub fn is_active(&self) -> bool {
        *self == SessionState::Active
    }

    /// Returns `true` if this is a terminal state (`Disposed`).
    #[inline]
    pub fn is_disposed(&self) -> bool {
        *self == SessionState::Disposed
    }

    /// Returns `true` if the session has crashed.
    #[inline]
    pub fn is_crashed(&self) -> bool {
        *self == SessionState::Crashed
    }

    /// Attempt a state transition. Returns `Ok(new_state)` on success,
    /// or `Err(message)` if the transition is invalid.
    ///
    /// Valid transitions:
    /// - `Active → Crashed`
    /// - `Active → Disposed`
    /// - `Crashed → Disposed` (crash followed by close)
    pub fn transition(self, to: SessionState) -> Result<SessionState, String> {
        let valid = matches!(
            (self, to),
            (SessionState::Active, SessionState::Crashed)
                | (SessionState::Active, SessionState::Disposed)
                | (SessionState::Crashed, SessionState::Disposed)
        );

        if valid {
            Ok(to)
        } else {
            Err(format!(
                "invalid session state transition: {self:?} → {to:?}"
            ))
        }
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionState::Active => write!(f, "Active"),
            SessionState::Crashed => write!(f, "Crashed"),
            SessionState::Disposed => write!(f, "Disposed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Global atomic message ID counter
// ---------------------------------------------------------------------------

/// Global atomic message ID counter.
///
/// Generates monotonically increasing message IDs starting at 1.
/// Thread-safe: uses `AtomicI64` with `Relaxed` ordering (sufficient
/// because we only need uniqueness, not happens-before relationships
/// with other memory operations).
///
/// Per PROTOCOL.md Section 1: IDs are globally monotonically increasing
/// across all sessions on a single connection. ID 0 is never used
/// (truthy check in Playwright). Negative IDs are reserved
/// (`-9999` for `Browser.close`).
pub struct IdGenerator {
    next: AtomicI64,
}

impl IdGenerator {
    /// Create a new ID generator. First call to `next()` returns 1.
    pub fn new() -> Self {
        Self {
            next: AtomicI64::new(1),
        }
    }

    /// Generate the next unique message ID.
    ///
    /// IDs start at 1 and increment by 1. This method is safe to call
    /// from multiple threads concurrently.
    pub fn next(&self) -> MessageId {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for IdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ConnectionState tests --

    #[test]
    fn connection_normal_lifecycle() {
        let state = ConnectionState::Connected;

        let state = state.transition(ConnectionState::Enabling).unwrap();
        assert_eq!(state, ConnectionState::Enabling);

        let state = state.transition(ConnectionState::Ready).unwrap();
        assert_eq!(state, ConnectionState::Ready);
        assert!(state.is_ready());

        let state = state.transition(ConnectionState::Closing).unwrap();
        assert_eq!(state, ConnectionState::Closing);

        let state = state.transition(ConnectionState::Closed).unwrap();
        assert_eq!(state, ConnectionState::Closed);
        assert!(state.is_closed());
    }

    #[test]
    fn connection_abnormal_close_from_connected() {
        let state = ConnectionState::Connected;
        let state = state.transition(ConnectionState::Closed).unwrap();
        assert_eq!(state, ConnectionState::Closed);
    }

    #[test]
    fn connection_abnormal_close_from_enabling() {
        let state = ConnectionState::Enabling;
        let state = state.transition(ConnectionState::Closed).unwrap();
        assert_eq!(state, ConnectionState::Closed);
    }

    #[test]
    fn connection_abnormal_close_from_ready() {
        let state = ConnectionState::Ready;
        let state = state.transition(ConnectionState::Closed).unwrap();
        assert_eq!(state, ConnectionState::Closed);
    }

    #[test]
    fn connection_invalid_backward_transition() {
        let result = ConnectionState::Ready.transition(ConnectionState::Connected);
        assert!(result.is_err());
    }

    #[test]
    fn connection_invalid_skip_transition() {
        let result = ConnectionState::Connected.transition(ConnectionState::Ready);
        assert!(result.is_err());
    }

    #[test]
    fn connection_closed_is_terminal() {
        // Cannot transition out of Closed
        assert!(ConnectionState::Closed
            .transition(ConnectionState::Connected)
            .is_err());
        assert!(ConnectionState::Closed
            .transition(ConnectionState::Ready)
            .is_err());
        assert!(ConnectionState::Closed
            .transition(ConnectionState::Closed)
            .is_err());
    }

    #[test]
    fn connection_cannot_skip_enabling() {
        // Connected cannot jump directly to Ready
        assert!(ConnectionState::Connected
            .transition(ConnectionState::Ready)
            .is_err());
    }

    #[test]
    fn connection_closing_cannot_go_back() {
        assert!(ConnectionState::Closing
            .transition(ConnectionState::Ready)
            .is_err());
        assert!(ConnectionState::Closing
            .transition(ConnectionState::Enabling)
            .is_err());
    }

    #[test]
    fn connection_display() {
        assert_eq!(format!("{}", ConnectionState::Connected), "Connected");
        assert_eq!(format!("{}", ConnectionState::Enabling), "Enabling");
        assert_eq!(format!("{}", ConnectionState::Ready), "Ready");
        assert_eq!(format!("{}", ConnectionState::Closing), "Closing");
        assert_eq!(format!("{}", ConnectionState::Closed), "Closed");
    }

    #[test]
    fn connection_helper_methods() {
        assert!(!ConnectionState::Connected.is_ready());
        assert!(!ConnectionState::Connected.is_closed());
        assert!(ConnectionState::Ready.is_ready());
        assert!(!ConnectionState::Ready.is_closed());
        assert!(ConnectionState::Closed.is_closed());
        assert!(!ConnectionState::Closed.is_ready());
    }

    // -- SessionState tests --

    #[test]
    fn session_active_to_crashed() {
        let state = SessionState::Active;
        let state = state.transition(SessionState::Crashed).unwrap();
        assert_eq!(state, SessionState::Crashed);
        assert!(state.is_crashed());
        assert!(!state.is_active());
        assert!(!state.is_disposed());
    }

    #[test]
    fn session_active_to_disposed() {
        let state = SessionState::Active;
        let state = state.transition(SessionState::Disposed).unwrap();
        assert_eq!(state, SessionState::Disposed);
        assert!(state.is_disposed());
    }

    #[test]
    fn session_crashed_to_disposed() {
        let state = SessionState::Crashed;
        let state = state.transition(SessionState::Disposed).unwrap();
        assert_eq!(state, SessionState::Disposed);
    }

    #[test]
    fn session_crashed_cannot_become_active() {
        assert!(SessionState::Crashed
            .transition(SessionState::Active)
            .is_err());
    }

    #[test]
    fn session_disposed_is_terminal() {
        assert!(SessionState::Disposed
            .transition(SessionState::Active)
            .is_err());
        assert!(SessionState::Disposed
            .transition(SessionState::Crashed)
            .is_err());
        assert!(SessionState::Disposed
            .transition(SessionState::Disposed)
            .is_err());
    }

    #[test]
    fn session_active_cannot_transition_to_active() {
        assert!(SessionState::Active
            .transition(SessionState::Active)
            .is_err());
    }

    #[test]
    fn session_display() {
        assert_eq!(format!("{}", SessionState::Active), "Active");
        assert_eq!(format!("{}", SessionState::Crashed), "Crashed");
        assert_eq!(format!("{}", SessionState::Disposed), "Disposed");
    }

    #[test]
    fn session_helper_methods() {
        assert!(SessionState::Active.is_active());
        assert!(!SessionState::Active.is_crashed());
        assert!(!SessionState::Active.is_disposed());

        assert!(!SessionState::Crashed.is_active());
        assert!(SessionState::Crashed.is_crashed());
        assert!(!SessionState::Crashed.is_disposed());

        assert!(!SessionState::Disposed.is_active());
        assert!(!SessionState::Disposed.is_crashed());
        assert!(SessionState::Disposed.is_disposed());
    }

    // -- IdGenerator tests --

    #[test]
    fn id_generator_starts_at_one() {
        let gen = IdGenerator::new();
        assert_eq!(gen.next(), 1);
    }

    #[test]
    fn id_generator_sequential() {
        let gen = IdGenerator::new();
        assert_eq!(gen.next(), 1);
        assert_eq!(gen.next(), 2);
        assert_eq!(gen.next(), 3);
        assert_eq!(gen.next(), 4);
        assert_eq!(gen.next(), 5);
    }

    #[test]
    fn id_generator_never_returns_zero() {
        let gen = IdGenerator::new();
        for _ in 0..100 {
            assert_ne!(gen.next(), 0);
        }
    }

    #[test]
    fn id_generator_unique_ids() {
        let gen = IdGenerator::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = gen.next();
            assert!(seen.insert(id), "duplicate ID: {id}");
        }
    }

    #[test]
    fn id_generator_thread_safety() {
        use std::sync::Arc;
        use std::thread;

        let gen = Arc::new(IdGenerator::new());
        let num_threads = 8;
        let ids_per_thread = 1000;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let gen = Arc::clone(&gen);
                thread::spawn(move || {
                    let mut ids = Vec::with_capacity(ids_per_thread);
                    for _ in 0..ids_per_thread {
                        ids.push(gen.next());
                    }
                    ids
                })
            })
            .collect();

        let mut all_ids = std::collections::HashSet::new();
        for handle in handles {
            let ids = handle.join().unwrap();
            for id in ids {
                assert!(id > 0, "ID must be positive, got {id}");
                assert!(all_ids.insert(id), "duplicate ID across threads: {id}");
            }
        }

        assert_eq!(all_ids.len(), num_threads * ids_per_thread);
    }

    #[test]
    fn id_generator_default() {
        let gen = IdGenerator::default();
        assert_eq!(gen.next(), 1);
    }
}
