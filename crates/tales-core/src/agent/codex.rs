//! Codex adapter — turn-based `codex exec --json`.
//!
//! Unlike Claude's persistent bidirectional stream, Codex runs one turn per
//! process: `codex exec` (first turn) or `codex exec … resume <thread_id>`
//! (subsequent turns) each emit a JSONL event stream and then exit. This
//! adapter is therefore a *per-turn process manager*: a single manager task
//! consumes [`AgentCommand`]s and, for each `StartTurn`, spawns a fresh `codex`
//! child, parses its events, waits for it to exit, then loops.
//!
//! `midturn_injection = false`: an [`AgentCommand::InjectMessage`] that arrives
//! between turns is queued and folded into the next `StartTurn`. This is the
//! architectural asymmetry with Claude, fully contained behind the trait.
//!
//! Wire format handled (verified against codex exec --json):
//!   {"type":"thread.started","thread_id":"…"}
//!   {"type":"turn.started"}
//!   {"type":"item.completed","item":{"type":"agent_message","text":"…"}}
//!   {"type":"item.completed","item":{"type":"command_execution",…}}  (tool)
//!   {"type":"item.completed","item":{"type":"error","message":"…"}}
//!   {"type":"turn.completed","usage":{…}}        (token counts, no USD cost)
//!   {"type":"turn.failed",…}

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{AgentAdapter, AgentCaps, AgentCommand, AgentEvent, Attachment, SpawnCtx, TurnId};
use crate::{AgentId, Result};

/// Adapter for the `codex` CLI.
pub struct CodexAdapter {
    bin: String,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            bin: "codex".to_string(),
        }
    }

    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentAdapter for CodexAdapter {
    fn caps(&self) -> AgentCaps {
        AgentCaps {
            midturn_injection: false, // turn-based: injected notes fold into next turn
            structured_output: true,  // --output-schema
            resumable: true,          // exec … resume <thread_id>
            mcp: true,
        }
    }

    async fn spawn(
        &mut self,
        ctx: SpawnCtx,
        events_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<mpsc::Sender<AgentCommand>> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AgentCommand>(64);
        let mgr = Manager {
            bin: self.bin.clone(),
            ctx,
            events_tx,
            thread_id: None,
            announced_session: false,
            turn: 0,
            pending: Vec::new(),
        };
        tokio::spawn(mgr.run(cmd_rx));
        Ok(cmd_tx)
    }
}

/// Owns all per-session state and drives one `codex` child per turn.
struct Manager {
    bin: String,
    ctx: SpawnCtx,
    events_tx: mpsc::Sender<AgentEvent>,
    thread_id: Option<String>,
    announced_session: bool,
    turn: TurnId,
    /// Notes injected between turns, folded into the next StartTurn prompt.
    pending: Vec<String>,
}

