mod request;

pub use request::*;

use std::sync::{Arc, Mutex};

use crate::db::models::RequestAuditSnapshot;
use crate::types::claude::Usage;

/// Context carried through the request pipeline for Claude Code
#[derive(Debug, Clone)]
pub struct ClaudeContext {
    pub stream: bool,
    /// Client requested a non-stream response, but the response body should
    /// use the whitespace keepalive bridge while clewdr aggregates upstream
    /// SSE events.
    pub non_stream_keepalive: bool,
    pub non_stream_keepalive_interval_ms: u64,
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
    /// Per-request audit snapshot when the API key has enhanced audit
    /// enabled. `None` for non-audited keys; presence is the trigger
    /// for the sidecar write in the terminal-log transaction.
    pub audit: Option<RequestAuditSnapshot>,
    /// Inbound `X-Claude-Code-Session-Id` header, if a real Claude Code client
    /// sent one. Carried through to send time as the highest-priority seed for
    /// the outbound session id, so a client whose turns share a session (but
    /// differ in prompt) keep one stable outbound session. `None` for
    /// 2api/OpenAI-compat callers.
    pub inbound_session_id: Option<String>,
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
