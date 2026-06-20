//! Claude Code adapter — bidirectional `stream-json` over stdio.
//!
//! Launches `claude -p --input-format stream-json --output-format stream-json
//! --include-partial-messages --verbose` and keeps one long-lived process per
//! session. Because the input format is also stream-json, new user messages can
//! be written to stdin at any time → `midturn_injection = true`. Closing stdin
//! is the graceful-shutdown signal (the CLI finishes and exits on EOF).
//!
//! Wire format handled (verified against claude 2.1.x):
//!   {"type":"system","subtype":"init","session_id":…}
//!   {"type":"stream_event","event":{"type":"message_start",…}}
//!   {"type":"stream_event","event":{"type":"content_block_delta",
//!                                   "delta":{"type":"text_delta","text":…}}}
//!   {"type":"assistant","message":{"content":[{"type":"text","text":…},
//!                                              {"type":"tool_use",…}]}}
//!   {"type":"result","subtype":"success","total_cost_usd":…}

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use base64::Engine as _;

use super::{
    project_mcp_config_risks, AgentAdapter, AgentCaps, AgentCommand, AgentEvent, Attachment,
    SpawnCtx, TurnId,
};
use crate::{AgentId, Result, TalesError};

/// Adapter for the `claude` CLI.
pub struct ClaudeAdapter {
    bin: String,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self {
            bin: "claude".to_string(),
        }
    }

    /// Override the binary name/path (useful for tests or non-default installs).
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn caps(&self) -> AgentCaps {
        AgentCaps {
            midturn_injection: true,
            structured_output: true,
            resumable: true,
            mcp: true,
        }
    }

    async fn spawn(
        &mut self,
        ctx: SpawnCtx,
        events_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<mpsc::Sender<AgentCommand>> {
        let session_id = ctx.agent.to_string();

        let mut cmd = Command::new(&self.bin);
        cmd.arg("-p")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--verbose")
            .arg("--session-id")
            .arg(&session_id)
            .arg("--permission-mode")
            .arg(&ctx.permission_mode)
            .arg("--add-dir")
            .arg(&ctx.cwd)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if !project_mcp_config_risks(&ctx.cwd).is_empty() {
            cmd.arg("--strict-mcp-config")
                .arg("--mcp-config")
                .arg(r#"{"mcpServers":{}}"#)
                .arg("--setting-sources")
                .arg("user");
        }

        if let Some(model) = &ctx.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(effort) = &ctx.effort {
            cmd.arg("--effort").arg(effort);
        }
        if let Some(tools) = &ctx.allowed_tools {
            if !tools.is_empty() {
                cmd.arg("--allowedTools").args(tools);
            }
        }
        // Own process group so a force-kill can reap claude's tool/MCP children.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| TalesError::Spawn(format!("{}: {e}", self.bin)))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (cmd_tx, cmd_rx) = mpsc::channel::<AgentCommand>(64);
        let agent = ctx.agent;
        // Lets the writer tell the waiter "shutdown requested" so the waiter can
        // force-kill if the child doesn't exit on stdin EOF within a grace.
        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        // Writer: owns stdin, serializes commands as stream-json user messages.
        tokio::spawn(writer_task(stdin, cmd_rx, kill_tx));
        // Reader: parses stdout JSONL → normalized events.
        tokio::spawn(reader_task(stdout, events_tx.clone(), agent));
        // Stderr drain → tracing (never let it fill the OS pipe).
        tokio::spawn(stderr_task(stderr, agent));
        // Waiter: reaps the child and reports exit (force-kills on shutdown).
        tokio::spawn(waiter_task(child, events_tx, agent, kill_rx));

        Ok(cmd_tx)
    }
}

/// One JSON line per user message, matching the `stream-json` input schema.
/// Image/PDF attachments are encoded as base64 content blocks so Claude sees them.
fn user_message_line(text: &str, attachments: &[Attachment]) -> String {
    let mut content = vec![serde_json::json!({ "type": "text", "text": text })];
    for a in attachments {
        if let Some(block) = attachment_block(a) {
            content.push(block);
        }
    }
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": content }
    })
    .to_string()
}

/// Encode an attachment as a Claude content block (image or document).
fn attachment_block(a: &Attachment) -> Option<Value> {
    let bytes = std::fs::read(&a.path).ok()?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if a.is_image() {
        let media_type = match a.ext().as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "image/png",
        };
        Some(serde_json::json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data }
        }))
    } else if a.is_pdf() {
        Some(serde_json::json!({
            "type": "document",
            "source": { "type": "base64", "media_type": "application/pdf", "data": data }
        }))
    } else {
        None
    }
}

