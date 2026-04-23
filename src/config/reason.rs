use std::fmt::{Debug, Display};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::AccountSlot;
use crate::config::ClewdrCookie;

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

/// A struct representing a cookie that can't be used
/// Contains the cookie and the reason why it's considered unusable
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InvalidAccountSlot {
    pub cookie: ClewdrCookie,
    pub reason: Reason,
    pub account_id: i64,
}

impl PartialEq<AccountSlot> for InvalidAccountSlot {
    fn eq(&self, other: &AccountSlot) -> bool {
        self.cookie == other.cookie
    }
}

impl InvalidAccountSlot {
    pub fn new(cookie: ClewdrCookie, reason: Reason, account_id: i64) -> Self {
        Self {
            cookie,
            reason,
            account_id,
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
                } else if let Some(ts) = other.strip_prefix("too_many_request:") {
                    Some(Reason::TooManyRequest(ts.parse().unwrap_or(0)))
                } else {
                    None
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
