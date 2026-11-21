//! JavaScript dialog tracking and automatic handling policies.
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::main_frame::SubscriptionGuard;
use crate::protocol::client::{Connection, Session};
use crate::protocol::errors::ProtocolError;

/// Information supplied when a JavaScript dialog opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogInfo {
    /// Browser-assigned dialog ID.
    #[serde(alias = "dialogId")]
    pub id: String,
    /// Dialog type: alert, confirm, prompt, or beforeunload.
    #[serde(alias = "type")]
    pub kind: String,
    /// Dialog text.
    pub message: String,
    /// Default prompt text, if supplied.
    #[serde(alias = "defaultValue", default)]
    pub default_value: Option<String>,
}

/// How newly opened JavaScript dialogs are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogPolicy {
    /// Leave the dialog open for an explicit response.
    #[default]
    Manual,
    /// Automatically accept dialogs.
    Accept,
    /// Automatically dismiss dialogs.
    Dismiss,
}

pub(crate) struct DialogTracker {
    pending: Mutex<Option<DialogInfo>>,
    last: Mutex<Option<DialogInfo>>,
    policy: Mutex<DialogPolicy>,
}

impl DialogTracker {
    pub(crate) fn new(policy: DialogPolicy) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(None),
            last: Mutex::new(None),
            policy: Mutex::new(policy),
        })
    }
    pub fn pending(&self) -> Option<DialogInfo> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn last(&self) -> Option<DialogInfo> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn set_policy(&self, policy: DialogPolicy) {
        *self
            .policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;
    }
    pub(crate) fn clear(&self, id: &str) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.as_ref().is_some_and(|d| d.id == id) {
            *self
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = pending.take();
        }
    }
    /// Install listeners; callers must retain the returned guards.
    pub(crate) fn install(
        self: &Arc<Self>,
        conn: &Arc<Connection>,
        session: &Session,
    ) -> Vec<SubscriptionGuard> {
        let tracker = Arc::clone(self);
        let weak = Arc::downgrade(conn);
        let key = session.key().to_owned();
        let handler_key = key.clone();
        let opened = conn.on_event(
            &key,
            "Page.dialogOpened",
            Arc::new(move |event| {
                let Ok(info) = serde_json::from_value::<DialogInfo>(event.params.clone()) else {
                    return;
                };
                *tracker
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(info.clone());
                let policy = *tracker
                    .policy
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match policy {
                    DialogPolicy::Manual => {
                        if let Some(conn) = weak.upgrade() {
                            conn.reject_session_pending(&handler_key, |method| {
                                ProtocolError::dialog_opened(Some(method.to_owned()), &info)
                            });
                        }
                    }
                    DialogPolicy::Accept | DialogPolicy::Dismiss => {
                        let Some(conn) = weak.upgrade() else {
                            return;
                        };
                        let session = conn.session_handle(handler_key.clone());
                        std::thread::spawn(move || {
                            let accept = policy == DialogPolicy::Accept;
                            let mut params = json!({"dialogId": info.id, "accept": accept});
                            if accept && info.kind == "prompt" {
                                params["promptText"] =
                                    json!(info.default_value.unwrap_or_default());
                            }
                            session.send_may_fail("Page.handleDialog", params);
                        });
                    }
                }
            }),
        );
        let tracker = Arc::clone(self);
        let closed = conn.on_event(
            &key,
            "Page.dialogClosed",
            Arc::new(move |event| {
                if let Some(id) = event.params.get("dialogId").and_then(|v| v.as_str()) {
                    tracker.clear(id);
                }
            }),
        );
        vec![
            SubscriptionGuard::session(Arc::clone(conn), &key, "Page.dialogOpened", opened),
            SubscriptionGuard::session(Arc::clone(conn), &key, "Page.dialogClosed", closed),
        ]
    }
}
