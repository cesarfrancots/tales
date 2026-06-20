//! Generic CLI adapter — the zero-code path for hooking a new tool.
//!
//! Many AI coding CLIs share the same minimal shape: run them once with a
//! prompt, they print the assistant's reply to stdout, then exit. Rather than
//! write a bespoke adapter per tool, any such CLI rides this one adapter,
//! configured entirely from its [`ToolInfo`](super::ToolInfo) registry row:
//!
//! ```text
//! <bin> <run_args...> [<model_flag> <model>]  <prompt>          (prompt_flag = "")
//! <bin> <run_args...> [<model_flag> <model>]  <prompt_flag> <prompt>
//! ```
//!
//! So adding a tool like Gemini is a *data* change (one row), not new Rust.
//!
//! It is **stateless**: each turn spawns a fresh child with no cross-turn
//! session continuity (unlike Codex's `resume` or Open Code's `--continue`,
//! which we can't assume an unknown CLI supports). That makes it an excellent
//! *executor* — the executor runs exactly one turn — and a per-turn participant
//! in discussions, where injected notes fold into the next prompt. Because there
//! is no shared session keyed on the directory, two generic agents are safe to
//! co-reside (see [`super::coresident_safe`]).
//!
//! `midturn_injection = false`: an [`AgentCommand::InjectMessage`] arriving
//! between turns is queued and folded into the next `StartTurn`, exactly as for
//! Codex and Open Code.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{AgentAdapter, AgentCaps, AgentCommand, AgentEvent, SpawnCtx, ToolInfo, TurnId};
use crate::AgentId;
use crate::Result;

/// How to invoke one turn of a generic CLI. Copied out of the registry row so
/// the adapter owns no borrowed `'static` data and stays cheap to clone/move.
#[derive(Clone, Debug)]
pub struct GenericSpec {
    pub bin: String,
    /// Fixed args before the model flag / prompt (e.g. `["run"]`).
    pub run_args: Vec<String>,
    /// Flag that selects a model, or empty for "no model flag".
    pub model_flag: String,
    /// Flag the prompt is passed with, or empty for "trailing positional".
    pub prompt_flag: String,
}

/// Adapter for any turn-based, stdout-printing CLI, driven by a [`GenericSpec`].
pub struct GenericAdapter {
    spec: GenericSpec,
}

impl GenericAdapter {
    pub fn new(spec: GenericSpec) -> Self {
        Self { spec }
    }

    /// Build the adapter from a registry row.
    pub fn from_info(info: &ToolInfo) -> Self {
        Self::new(GenericSpec {
            bin: info.bin.to_string(),
            run_args: info.run_args.iter().map(|s| s.to_string()).collect(),
            model_flag: info.model_flag.to_string(),
            prompt_flag: info.prompt_flag.to_string(),
        })
    }
}

#[async_trait]
impl AgentAdapter for GenericAdapter {
    fn caps(&self) -> AgentCaps {
        AgentCaps {
            midturn_injection: false, // turn-based: injected notes fold into next turn
            structured_output: false, // plain stdout, no schema mode
            resumable: false,         // stateless — every turn is a fresh process
            mcp: false,               // unknown CLI; assume nothing
        }
    }

    async fn spawn(
        &mut self,
        ctx: SpawnCtx,
        events_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<mpsc::Sender<AgentCommand>> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AgentCommand>(64);
        let mgr = Manager {
            spec: self.spec.clone(),
            ctx,
            events_tx,
            turn: 0,
            pending: Vec::new(),
        };
        tokio::spawn(mgr.run(cmd_rx));
        Ok(cmd_tx)
    }
}

/// Owns per-session state and drives one child process per turn.
struct Manager {
    spec: GenericSpec,
    ctx: SpawnCtx,
    events_tx: mpsc::Sender<AgentEvent>,
    turn: TurnId,
    /// Notes injected between turns, folded into the next StartTurn prompt.
    pending: Vec<String>,
}

impl Manager {
    async fn run(mut self, mut cmd_rx: mpsc::Receiver<AgentCommand>) {
        let mut last_code: Option<i32> = Some(0);
        while let Some(command) = cmd_rx.recv().await {
            match command {
                AgentCommand::StartTurn { prompt, .. } => {
                    // Attachments aren't passed through an unknown CLI; the
                    // prompt text already names every shared file.
                    let full = self.fold_prompt(prompt);
                    last_code = self.run_turn(full).await;
                }
                AgentCommand::InjectMessage { text } => {
                    self.pending.push(text);
                }
                AgentCommand::Shutdown => break,
            }
        }
        let _ = self
            .events_tx
            .send(AgentEvent::Exited {
                agent: self.ctx.agent,
                code: last_code,
            })
            .await;
    }

    /// Prepend any queued injected notes to the turn prompt.
    fn fold_prompt(&mut self, prompt: String) -> String {
        if self.pending.is_empty() {
            return prompt;
        }
        let mut buf = String::new();
        for note in self.pending.drain(..) {
            buf.push_str("[note from orchestrator] ");
            buf.push_str(&note);
            buf.push_str("\n\n");
        }
        buf.push_str(&prompt);
        buf
    }

