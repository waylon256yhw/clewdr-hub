use colored::Colorize;
use http::Method;
use serde_json::Value;
use snafu::ResultExt;

use super::ClaudeCodeState;
use crate::{
    config::Reason,
    error::{CheckClaudeErr, ClewdrError, WreqSnafu},
    utils::print_out_json,
};

pub struct BootstrapInfo {
    pub email: String,
    pub org_uuid: String,
    pub account_type: String,
    pub capabilities: Vec<String>,
    /// e.g. `default_claude_max_20x`, `default_claude_max_5x`,
    /// `default_claude_pro`. Refines `account_type` for Max users.
    pub rate_limit_tier: Option<String>,
    /// e.g. `google_play_subscription`, `stripe`. Informational.
    pub billing_type: Option<String>,
    /// Best-available proxy for "subscription start". The cookie
    /// bootstrap response does not expose `subscription_created_at`
    /// directly, so this falls back through `organization.created_at`
    /// → `selected_membership.created_at`. For personal Pro/Max
    /// accounts these typically match the real subscription start;
    /// see callers for the caveat.
    pub subscription_created_at: Option<String>,
}

fn derive_account_type(capabilities: &[String]) -> String {
    let has = |keyword: &str| capabilities.iter().any(|c| c.contains(keyword));
    if has("max") {
        "max".to_string()
    } else if has("enterprise") {
        "enterprise".to_string()
    } else if has("pro") || has("raven") {
        "pro".to_string()
    } else {
        "free".to_string()
    }
}

impl ClaudeCodeState {
    pub async fn fetch_bootstrap_info(&self) -> Result<BootstrapInfo, ClewdrError> {
        self.fetch_bootstrap_info_raw().await.map(|(info, _)| info)
    }

    /// Same as [`fetch_bootstrap_info`] but also returns the raw JSON body, so
    /// manual probes can persist it for debugging.
    pub async fn fetch_bootstrap_info_raw(&self) -> Result<(BootstrapInfo, Value), ClewdrError> {
        let end_point = self
            .endpoint
            .join("api/bootstrap")
            .expect("Url parse error");
        let res = self
            .build_request(Method::GET, end_point)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to bootstrap",
            })?
            .check_claude()
            .await?;
        let bootstrap = res.json::<Value>().await.context(WreqSnafu {
            msg: "Failed to parse bootstrap response",
        })?;
        print_out_json(&bootstrap, "bootstrap_res.json");
        let info = parse_bootstrap_info(&bootstrap)?;
        Ok((info, bootstrap))
    }

    pub async fn get_organization(&self) -> Result<String, ClewdrError> {
        let info = self.fetch_bootstrap_info().await?;
        if info.account_type == "free" {
            return Err(Reason::Free.into());
        }
        println!(
            "[{}]\nemail: {}\ncapabilities: {}",
            self.cookie.as_ref().unwrap().credential_label().green(),
            info.email.blue(),
            info.capabilities.join(", ").blue()
        );
        Ok(info.org_uuid)
    }
}

fn parse_bootstrap_info(bootstrap: &Value) -> Result<BootstrapInfo, ClewdrError> {
    if bootstrap["account"].is_null() {
        return Err(Reason::Null.into());
    }
    let memberships = bootstrap["account"]["memberships"]
        .as_array()
        .ok_or(Reason::Null)?;
    let selected_membership = memberships
        .iter()
        .find(|m| {
            m["organization"]["capabilities"]
                .as_array()
                .is_some_and(|c| c.iter().any(|c| c.as_str() == Some("chat")))
        })
        .ok_or(Reason::Null)?;
    let boot_acc_info = selected_membership["organization"]
        .as_object()
        .ok_or(Reason::Null)?;
    let capabilities = boot_acc_info["capabilities"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let email = bootstrap["account"]["email_address"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let uuid = boot_acc_info["uuid"]
        .as_str()
        .ok_or(ClewdrError::UnexpectedNone {
            msg: "Failed to get organization UUID",
        })?
        .to_string();
    let account_type = derive_account_type(&capabilities);

    let rate_limit_tier = boot_acc_info
        .get("rate_limit_tier")
        .and_then(Value::as_str)
        .map(str::to_string);
    let billing_type = boot_acc_info
        .get("billing_type")
        .and_then(Value::as_str)
        .map(str::to_string);
    // Cookie bootstrap has no `subscription_created_at`. Prefer the org
    // creation timestamp (closer to subscription start for personal
    // orgs), then fall back to the membership's own creation timestamp
    // before giving up.
    let subscription_created_at = boot_acc_info
        .get("subscription_created_at")
        .and_then(Value::as_str)
        .or_else(|| boot_acc_info.get("created_at").and_then(Value::as_str))
        .or_else(|| {
            selected_membership
                .get("created_at")
                .and_then(Value::as_str)
        })
        .map(str::to_string);

    Ok(BootstrapInfo {
        email,
        org_uuid: uuid,
        account_type,
        capabilities,
        rate_limit_tier,
        billing_type,
        subscription_created_at,
    })
}
