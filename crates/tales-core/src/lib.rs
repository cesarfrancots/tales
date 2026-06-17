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
pub mod bus;
pub mod conductor;
pub mod error;
pub mod event;
pub mod orchestrator;
pub mod recommend;
pub mod supervisor;
pub mod worktree;

/// Stable identifier for a single agent session within a run.
pub type AgentId = uuid::Uuid;

pub use error::{Result, TalesError};
