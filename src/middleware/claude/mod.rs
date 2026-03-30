mod request;

pub use request::*;

use crate::types::claude::Usage;

/// Context carried through the request pipeline for Claude Code
#[derive(Debug, Clone)]
pub struct ClaudeContext {
    /// Whether the response should be streamed
    pub stream: bool,
    /// The hash of the system messages for caching purposes
    pub system_prompt_hash: Option<u64>,
    /// Optional anthropic-beta header forwarded from client request
    pub anthropic_beta: Option<String>,
    /// Usage information for the request
    pub usage: Usage,
}
