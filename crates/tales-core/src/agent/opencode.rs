//! Open Code adapter — turn-based `opencode run`.
//!
//! [Open Code](https://opencode.ai) (the `opencode` CLI) has no bidirectional
//! stream-json mode, so — like Codex — it is driven one turn per process:
//! `opencode run <prompt>` (first turn) or `opencode run --continue <prompt>`
//! (subsequent turns) print the assistant's reply to stdout and then exit. This
//! adapter is therefore a *per-turn process manager*: a single manager task
//! consumes [`AgentCommand`]s and, for each `StartTurn`, spawns a fresh
//! `opencode` child, streams its stdout, waits for it to exit, then loops.
//!
//! `midturn_injection = false`: an [`AgentCommand::InjectMessage`] arriving
//! between turns is queued and folded into the next `StartTurn`, exactly as for
//! Codex. Session continuity across turns uses `--continue`, which resumes the
//! **last session in this project directory** — unlike Codex (which captures and
//! resumes an explicit `thread_id`), Open Code exposes no session id to `run`,
//! so its continuity is keyed on the cwd. PRECONDITION: at most one Open Code
//! agent per run, since two sharing a directory would resume each other's
//! `--continue` session. [`crate::agent::validate_roster`] enforces this at
//! roster construction. The flag is set only after a turn that actually
//! succeeds with output, so a failed/empty first turn never resumes a stale or
//! foreign session.
//!
//! Unlike Codex's JSONL event stream, `opencode run` prints the response as
//! plain markdown on stdout. We stream it line-by-line as `TokenDelta`s for a
//! live feel, then deliver the whole reply as one `MessageFinal` and synthesize
//! a `TurnComplete` (Open Code reports no machine-readable USD cost).

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{AgentAdapter, AgentCaps, AgentCommand, AgentEvent, SpawnCtx, TurnId};
use crate::AgentId;
use crate::Result;

/// Adapter for the `opencode` CLI.
pub struct OpenCodeAdapter {
    bin: String,
}

impl OpenCodeAdapter {
    pub fn new() -> Self {
        Self {
            bin: "opencode".to_string(),
        }
    }

    /// Override the binary name/path (useful for tests or non-default installs).
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

impl Default for OpenCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentAdapter for OpenCodeAdapter {
    fn caps(&self) -> AgentCaps {
        AgentCaps {
            midturn_injection: false, // turn-based: injected notes fold into next turn
            structured_output: false, // `run` prints plain markdown, no schema mode
            resumable: true,          // run --continue
            mcp: true,                // opencode supports MCP servers
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
            started_session: false,
            turn: 0,
            pending: Vec::new(),
        };
        tokio::spawn(mgr.run(cmd_rx));
        Ok(cmd_tx)
    }
}

/// Owns all per-session state and drives one `opencode` child per turn.
struct Manager {
    bin: String,
    ctx: SpawnCtx,
    events_tx: mpsc::Sender<AgentEvent>,
    /// Set once the first turn has run, so later turns add `--continue`.
    started_session: bool,
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
                    // Open Code's `run` takes only a text prompt — image/PDF
                    // attachments aren't passed through the CLI, but the prompt
                    // text already names every shared file.
                    let full = self.fold_prompt(prompt);
                    last_code = self.run_turn(full).await;
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

    /// Build args for one turn (fresh session vs `--continue`).
    fn build_args(&self, prompt: &str) -> Vec<String> {
        let mut args: Vec<String> = vec!["run".into()];
        if self.started_session {
            // Continue the session this agent created on its first turn.
            args.push("--continue".into());
        }
        if let Some(model) = &self.ctx.model {
            // Open Code expects provider/model form, e.g. `anthropic/claude-...`.
            args.push("--model".into());
            args.push(model.clone());
        }
        // The prompt is a single positional. Our orchestrator-composed prompts
        // never begin with `-`, so they won't be mistaken for flags.
        args.push(prompt.to_string());
        args
    }

    /// Spawn one `opencode` child, stream its stdout to completion, reap it.
    async fn run_turn(&mut self, prompt: String) -> Option<i32> {
        let agent = self.ctx.agent;
        self.turn += 1;
        let turn = self.turn;
        let args = self.build_args(&prompt);

        let mut child = match Command::new(&self.bin)
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
        // Open Code reports no machine cost; always close the turn so the
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

        // Only mark the session resumable if this turn actually succeeded with
        // output. Setting it unconditionally would let a failed/empty first turn
        // (auth error, bad --model, network) make later turns' `--continue`
        // resume a stale or foreign session in the same directory.
        let code = child.wait().await.ok().and_then(|s| s.code());
        if code == Some(0) && !trimmed.is_empty() {
            self.started_session = true;
        }
        code
    }
}

async fn stderr_task(stderr: tokio::process::ChildStderr, agent: AgentId) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            tracing::warn!(%agent, "opencode stderr: {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn mgr() -> Manager {
        let (tx, _rx) = mpsc::channel(8);
        Manager {
            bin: "opencode".into(),
            ctx: SpawnCtx {
                agent: Uuid::new_v4(),
                label: "opencode".into(),
                cwd: PathBuf::from("/tmp"),
                model: None,
                effort: None,
                permission_mode: "acceptEdits".into(),
                sandbox: "workspace-write".into(),
                allowed_tools: None,
            },
            events_tx: tx,
            started_session: false,
            turn: 0,
            pending: Vec::new(),
        }
    }

    #[test]
    fn first_turn_has_no_continue() {
        let m = mgr();
        let args = m.build_args("hello");
        assert_eq!(args.first().map(String::as_str), Some("run"));
        assert!(!args.iter().any(|a| a == "--continue"));
        assert_eq!(args.last().map(String::as_str), Some("hello"));
    }

    #[test]
    fn later_turns_continue_and_pass_model() {
        let mut m = mgr();
        m.started_session = true;
        m.ctx.model = Some("anthropic/claude-sonnet".into());
        let args = m.build_args("revise the plan");
        assert!(args.iter().any(|a| a == "--continue"));
        assert!(args.iter().any(|a| a == "--model"));
        assert!(args.iter().any(|a| a == "anthropic/claude-sonnet"));
        assert_eq!(args.last().map(String::as_str), Some("revise the plan"));
    }

    #[test]
    fn injected_notes_fold_into_prompt() {
        let mut m = mgr();
        m.pending.push("focus on tests".into());
        let folded = m.fold_prompt("draft the plan".into());
        assert!(folded.contains("focus on tests"));
        assert!(folded.contains("draft the plan"));
        assert!(m.pending.is_empty());
    }
}
