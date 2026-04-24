use std::fmt::{Debug, Display};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::AuthMethod;

/// Reason why an account is considered unusable for dispatch
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Error)]
pub enum Reason {
    Free,
    Disabled,
    Banned,
    Null,
    Restricted(i64),
    TooManyRequest(i64),
}

impl Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let format_time = |secs: i64| {
            chrono::DateTime::from_timestamp(secs, 0)
                .map(|t| t.format("UTC %Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or("Invalid date".to_string())
        };
        match self {
            Reason::Disabled => write!(f, "Organization disabled"),
            Reason::Free => write!(f, "Free-tier account"),
            Reason::Banned => write!(f, "Account banned"),
            Reason::Null => write!(f, "Account unavailable"),
            Reason::Restricted(i) => {
                write!(f, "Account restricted until {}", format_time(*i))
            }
            Reason::TooManyRequest(i) => {
                write!(f, "Account cooling down until {}", format_time(*i))
            }
        }
    }
}

/// Pool-side record for an account that is currently dispatch-ineligible.
///
/// Step 4 / C6 retired the embedded cookie blob — pre-C6 this struct
/// stored the full `ClewdrCookie` so `spawn_probe_*` and `collect_by_id`
/// could reconstruct an `AccountSlot` from in-memory residue. Post-C4
/// probes load credentials from DB by `account_id`; post-C5 release
/// guards drop stale runtime via `CredentialFingerprint`. Neither path
/// needs the bytes here any more, and keeping them was forcing
/// `pool_credential_fingerprint` to fall back to a half-typed
/// (cookie-only) identity for OAuth accounts in invalid.
///
/// `auth_method` stays so admin overview / `AccountHealth` can group
/// invalid accounts by kind without re-querying the DB.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InvalidAccountSlot {
    pub account_id: i64,
    pub auth_method: AuthMethod,
    pub reason: Reason,
}

impl InvalidAccountSlot {
    pub fn new(account_id: i64, auth_method: AuthMethod, reason: Reason) -> Self {
        Self {
            account_id,
            auth_method,
            reason,
        }
    }
}

impl Reason {
    pub fn from_db_string_checked(s: &str) -> Option<Self> {
        match s {
            "free" => Some(Reason::Free),
            "disabled" => Some(Reason::Disabled),
            "banned" => Some(Reason::Banned),
            "null" => Some(Reason::Null),
            other => {
                if let Some(ts) = other.strip_prefix("restricted:") {
                    Some(Reason::Restricted(ts.parse().unwrap_or(0)))
                } else {
                    other
                        .strip_prefix("too_many_request:")
                        .map(|ts| Reason::TooManyRequest(ts.parse().unwrap_or(0)))
                }
            }
        }
    }

    pub fn to_db_string(&self) -> String {
        match self {
            Reason::Free => "free".to_string(),
            Reason::Disabled => "disabled".to_string(),
            Reason::Banned => "banned".to_string(),
            Reason::Null => "null".to_string(),
            Reason::Restricted(ts) => format!("restricted:{ts}"),
            Reason::TooManyRequest(ts) => format!("too_many_request:{ts}"),
        }
    }

    pub fn from_db_string(s: &str) -> Self {
        Self::from_db_string_checked(s).unwrap_or(Reason::Null)
    }
}
