use std::{
    fmt::{Debug, Display},
    hash::Hash,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::CookieStatus;
use crate::config::ClewdrCookie;

/// Reason why a cookie is considered useless
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Error)]
pub enum Reason {
    NormalPro,
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
            Reason::NormalPro => write!(f, "Normal Pro account"),
            Reason::Disabled => write!(f, "Organization Disabled"),
            Reason::Free => write!(f, "Free account"),
            Reason::Banned => write!(f, "Banned"),
            Reason::Null => write!(f, "Null"),
            Reason::Restricted(i) => {
                write!(f, "Restricted/Warning: until {}", format_time(*i))
            }
            Reason::TooManyRequest(i) => {
                write!(f, "429 Too many request: until {}", format_time(*i))
            }
        }
    }
}

/// A struct representing a cookie that can't be used
/// Contains the cookie and the reason why it's considered unusable
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UselessCookie {
    pub cookie: ClewdrCookie,
    pub reason: Reason,
    #[serde(default)]
    pub account_id: Option<i64>,
}

impl PartialEq<CookieStatus> for UselessCookie {
    fn eq(&self, other: &CookieStatus) -> bool {
        self.cookie == other.cookie
    }
}

impl PartialEq for UselessCookie {
    fn eq(&self, other: &Self) -> bool {
        self.cookie == other.cookie
    }
}

impl Eq for UselessCookie {}

impl Hash for UselessCookie {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.cookie.hash(state);
    }
}

impl UselessCookie {
    pub fn new(cookie: ClewdrCookie, reason: Reason) -> Self {
        Self {
            cookie,
            reason,
            account_id: None,
        }
    }

    pub fn with_account_id(cookie: ClewdrCookie, reason: Reason, account_id: Option<i64>) -> Self {
        Self {
            cookie,
            reason,
            account_id,
        }
    }
}

impl Reason {
    pub fn to_db_string(&self) -> String {
        match self {
            Reason::NormalPro => "normal_pro".to_string(),
            Reason::Free => "free".to_string(),
            Reason::Disabled => "disabled".to_string(),
            Reason::Banned => "banned".to_string(),
            Reason::Null => "null".to_string(),
            Reason::Restricted(ts) => format!("restricted:{ts}"),
            Reason::TooManyRequest(ts) => format!("too_many_request:{ts}"),
        }
    }

    pub fn from_db_string(s: &str) -> Self {
        match s {
            "normal_pro" => Reason::NormalPro,
            "free" => Reason::Free,
            "disabled" => Reason::Disabled,
            "banned" => Reason::Banned,
            "null" => Reason::Null,
            other => {
                if let Some(ts) = other.strip_prefix("restricted:") {
                    Reason::Restricted(ts.parse().unwrap_or(0))
                } else if let Some(ts) = other.strip_prefix("too_many_request:") {
                    Reason::TooManyRequest(ts.parse().unwrap_or(0))
                } else {
                    Reason::Null
                }
            }
        }
    }
}
