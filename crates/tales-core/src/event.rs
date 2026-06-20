//! The UI-agnostic bus contract.
//!
//! Frontends (the ratatui TUI today, a web dashboard tomorrow) subscribe to
//! [`OrchestratorEvent`]s and send [`UserCommand`]s. These are the *only* types
//! that cross the core ↔ frontend boundary. Events fan out over a lossy
//! broadcast channel so a stalled UI can never block the engine; commands fan
//! in over an mpsc so all frontends are interchangeable producers.

use serde_json::Value;

use crate::{AgentId, TokenUsage};

/// Everything the core tells the outside world. Frontends render these.
#[derive(Clone, Debug)]
pub enum OrchestratorEvent {
    /// An agent process came up and is ready to take turns.
    AgentSpawned {
        agent: AgentId,
        label: String,
        session_id: String,
    },
    /// A streamed token chunk from an agent (the live "console" feed).
    Token { agent: AgentId, text: String },
    /// An agent's turn is starting (carries the agent id + its role). Frontends
    /// use this to show a per-agent "thinking" placeholder keyed by id, which
    /// correctly disambiguates two agents that happen to share a label.
    TurnStarted { agent: AgentId, role: String },
    /// A completed assistant message (full text of one turn).
    Message { agent: AgentId, text: String },
    /// A message the human typed into the chat (human-in-the-loop).
    UserMessage { text: String },
    /// An agent invoked a tool (edit, bash, …).
    ToolActivity { agent: AgentId, summary: String },
    /// An agent finished a turn. `cost_usd` is the session-cumulative cost as
    /// reported by the tool (Claude), or `None` when the tool reports no machine
    /// cost (Codex, Open Code) — so it is a running total, not a per-turn delta.
    /// `token_usage` is provider-reported usage for this turn when available.
    TurnComplete {
        agent: AgentId,
        cost_usd: Option<f64>,
        token_usage: Option<TokenUsage>,
    },
    /// An agent process exited.
    AgentExited { agent: AgentId, code: Option<i32> },
    /// The discussion phase changed (planning → recommending → …).
    PhaseChanged { phase: String },
    /// The agents recommended an executor; awaiting the user's decision.
    RecommendationReady {
        executor: String,
        rationale: String,
        confident: bool,
        scores: Vec<(String, f32)>,
    },
    /// Compact execution handoff/audit packet assembled after the gate opens.
    ExecutionPacket {
        executor: String,
        text: String,
        included_in_prompt: bool,
    },
    /// Deterministic local run report for handoff/replay. This is generated
    /// without model calls, so it is cheap to replay or copy into a follow-up.
    SessionReport { markdown: String, summary: Value },
    /// The engine is blocked waiting for the user to confirm/override/reject.
    AwaitingConfirmation { prompt: String },
    /// Diagnostic log line for an in-UI log pane.
    Log { level: String, msg: String },
    /// Unrecoverable error.
    Fatal { msg: String },
}

/// Everything a frontend can ask the core to do.
#[derive(Clone, Debug)]
pub enum UserCommand {
    /// Start a fresh turn for `agent` with `prompt`.
    StartTurn { agent: AgentId, prompt: String },
    /// User joins the discussion — routed live (Claude) or at the next turn
    /// boundary (Codex), depending on the agent's capabilities. `attachments`
    /// are media file paths to share with every connected agent.
    InjectNote {
        agent: AgentId,
        text: String,
        attachments: Vec<std::path::PathBuf>,
    },
    /// Release the execution gate: run the plan with the chosen executor. This
    /// is the ONLY way the engine may enter the executing phase.
    ConfirmExecution { executor: String },
    /// Decline to execute the plan.
    Reject,
    /// Tear everything down.
    Shutdown,
}
