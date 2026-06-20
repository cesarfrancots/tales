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
use crate::{AgentId, Result, TokenUsage};

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
                AgentCommand::StartTurn {
                    prompt,
                    attachments,
                } => {
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
        // Reasoning effort is a config override (`-c key=value`), an exec-level
        // flag, so it must precede the `resume` subcommand below.
        if let Some(effort) = &self.ctx.effort {
            args.push("-c".into());
            args.push(format!("model_reasoning_effort={effort}"));
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
            // Non-interactive: the prompt is passed as an arg, so don't wait on
            // stdin. (Codex still prints a benign "Reading additional input from
            // stdin…" banner to stderr regardless — filtered in `stderr_task`.)
            .stdin(Stdio::null())
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
                    token_usage: None,
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
                let token_usage = parse_codex_token_usage(v);
                let _ = self
                    .events_tx
                    .send(AgentEvent::TurnComplete {
                        agent,
                        turn,
                        cost_usd: None, // codex reports tokens, not USD
                        token_usage,
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
                // Surface the failure AS the turn's message so it lands in the
                // transcript (and the next agent's context) instead of vanishing
                // into a warn log — otherwise a failed turn reads as the agent
                // saying nothing at all.
                let _ = self
                    .events_tx
                    .send(AgentEvent::MessageFinal {
                        agent,
                        turn,
                        text: format!("[turn failed: {msg}]"),
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
                        token_usage: None,
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
            // web_search, …) is surfaced as a tool activity — with a human
            // summary of what it actually did, not just the wire-type name (so
            // the Codex feed isn't a wall of identical "command_execution").
            Some(other) => {
                let _ = self
                    .events_tx
                    .send(AgentEvent::ToolCall {
                        agent,
                        turn,
                        name: codex_tool_summary(other, item),
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
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Codex prints benign banners and (user-side) MCP transport chatter to
        // stderr that are NOT turn failures. Logging them at `warn` makes them
        // read like Tales errored — and on a shared console they interleave into
        // the discussion. Demote the known-benign ones to `debug`.
        if is_benign_codex_noise(t) {
            tracing::debug!(%agent, "codex: {t}");
        } else {
            tracing::warn!(%agent, "codex stderr: {t}");
        }
    }
}

/// True for Codex stderr lines that are noise relative to the turn — its
/// non-interactive banner and chatter from the user's own MCP servers (e.g. an
/// unauthenticated remote MCP), neither of which means the turn failed.
fn is_benign_codex_noise(line: &str) -> bool {
    const NOISE: &[&str] = &[
        "Reading additional input from stdin",
        "rmcp::transport",
        "worker quit with fatal",
        "Transport channel closed",
        "AuthRequired",
        "oauth-protected-resource",
    ];
    NOISE.iter().any(|n| line.contains(n))
}

fn parse_codex_token_usage(event: &Value) -> Option<TokenUsage> {
    let usage = event.get("usage")?;
    let token_usage = TokenUsage {
        input_tokens: usage_u64(
            usage,
            &[
                "input_tokens",
                "prompt_tokens",
                "input",
                "prompt",
                "tokens_in",
            ],
            &[],
        ),
        cached_input_tokens: usage_u64(
            usage,
            &[
                "cached_input_tokens",
                "input_cached_tokens",
                "cached_tokens",
            ],
            &[
                "/input_token_details/cached_tokens",
                "/prompt_tokens_details/cached_tokens",
            ],
        ),
        output_tokens: usage_u64(
            usage,
            &[
                "output_tokens",
                "completion_tokens",
                "output",
                "completion",
                "tokens_out",
            ],
            &[],
        ),
        reasoning_output_tokens: usage_u64(
            usage,
            &["reasoning_output_tokens", "reasoning_tokens"],
            &["/output_token_details/reasoning_tokens"],
        ),
        total_tokens: usage_u64(usage, &["total_tokens", "total"], &[]),
    };
    (!token_usage.is_empty()).then_some(token_usage)
}

fn usage_u64(usage: &Value, keys: &[&str], pointers: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(value) = usage.get(*key).and_then(Value::as_u64) {
            return Some(value);
        }
    }
    for pointer in pointers {
        if let Some(value) = usage.pointer(pointer).and_then(Value::as_u64) {
            return Some(value);
        }
    }
    None
}

/// Build a human-readable summary of a Codex `item.completed` tool event, so the
/// feed shows what it did (`command_execution: git diff --stat`) instead of a
/// wall of identical wire-type names. Falls back to the type name if no useful
/// field is present, so an unexpected payload shape never regresses.
fn codex_tool_summary(kind: &str, item: &Value) -> String {
    const KEYS: &[&str] = &[
        "command",
        "cmd",
        "parsed_cmd",
        "path",
        "query",
        "tool",
        "name",
        "aggregated_output",
    ];
    for k in KEYS {
        if let Some(v) = item.get(k) {
            if let Some(s) = value_brief(v) {
                let s = s.trim();
                if !s.is_empty() {
                    return format!("{kind}: {}", truncate(s, 80));
                }
            }
        }
    }
    kind.to_string()
}

/// First useful single line from a JSON value: a string's first line, or a
/// string array joined (e.g. Codex's `["bash","-lc","git diff"]`).
fn value_brief(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.lines().next().unwrap_or(s).to_string());
    }
    if let Some(arr) = v.as_array() {
        let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }
    None
}

/// Truncate to `max` chars with an ellipsis, on a char boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_summary_uses_real_command_not_wire_type() {
        let item =
            json!({ "type": "command_execution", "command": ["bash", "-lc", "git diff --stat"] });
        assert_eq!(
            codex_tool_summary("command_execution", &item),
            "command_execution: bash -lc git diff --stat"
        );
        // file_change → the path.
        let fc = json!({ "type": "file_change", "path": "src/main.rs" });
        assert_eq!(
            codex_tool_summary("file_change", &fc),
            "file_change: src/main.rs"
        );
    }

    #[test]
    fn tool_summary_falls_back_to_kind_when_no_useful_field() {
        let item = json!({ "type": "mystery", "weird": 1 });
        assert_eq!(codex_tool_summary("mystery", &item), "mystery");
    }

    #[test]
    fn benign_noise_is_classified() {
        assert!(is_benign_codex_noise(
            "Reading additional input from stdin..."
        ));
        assert!(is_benign_codex_noise(
            "ERROR rmcp::transport::worker: worker quit with fatal: Transport channel closed, when AuthRequired(...)"
        ));
        assert!(!is_benign_codex_noise("error: rate limit exceeded"));
    }

    #[test]
    fn token_usage_parses_flat_and_nested_codex_usage() {
        let flat = json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": 100,
                "cached_input_tokens": 25,
                "output_tokens": 40,
                "reasoning_output_tokens": 10,
                "total_tokens": 150
            }
        });
        assert_eq!(
            parse_codex_token_usage(&flat),
            Some(TokenUsage {
                input_tokens: Some(100),
                cached_input_tokens: Some(25),
                output_tokens: Some(40),
                reasoning_output_tokens: Some(10),
                total_tokens: Some(150),
            })
        );

        let nested = json!({
            "type": "turn.completed",
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 40,
                "prompt_tokens_details": { "cached_tokens": 20 },
                "output_token_details": { "reasoning_tokens": 8 }
            }
        });
        assert_eq!(
            parse_codex_token_usage(&nested),
            Some(TokenUsage {
                input_tokens: Some(100),
                cached_input_tokens: Some(20),
                output_tokens: Some(40),
                reasoning_output_tokens: Some(8),
                total_tokens: None,
            })
        );
    }

    fn mgr_with(model: Option<&str>, effort: Option<&str>) -> Manager {
        let (tx, _rx) = mpsc::channel(8);
        Manager {
            bin: "codex".into(),
            ctx: SpawnCtx {
                agent: uuid::Uuid::new_v4(),
                label: "codex".into(),
                cwd: PathBuf::from("/tmp"),
                model: model.map(String::from),
                effort: effort.map(String::from),
                permission_mode: "acceptEdits".into(),
                sandbox: "workspace-write".into(),
                allowed_tools: None,
            },
            events_tx: tx,
            thread_id: None,
            announced_session: false,
            turn: 0,
            pending: Vec::new(),
        }
    }

    #[test]
    fn build_args_wires_model_and_effort() {
        let m = mgr_with(Some("gpt-5-codex"), Some("high"));
        let args = m.build_args("do the thing", &[]);
        // model via -m
        let mi = args.iter().position(|a| a == "-m").expect("-m present");
        assert_eq!(args[mi + 1], "gpt-5-codex");
        // effort via -c model_reasoning_effort=high, before the trailing prompt
        let ci = args.iter().position(|a| a == "-c").expect("-c present");
        assert_eq!(args[ci + 1], "model_reasoning_effort=high");
        assert_eq!(args.last().map(String::as_str), Some("do the thing"));
    }

    #[test]
    fn build_args_omits_effort_when_unset() {
        let m = mgr_with(None, None);
        let args = m.build_args("hi", &[]);
        assert!(!args.iter().any(|a| a == "-c"));
        assert!(!args.iter().any(|a| a == "-m"));
    }
}
