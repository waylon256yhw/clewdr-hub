mod request;

pub use request::*;

use std::sync::{Arc, Mutex};

use crate::types::claude::Usage;

/// Context carried through the request pipeline for Claude Code
#[derive(Debug, Clone)]
pub struct ClaudeContext {
    pub stream: bool,
    pub system_prompt_hash: Option<u64>,
    pub anthropic_beta: Option<String>,
    pub usage: Usage,
    pub user_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub max_concurrent: Option<i32>,
    pub rpm_limit: Option<i32>,
    /// Raw model string from client request (for billing)
    pub model_raw: String,
    /// Unique request ID (for billing/logging)
    pub request_id: String,
    /// Request start time (for billing duration)
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Weekly budget from policy (nanousd)
    pub weekly_budget_nanousd: Option<i64>,
    /// Monthly budget from policy (nanousd)
    pub monthly_budget_nanousd: Option<i64>,
    pub bound_account_ids: Vec<i64>,
    pub selected_account_id: Arc<Mutex<Option<i64>>>,
}

impl ClaudeContext {
    pub fn selected_account_id(&self) -> Option<i64> {
        self.selected_account_id
            .lock()
            .map(|slot| *slot)
            .unwrap_or(None)
    }

    pub fn set_selected_account_id(&self, account_id: Option<i64>) {
        if let Ok(mut slot) = self.selected_account_id.lock() {
            *slot = account_id;
        }
    }
}
