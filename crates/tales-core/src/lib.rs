//! `tales-core` — the UI-agnostic orchestration core.
//!
//! Nothing in this crate may depend on a frontend (ratatui, axum, …). Frontends
//! talk to the core *only* through the bus types in [`event`] and [`bus`]. The
//! one abstraction everything hangs on lives in [`agent`]: every AI tool is an
//! [`agent::AgentAdapter`] that emits normalized [`agent::AgentEvent`]s and
//! consumes [`agent::AgentCommand`]s, so the engine never branches on *which*
//! CLI it is talking to — only on its [`agent::AgentCaps`].

pub mod agent;
pub mod blackboard;
pub mod build_info;
pub mod bus;
pub mod conductor;
pub mod coordinator;
pub mod dataset;
pub mod error;
pub mod eval_harness;
pub mod event;
#[cfg(feature = "llm-conductor")]
pub mod llm_conductor;
pub mod orchestrator;
pub mod project_context;
pub mod prompt_forecast;
pub mod recommend;
pub mod session;
pub mod supervisor;
pub mod tool_recommendation;
pub mod trace;
pub mod verify;
pub mod workspace_profile;
pub mod worktree;

/// Stable identifier for a single agent session within a run.
pub type AgentId = uuid::Uuid;

/// Provider-reported token usage for one or more completed turns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.reasoning_output_tokens.is_none()
            && self.total_tokens.is_none()
    }

    pub fn add(&mut self, other: TokenUsage) {
        self.input_tokens = add_optional_u64(self.input_tokens, other.input_tokens);
        self.cached_input_tokens =
            add_optional_u64(self.cached_input_tokens, other.cached_input_tokens);
        self.output_tokens = add_optional_u64(self.output_tokens, other.output_tokens);
        self.reasoning_output_tokens =
            add_optional_u64(self.reasoning_output_tokens, other.reasoning_output_tokens);
        self.total_tokens = add_optional_u64(self.total_tokens, other.total_tokens);
    }

    pub fn total_or_sum(&self) -> Option<u64> {
        self.total_tokens.or_else(|| {
            let parts = [
                self.input_tokens,
                self.output_tokens,
                self.reasoning_output_tokens,
            ];
            let mut total = 0u64;
            let mut seen = false;
            for value in parts.into_iter().flatten() {
                total = total.saturating_add(value);
                seen = true;
            }
            seen.then_some(total)
        })
    }
}

fn add_optional_u64(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

pub use error::{Result, TalesError};
