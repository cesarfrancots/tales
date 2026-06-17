//! The agent abstraction — the heart of Tales.
//!
//! Every AI tool (Claude Code, Codex, future adapters) is an [`AgentAdapter`]
//! that produces a stream of normalized [`AgentEvent`]s and consumes
//! [`AgentCommand`]s. The differences between tools — Claude's bidirectional
//! stream-json vs Codex's turn-based exec/resume — are expressed only through
//! [`AgentCaps`], never by branching on the concrete type upstream.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{AgentId, Result};

pub mod claude;
pub mod codex;
pub mod mock;

/// Monotonic per-agent turn counter.
pub type TurnId = u64;

/// What an adapter can and cannot do. The engine schedules around these flags
/// instead of asking "is this Claude or Codex".
#[derive(Clone, Copy, Debug)]
pub struct AgentCaps {
    /// Can accept a new message *during* an in-flight turn (Claude: yes,
    /// Codex: no — injected messages are folded into the next turn).
    pub midturn_injection: bool,
    /// Supports schema-constrained structured output.
    pub structured_output: bool,
    /// Supports resuming a prior session by id.
    pub resumable: bool,
    /// Supports attaching MCP servers.
    pub mcp: bool,
}

/// Normalized inbound events: agent → orchestrator. The engine and every
/// frontend see only this enum.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// The underlying CLI is up; `native_session_id` is its own session handle
    /// (used for resume).
    SessionReady {
        agent: AgentId,
        native_session_id: String,
    },
    /// The skills / slash-commands this tool exposes (discovered at startup).
    Skills { agent: AgentId, skills: Vec<String> },
    /// A new turn began.
    TurnStarted { agent: AgentId, turn: TurnId },
    /// A streamed token chunk.
    TokenDelta {
        agent: AgentId,
        turn: TurnId,
        text: String,
    },
    /// The full text of a completed assistant message.
    MessageFinal {
        agent: AgentId,
        turn: TurnId,
        text: String,
    },
    /// The agent invoked a tool.
    ToolCall {
        agent: AgentId,
        turn: TurnId,
        name: String,
        input: Value,
    },
    /// The turn finished; `cost_usd` is cumulative reported cost if available.
    TurnComplete {
        agent: AgentId,
        turn: TurnId,
        cost_usd: Option<f64>,
    },
    /// A recoverable or fatal error from the agent.
    Error {
        agent: AgentId,
        message: String,
        fatal: bool,
    },
    /// The process exited.
    Exited { agent: AgentId, code: Option<i32> },
}

/// A media file (image or PDF) to attach to a turn so the agent can see it.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub path: PathBuf,
}

impl Attachment {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    /// Lowercased file extension (no dot), if any.
    pub fn ext(&self) -> String {
        self.path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
    }
    pub fn is_image(&self) -> bool {
        matches!(self.ext().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
    }
    pub fn is_pdf(&self) -> bool {
        self.ext() == "pdf"
    }
    /// File name for display.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string()
    }
}

/// Normalized outbound commands: orchestrator → agent.
#[derive(Clone, Debug)]
pub enum AgentCommand {
    /// Begin a new turn with `prompt` and any media attachments to share.
    StartTurn {
        prompt: String,
        attachments: Vec<Attachment>,
    },
    /// Inject a message. Honored live only if [`AgentCaps::midturn_injection`];
    /// otherwise the adapter queues it for the next turn.
    InjectMessage { text: String },
    /// Gracefully end the session (closes stdin → CLI exits).
    Shutdown,
}

/// Everything an adapter needs to launch a session.
#[derive(Clone, Debug)]
pub struct SpawnCtx {
    pub agent: AgentId,
    pub label: String,
    /// The working directory — in v1 this is the agent's own git worktree.
    pub cwd: PathBuf,
    /// Model alias/id, or `None` for the CLI default.
    pub model: Option<String>,
    /// Pre-authorized permission mode (Claude) so headless runs never hang on a
    /// prompt. Ignored by adapters that don't use it.
    pub permission_mode: String,
    /// Sandbox policy (Codex): `read-only` | `workspace-write` |
    /// `danger-full-access`. Ignored by adapters that don't use it.
    pub sandbox: String,
    /// If set, restricts the agent to these tools (Claude: `--allowedTools`).
    /// Used to keep an executor on non-blocking file tools (Write/Edit) so it
    /// can't hang on an unapproved Bash call in headless mode.
    pub allowed_tools: Option<Vec<String>>,
}

/// The uniform interface over every AI tool.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Static capabilities of this adapter.
    fn caps(&self) -> AgentCaps;

    /// Launch the underlying CLI. Spawns the supervision tasks that pump
    /// [`AgentEvent`]s into `events_tx`, and returns the command sender the
    /// orchestrator drives the agent with.
    async fn spawn(
        &mut self,
        ctx: SpawnCtx,
        events_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<mpsc::Sender<AgentCommand>>;
}