impl Manager {
    async fn run(mut self, mut cmd_rx: mpsc::Receiver<AgentCommand>) {
        let mut last_code: Option<i32> = Some(0);
        while let Some(command) = cmd_rx.recv().await {
            match command {
                AgentCommand::StartTurn { prompt, attachments } => {
                    let full = self.fold_prompt(prompt);
                    last_code = self.run_turn(full, &attachments).await;
                }
                AgentCommand::InjectMessage { text } => {
                    // Turn-based: cannot interrupt; defer to the next turn.
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

    /// Build args for one turn (fresh session vs resume).
    fn build_args(&self, prompt: &str, attachments: &[Attachment]) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "exec".into(),
            "--json".into(),
            "--sandbox".into(),
            self.ctx.sandbox.clone(),
            "--skip-git-repo-check".into(),
        ];
        if let Some(model) = &self.ctx.model {
            args.push("-m".into());
            args.push(model.clone());
        }
        // Images are attached with -i; Codex doesn't take PDFs this way, but the
        // prompt text already names every attachment.
        for a in attachments {
            if a.is_image() {
                args.push("-i".into());
                args.push(a.path.to_string_lossy().into_owned());
            }
        }
        match &self.thread_id {
            Some(tid) => {
                // exec-level flags first, then the `resume` subcommand.
                args.push("resume".into());
                args.push(tid.clone());
                args.push("--".into());
                args.push(prompt.to_string());
            }
            None => {
                args.push("--".into());
                args.push(prompt.to_string());
            }
        }
        args
    }

    /// Spawn one `codex` child, parse its events to completion, reap it.
    async fn run_turn(&mut self, prompt: String, attachments: &[Attachment]) -> Option<i32> {
        let agent = self.ctx.agent;
        self.turn += 1;
        let turn = self.turn;
        let args = self.build_args(&prompt, attachments);

        let mut child = match Command::new(&self.bin)
            .args(&args)
            .current_dir(PathBuf::from(&self.ctx.cwd))
            .stdin(Stdio::null()) // avoid "reading from stdin" wait
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
                        message: format!("failed to spawn {}: {e}", self.bin),
                        fatal: true,
                    })
                    .await;
                return None;
            }
        };

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        tokio::spawn(stderr_task(stderr, agent));

        let _ = self
            .events_tx
            .send(AgentEvent::TurnStarted { agent, turn })
            .await;

        let mut lines = BufReader::new(stdout).lines();
        let mut terminal_seen = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if self.handle_event(&v, turn).await {
                terminal_seen = true;
            }
        }

        // The child exited. If it never emitted a terminal turn event (crash,
        // killed, truncated stream), synthesize a TurnComplete so the
        // orchestrator never deadlocks waiting for one.
        if !terminal_seen {
            let _ = self
                .events_tx
                .send(AgentEvent::TurnComplete {
                    agent: self.ctx.agent,
                    turn,
                    cost_usd: None,
                })
                .await;
        }

        child.wait().await.ok().and_then(|s| s.code())
    }

    /// Returns `true` if this event terminated the turn (`turn.completed` or
    /// `turn.failed`) — both are terminal in the codex wire protocol, so the
    /// orchestrator must see a `TurnComplete` for either.
    async fn handle_event(&mut self, v: &Value, turn: TurnId) -> bool {
        let agent = self.ctx.agent;
        match v.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                if let Some(tid) = v.get("thread_id").and_then(Value::as_str) {
                    self.thread_id = Some(tid.to_string());
                    if !self.announced_session {
                        self.announced_session = true;
                        let _ = self
                            .events_tx
                            .send(AgentEvent::SessionReady {
                                agent,
                                native_session_id: tid.to_string(),
                            })
                            .await;
                    }
                }
                false
            }
            Some("item.completed") => {
                if let Some(item) = v.get("item") {
                    self.handle_item(item, turn).await;
                }
                false
            }
            Some("turn.completed") => {
                let _ = self
                    .events_tx
                    .send(AgentEvent::TurnComplete {
                        agent,
                        turn,
                        cost_usd: None, // codex reports tokens, not USD
                    })
                    .await;
                true
            }
            Some("turn.failed") => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed")
                    .to_string();
                let _ = self
                    .events_tx
                    .send(AgentEvent::Error {
                        agent,
                        message: msg,
                        fatal: false,
                    })
                    .await;
                // `turn.failed` is terminal — no `turn.completed` will follow.
                // Emit TurnComplete so the orchestrator's turn loop unblocks.
                let _ = self
                    .events_tx
                    .send(AgentEvent::TurnComplete {
                        agent,
                        turn,
                        cost_usd: None,
                    })
                    .await;
                true
            }
            _ => false,
        }
    }

    async fn handle_item(&mut self, item: &Value, turn: TurnId) {
        let agent = self.ctx.agent;
        match item.get("type").and_then(Value::as_str) {
            Some("agent_message") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    let _ = self
                        .events_tx
                        .send(AgentEvent::MessageFinal {
                            agent,
                            turn,
                            text: text.to_string(),
                        })
                        .await;
                }
            }
            Some("error") => {
                let msg = item
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string();
                // Codex emits an informational "Skill descriptions were
                // shortened…" notice as an error item at turn start; it is not
                // a real failure, so demote it to a log line.
                if msg.starts_with("Skill descriptions were shortened") {
                    tracing::debug!(%agent, "codex notice: {msg}");
                } else {
                    let _ = self
                        .events_tx
                        .send(AgentEvent::Error {
                            agent,
                            message: msg,
                            fatal: false,
                        })
                        .await;
                }
            }
            // reasoning is internal; ignore for the console feed.
            Some("reasoning") => {}
            // Everything else (command_execution, file_change, mcp_tool_call,
            // web_search, …) is surfaced as a tool activity.
            Some(other) => {
                let _ = self
                    .events_tx
                    .send(AgentEvent::ToolCall {
                        agent,
                        turn,
                        name: other.to_string(),
                        input: item.clone(),
                    })
                    .await;
            }
            None => {}
        }
    }
}

async fn stderr_task(stderr: tokio::process::ChildStderr, agent: AgentId) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            tracing::warn!(%agent, "codex stderr: {line}");
        }
    }
}
