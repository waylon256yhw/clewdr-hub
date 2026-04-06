use std::time::Duration;

use chrono::{DateTime, Utc};
use oauth2::{EmptyExtraTokenFields, StandardTokenResponse, TokenResponse, basic::BasicTokenType};
use serde::{Deserialize, Serialize};
use serde_with::{DurationSeconds, TimestampSecondsWithFrac, serde_as};
use tracing::debug;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]

pub struct Organization {
    pub uuid: String,
}

#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenInfo {
    pub access_token: String,
    #[serde_as(as = "DurationSeconds")]
    pub expires_in: Duration,
    pub organization: Organization,
    pub refresh_token: String,
    #[serde_as(as = "TimestampSecondsWithFrac")]
    pub expires_at: DateTime<Utc>,
}

impl TokenInfo {
    pub fn from_parts(
        access_token: String,
        refresh_token: String,
        expires_in: Duration,
        organization_uuid: String,
    ) -> Self {
        let expires_at = Utc::now() + expires_in;
        Self {
            access_token,
            expires_in,
            organization: Organization {
                uuid: organization_uuid,
            },
            refresh_token,
            expires_at,
        }
    }

    pub fn new(
        raw: StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>,
        organization_uuid: String,
    ) -> Self {
        Self::from_parts(
            raw.access_token().secret().to_string(),
            raw.refresh_token()
                .map_or_else(Default::default, |rt| rt.secret().to_string()),
            raw.expires_in().unwrap_or_default(),
            organization_uuid,
        )
    }

    pub fn is_expired(&self) -> bool {
        debug!("Expires at: {}", self.expires_at.to_rfc3339());
        Utc::now() >= self.expires_at - Duration::from_secs(60 * 5) // 5 minutes
    }
}