async fn writer_task(
    mut stdin: tokio::process::ChildStdin,
    mut cmd_rx: mpsc::Receiver<AgentCommand>,
    kill_tx: oneshot::Sender<()>,
) {
    while let Some(command) = cmd_rx.recv().await {
        let line = match command {
            AgentCommand::StartTurn {
                prompt,
                attachments,
            } => user_message_line(&prompt, &attachments),
            AgentCommand::InjectMessage { text } => user_message_line(&text, &[]),
            AgentCommand::Shutdown => break,
        };
        if stdin.write_all(line.as_bytes()).await.is_err()
            || stdin.write_all(b"\n").await.is_err()
            || stdin.flush().await.is_err()
        {
            break;
        }
    }
    // Close the pipe → claude sees EOF and exits gracefully, then tell the
    // waiter so it can force-kill if the child lingers.
    drop(stdin);
    let _ = kill_tx.send(());
}

async fn reader_task(
    stdout: tokio::process::ChildStdout,
    events_tx: mpsc::Sender<AgentEvent>,
    agent: AgentId,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut turn: TurnId = 0;

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break, // EOF
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // tolerate non-JSON / partial noise
        };

        match v.get("type").and_then(Value::as_str) {
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                    let _ = events_tx
                        .send(AgentEvent::SessionReady {
                            agent,
                            native_session_id: sid.to_string(),
                        })
                        .await;
                }
                if let Some(skills) = v.get("skills").and_then(Value::as_array) {
                    let list: Vec<String> = skills
                        .iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect();
                    if !list.is_empty() {
                        let _ = events_tx
                            .send(AgentEvent::Skills {
                                agent,
                                skills: list,
                            })
                            .await;
                    }
                }
            }
            Some("stream_event") => {
                if let Some(event) = v.get("event") {
                    match event.get("type").and_then(Value::as_str) {
                        Some("message_start") => {
                            turn += 1;
                            let _ = events_tx
                                .send(AgentEvent::TurnStarted { agent, turn })
                                .await;
                        }
                        Some("content_block_delta") => {
                            if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str)
                            {
                                let _ = events_tx
                                    .send(AgentEvent::TokenDelta {
                                        agent,
                                        turn,
                                        text: text.to_string(),
                                    })
                                    .await;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                if let Some(content) = v.pointer("/message/content").and_then(Value::as_array) {
                    let mut text = String::new();
                    for block in content {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    text.push_str(t);
                                }
                            }
                            Some("tool_use") => {
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                let input = block.get("input").cloned().unwrap_or(Value::Null);
                                let _ = events_tx
                                    .send(AgentEvent::ToolCall {
                                        agent,
                                        turn,
                                        name,
                                        input,
                                    })
                                    .await;
                            }
                            _ => {}
                        }
                    }
                    if !text.is_empty() {
                        let _ = events_tx
                            .send(AgentEvent::MessageFinal { agent, turn, text })
                            .await;
                    }
                }
            }
            Some("result") => {
                let cost = v.get("total_cost_usd").and_then(Value::as_f64);
                let _ = events_tx
                    .send(AgentEvent::TurnComplete {
                        agent,
                        turn,
                        cost_usd: cost,
                        token_usage: None,
                    })
                    .await;
            }
            _ => {}
        }
    }
}

async fn stderr_task(stderr: tokio::process::ChildStderr, agent: AgentId) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            tracing::warn!(%agent, "claude stderr: {line}");
        }
    }
}

async fn waiter_task(
    mut child: tokio::process::Child,
    events_tx: mpsc::Sender<AgentEvent>,
    agent: AgentId,
    kill_rx: oneshot::Receiver<()>,
) {
    let pid = child.id();
    let code = tokio::select! {
        status = child.wait() => status.ok().and_then(|s| s.code()),
        _ = kill_rx => {
            // Shutdown requested: give the child a short grace to exit on the
            // stdin EOF, then force-kill so it can never become a zombie.
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(status) => status.ok().and_then(|s| s.code()),
                Err(_) => {
                    kill_process_group(pid);
                    let _ = child.start_kill();
                    child.wait().await.ok().and_then(|s| s.code())
                }
            }
        }
    };
    let _ = events_tx.send(AgentEvent::Exited { agent, code }).await;
}

/// Force-kill the child's whole process group. The child is its own group
/// leader (`process_group(0)` at spawn), so its pgid equals its pid; signalling
/// `-pid` reaps the agent and any tool/MCP subprocesses it spawned.
#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .output();
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}