    /// Assemble argv for one turn: `run_args`, then an optional `model_flag
    /// model`, then the prompt (trailing positional, or behind `prompt_flag`).
    fn build_args(&self, prompt: &str) -> Vec<String> {
        let mut args: Vec<String> = self.spec.run_args.clone();
        if let Some(model) = &self.ctx.model {
            if !self.spec.model_flag.is_empty() {
                args.push(self.spec.model_flag.clone());
                args.push(model.clone());
            }
        }
        if self.spec.prompt_flag.is_empty() {
            args.push(prompt.to_string());
        } else {
            args.push(self.spec.prompt_flag.clone());
            args.push(prompt.to_string());
        }
        args
    }

    /// Spawn one child, stream its stdout to completion, reap it.
    async fn run_turn(&mut self, prompt: String) -> Option<i32> {
        let agent = self.ctx.agent;
        self.turn += 1;
        let turn = self.turn;
        let args = self.build_args(&prompt);

        let mut child = match Command::new(&self.spec.bin)
            .args(&args)
            .current_dir(PathBuf::from(&self.ctx.cwd))
            .stdin(Stdio::null()) // non-interactive; never block reading stdin
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = self
                    .events_tx
                    .send(AgentEvent::Error {
                        agent,
                        message: format!("failed to spawn {}: {e}", self.spec.bin),
                        fatal: true,
                    })
                    .await;
                return None;
            }
        };

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let bin = self.spec.bin.clone();
        tokio::spawn(stderr_task(stderr, agent, bin));

        let _ = self
            .events_tx
            .send(AgentEvent::TurnStarted { agent, turn })
            .await;

        // Stream stdout line-by-line as token deltas; accumulate the full reply.
        let mut lines = BufReader::new(stdout).lines();
        let mut full = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            full.push_str(&line);
            full.push('\n');
            let _ = self
                .events_tx
                .send(AgentEvent::TokenDelta {
                    agent,
                    turn,
                    text: format!("{line}\n"),
                })
                .await;
        }

        let trimmed = full.trim();
        if !trimmed.is_empty() {
            let _ = self
                .events_tx
                .send(AgentEvent::MessageFinal {
                    agent,
                    turn,
                    text: trimmed.to_string(),
                })
                .await;
        }
        // Generic CLIs report no machine cost; always close the turn so the
        // orchestrator's turn loop never deadlocks waiting for a terminal event.
        let _ = self
            .events_tx
            .send(AgentEvent::TurnComplete {
                agent,
                turn,
                cost_usd: None,
                token_usage: None,
            })
            .await;

        child.wait().await.ok().and_then(|s| s.code())
    }
}

async fn stderr_task(stderr: tokio::process::ChildStderr, agent: AgentId, bin: String) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            tracing::warn!(%agent, "{bin} stderr: {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn mgr(spec: GenericSpec) -> Manager {
        let (tx, _rx) = mpsc::channel(8);
        Manager {
            spec,
            ctx: SpawnCtx {
                agent: Uuid::new_v4(),
                label: "generic".into(),
                cwd: PathBuf::from("/tmp"),
                model: None,
                effort: None,
                permission_mode: "acceptEdits".into(),
                sandbox: "workspace-write".into(),
                allowed_tools: None,
            },
            events_tx: tx,
            turn: 0,
            pending: Vec::new(),
        }
    }

    fn spec(run_args: &[&str], model_flag: &str, prompt_flag: &str) -> GenericSpec {
        GenericSpec {
            bin: "tool".into(),
            run_args: run_args.iter().map(|s| s.to_string()).collect(),
            model_flag: model_flag.into(),
            prompt_flag: prompt_flag.into(),
        }
    }

    #[test]
    fn trailing_positional_prompt() {
        // opencode-style: `tool run <prompt>`
        let m = mgr(spec(&["run"], "--model", ""));
        let args = m.build_args("hello");
        assert_eq!(args, vec!["run", "hello"]);
    }

    #[test]
    fn prompt_behind_a_flag_with_model() {
        // gemini-style: `tool -m <model> -p <prompt>`
        let mut m = mgr(spec(&[], "-m", "-p"));
        m.ctx.model = Some("gemini-2.5-pro".into());
        let args = m.build_args("explain this");
        assert_eq!(args, vec!["-m", "gemini-2.5-pro", "-p", "explain this"]);
    }

    #[test]
    fn no_model_flag_means_model_is_dropped() {
        // A tool with no model flag ignores any selected model.
        let mut m = mgr(spec(&["chat"], "", ""));
        m.ctx.model = Some("whatever".into());
        let args = m.build_args("hi");
        assert_eq!(args, vec!["chat", "hi"]);
    }

    #[test]
    fn injected_notes_fold_into_prompt() {
        let mut m = mgr(spec(&[], "-m", ""));
        m.pending.push("focus on tests".into());
        let folded = m.fold_prompt("draft the plan".into());
        assert!(folded.contains("focus on tests"));
        assert!(folded.contains("draft the plan"));
        assert!(m.pending.is_empty());
    }

    #[test]
    fn from_info_copies_the_row() {
        let info = crate::agent::tool_info("gemini").expect("gemini row");
        let a = GenericAdapter::from_info(info);
        assert_eq!(a.spec.bin, "gemini");
        assert_eq!(a.spec.prompt_flag, "-p");
    }
}
