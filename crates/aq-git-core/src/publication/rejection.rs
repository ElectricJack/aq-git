//! Bounded, typed classification of a rejected target acknowledgement.
//!
//! Only the fixed reason text of the one porcelain line that acknowledges the
//! exact requested target is consulted. Standard error, hook output and every
//! other reference line are never inspected, and no remote text is copied into
//! the classification.
use serde::{Deserialize, Serialize};

/// Capability token advertised once new `remote_rejected` results carry `rejection_class`.
pub const CLASSIFICATION_CAPABILITY: &str = "publication_rejection_classification_v1";

/// Closed v1 set. These describe the acknowledgement text only, not the
/// underlying policy, credential or permission state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionClass {
    Unknown,
    NonFastForward,
    StaleLease,
    HookDeclined,
}

impl RejectionClass {
    pub const ALL: [&'static str; 4] = [
        "unknown",
        "non_fast_forward",
        "stale_lease",
        "hook_declined",
    ];

    /// Classifies the third porcelain field. Anything outside the fixed allowlist is `Unknown`.
    pub fn from_summary(summary: &[u8]) -> Self {
        match summary {
            b"[rejected] (non-fast-forward)" => Self::NonFastForward,
            b"[rejected] (stale info)" => Self::StaleLease,
            b"[remote rejected] (hook declined)"
            | b"[remote rejected] (pre-receive hook declined)" => Self::HookDeclined,
            _ => Self::Unknown,
        }
    }
}
