//! `tales` — command-line frontend over the orchestration core.
//!
//! Subcommands:
//! - `term` — open the Warp-style Tales terminal workspace.
//! - `solo` — drive a single agent for one turn (claude or codex).
//! - `discuss` — a live drafter/critic discussion between two agents.
//! - `run` — the full pipeline (discuss → recommend → execute), scriptable.
//!
//! A thin frontend: it subscribes to `OrchestratorEvent`s and sends commands.
//! `tales-tui` (terminal) and `tales-web` (browser) are richer frontends over
//! the same bus.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use tales_core::agent::{make_adapter, AgentCommand, AgentEvent, Attachment, SpawnCtx};
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::{OrchestratorEvent, UserCommand};
use tales_core::orchestrator::{Orchestrator, RunOutcome};
use tales_core::worktree::{MergeOutcome, WorktreeManager};

#[derive(Parser, Debug)]
#[command(
    name = "tales",
    about = "Multi-agent AI coding orchestrator",
    long_about = "Multi-agent AI coding orchestrator.\n\nRun `tales` with no subcommand to open the terminal workspace — Tales is the \
                  default pane, with sibling panes for shells, Codex, Claude Code, and Open Code. \
                  The subcommands below are the scriptable, non-interactive counterparts."
)]
struct Cli {
    /// With no subcommand, `tales` opens the terminal workspace (the headline
    /// experience). The subcommands are for scripting or explicit launching.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the Warp-style Tales terminal workspace.
    Term,
    /// Drive a single agent for one turn.
    Solo {
        /// The prompt to send.
        prompt: String,
        /// Which agent: claude | codex.
        #[arg(long, default_value = "claude")]
        agent: String,
        /// Model alias/id (optional).
        #[arg(long)]
        model: Option<String>,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Codex sandbox policy.
        #[arg(long, default_value = "workspace-write")]
        sandbox: String,
        /// Attach an image/PDF (repeatable): --image a.png --image b.pdf
        #[arg(long)]
        image: Vec<String>,
    },
    /// Full pipeline, non-interactive: discuss → recommend → auto-confirm the
    /// chosen executor → execute. The scriptable counterpart to the live TUI.
    Run {
        /// The task to plan and execute.
        prompt: String,
        /// Drafter agent: claude | codex.
        #[arg(long, default_value = "claude")]
        drafter: String,
        /// Critic agent: claude | codex.
        #[arg(long, default_value = "codex")]
        critic: String,
        /// Which agent executes the agreed plan (auto-confirmed).
        #[arg(long, default_value = "claude")]
        execute: String,
        #[arg(long)]
        drafter_model: Option<String>,
        #[arg(long)]
        critic_model: Option<String>,
        #[arg(long, default_value_t = 4)]
        turns: usize,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value = "workspace-write")]
        sandbox: String,
        /// Run the executor in its own git worktree, then merge the result back
        /// into the current branch (requires a git repo).
        #[arg(long)]
        worktree: bool,
    },
    /// Run a live drafter/critic discussion between two agents.
    Discuss {
        /// The task the agents discuss.
        prompt: String,
        /// Drafter agent: claude | codex.
        #[arg(long, default_value = "claude")]
        drafter: String,
        /// Critic agent: claude | codex.
        #[arg(long, default_value = "codex")]
        critic: String,
        /// Total number of turns (alternating drafter/critic).
        #[arg(long, default_value_t = 4)]
        turns: usize,
        /// Model for the drafter (optional).
        #[arg(long)]
        drafter_model: Option<String>,
        /// Model for the critic (optional).
        #[arg(long)]
        critic_model: Option<String>,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Codex sandbox policy.
        #[arg(long, default_value = "read-only")]
        sandbox: String,
    },
    /// Open the terminal workspace in a NEW window — for harnesses with no TTY
    /// (the `/tales` command shells out to this). Deterministic: no AI reasoning
    /// is needed to launch. macOS only.
    Open {
        /// Pre-connect a tool in the new window (repeatable): --connect claude.
        #[arg(long = "connect")]
        connect: Vec<String>,
        /// Optional task to pre-fill the Tales prompt.
        task: Option<String>,
    },
}

/// Resolve the `tales-tui` binary: next to this exe first, then `PATH`.
fn tui_path() -> std::ffi::OsString {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("tales-tui")))
        .filter(|p| p.exists())
        .map(|p| p.into_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("tales-tui"))
}

/// `tales` with no subcommand IS the terminal workspace. The live UI lives in
/// the sibling `tales-tui` binary (it pulls in ratatui/crossterm, which the
/// scriptable CLI doesn't), so we hand off to it — replacing this process on
/// unix so there's no extra shell in the tree.
fn launch_tui() -> ! {
    let target = tui_path();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&target).exec();
        eprintln!(
            "tales: could not launch the terminal workspace ({err}).\n\
             Build it with `cargo build --release` and put target/release on PATH."
        );
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&target).status() {
            Ok(s) => std::process::exit(s.code().unwrap_or(0)),
            Err(e) => {
                eprintln!(
                    "tales: could not launch the terminal workspace ({e}).\n\
                     Build it with `cargo build --release` and put target/release on PATH."
                );
                std::process::exit(127);
            }
        }
    }
}

/// Single-quote a string for safe embedding in the generated bash `.command`
/// (handles spaces, quotes, `$`, etc. — everything inside `'...'` is literal).
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''"); // close, escaped quote, reopen
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Build the contents of the `.command` launcher: cd to the project, then exec
/// `tales-tui` with the pre-connect/pre-fill flags. An empty/whitespace task is
/// omitted so the prompt screen opens blank.
fn build_open_script(tui: &str, cwd: &str, connect: &[String], task: Option<&str>) -> String {
    let mut s = String::from("#!/bin/bash\n");
    s.push_str(&format!("cd {} || exit 1\n", sh_quote(cwd)));
    s.push_str("exec ");
    s.push_str(&sh_quote(tui));
    for c in connect {
        s.push_str(" --connect ");
        s.push_str(&sh_quote(c));
    }
    if let Some(t) = task {
        let t = t.trim();
        if !t.is_empty() {
            s.push_str(" --prefill ");
            s.push_str(&sh_quote(t));
        }
    }
    s.push('\n');
    s
}

/// `tales open` — write a one-shot `.command` and hand it to macOS `open`, which
/// launches it in a new Terminal window. Pure mechanics; no AI reasoning needed.
fn run_open(connect: Vec<String>, task: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let tui = tui_path();
    let cwd = std::env::current_dir()?;
    let script = build_open_script(
        &tui.to_string_lossy(),
        &cwd.to_string_lossy(),
        &connect,
        task.as_deref(),
    );

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("tales-run.{}.{nanos}.command", std::process::id()));
        std::fs::write(&path, script)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        let status = std::process::Command::new("open").arg(&path).status()?;
        if !status.success() {
            return Err(format!("`open` failed ({status})").into());
        }
        println!("Tales terminal opening in a new window.");
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = script;
        eprintln!(
            "`tales open` opens a new macOS Terminal window. On this OS, run `tales` \
             (or `tales-tui`) directly in a terminal instead."
        );
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let Some(command) = Cli::parse().command else {
        // Bare `tales` → open the terminal workspace.
        launch_tui();
    };

    match command {
        Command::Term => launch_tui(),
        // `tales open` needs no async runtime or agents — it just spawns a new
        // Terminal window. Kept dead-simple so `/tales` is a zero-reasoning launcher.
        Command::Open { connect, task } => run_open(connect, task),
        Command::Solo {
            prompt,
            agent,
            model,
            cwd,
            sandbox,
            image,
        } => run_solo(prompt, agent, model, cwd, sandbox, image).await,
        Command::Run {
            prompt,
            drafter,
            critic,
            execute,
            drafter_model,
            critic_model,
            turns,
            cwd,
            sandbox,
            worktree,
        } => {
            run_pipeline(
                prompt,
                drafter,
                critic,
                execute,
                drafter_model,
                critic_model,
                turns,
                cwd,
                sandbox,
                worktree,
            )
            .await
        }
        Command::Discuss {
            prompt,
            drafter,
            critic,
            turns,
            drafter_model,
            critic_model,
            cwd,
            sandbox,
        } => {
            run_discuss(
                prompt,
                drafter,
                critic,
                turns,
                drafter_model,
                critic_model,
                cwd,
                sandbox,
            )
            .await
        }
    }
}

async fn run_solo(
    prompt: String,
    agent_name: String,
    model: Option<String>,
    cwd: Option<String>,
    sandbox: String,
    images: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let agent = Uuid::new_v4();
    let ctx = SpawnCtx {
        agent,
        label: agent_name.clone(),
        cwd,
        model,
        permission_mode: "acceptEdits".to_string(),
        sandbox,
        allowed_tools: None,
    };

    let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
    let mut adapter = match make_adapter(&agent_name) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let cmd_tx = adapter.spawn(ctx, events_tx).await?;
    let attachments: Vec<Attachment> = images.iter().map(Attachment::new).collect();
    cmd_tx
        .send(AgentCommand::StartTurn {
            prompt,
            attachments,
        })
        .await?;

    let mut streaming = false;
    while let Some(event) = events_rx.recv().await {
        match event {
            AgentEvent::SessionReady {
                native_session_id, ..
            } => println!("● session {native_session_id}"),
            AgentEvent::Skills { skills, .. } => {
                println!("● {} skills available", skills.len());
            }
            AgentEvent::TurnStarted { turn, .. } => {
                print!("\n[turn {turn}] ");
                flush();
            }
            AgentEvent::TokenDelta { text, .. } => {
                streaming = true;
                print!("{text}");
                flush();
            }
            AgentEvent::MessageFinal { text, .. } => {
                if !streaming {
                    print!("{text}");
                    flush();
                }
            }
            AgentEvent::ToolCall { name, .. } => println!("\n  ⚙ tool: {name}"),
            AgentEvent::TurnComplete { cost_usd, .. } => {
                match cost_usd {
                    Some(c) => println!("\n✓ turn complete (${c:.4})"),
                    None => println!("\n✓ turn complete"),
                }
                let _ = cmd_tx.send(AgentCommand::Shutdown).await;
            }
            AgentEvent::Error { message, fatal, .. } => {
                eprintln!(
                    "\n✗ error{}: {message}",
                    if fatal { " (fatal)" } else { "" }
                );
            }
            AgentEvent::Exited { code, .. } => {
                println!("● agent exited ({code:?})");
                break;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_discuss(
    prompt: String,
    drafter: String,
    critic: String,
    turns: usize,
    drafter_model: Option<String>,
    critic_model: Option<String>,
    cwd: Option<String>,
    sandbox: String,
) -> Result<(), Box<dyn std::error::Error>> {
    tales_core::agent::validate_roster(&[drafter.clone(), critic.clone()])?;
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);

    let (bus, _commands_rx) = EventBus::new(1024, 64);

    // Console frontend: render the conversation as it streams onto the bus.
    let printer = spawn_printer(&bus);

    let mut orch = Orchestrator::new(bus.clone());

    let drafter_ctx = SpawnCtx {
        agent: Uuid::new_v4(),
        label: drafter.clone(),
        cwd: cwd.clone(),
        model: drafter_model,
        permission_mode: "acceptEdits".to_string(),
        sandbox: sandbox.clone(),
        allowed_tools: None,
    };
    let critic_ctx = SpawnCtx {
        agent: Uuid::new_v4(),
        label: critic.clone(),
        cwd,
        model: critic_model,
        permission_mode: "acceptEdits".to_string(),
        sandbox,
        allowed_tools: None,
    };

    orch.add_agent(make_adapter(&drafter)?, drafter_ctx, Role::Drafter)
        .await?;
    orch.add_agent(make_adapter(&critic)?, critic_ctx, Role::Critic)
        .await?;

    println!("\n=== discussion: {prompt} ({turns} turns) ===");
    orch.run_discussion(&prompt, turns).await?;
    orch.shutdown().await;

    // Let the printer drain the final events, then stop it. We abort rather
    // than await: `orch` still holds a bus clone, so the broadcast never closes
    // on its own.
    sleep(Duration::from_millis(200)).await;
    printer.abort();

    println!("\n=== discussion complete ===");
    Ok(())
}

fn flush() {
    let _ = std::io::stdout().flush();
}

/// Spawn a console printer that renders orchestration events as they stream onto
/// the bus. Shared by `discuss` and `run`.
fn spawn_printer(bus: &EventBus) -> tokio::task::JoinHandle<()> {
    let mut events = bus.subscribe();
    tokio::spawn(async move {
        let mut labels: HashMap<Uuid, String> = HashMap::new();
        loop {
            match events.recv().await {
                Ok(ev) => match ev {
                    OrchestratorEvent::AgentSpawned { agent, label, .. } => {
                        println!("● enrolled {label}");
                        labels.insert(agent, label);
                    }
                    OrchestratorEvent::Log { msg, .. } => println!("\n──────── {msg} ────────"),
                    OrchestratorEvent::Message { agent, text } => {
                        let who = labels.get(&agent).cloned().unwrap_or_else(|| "?".into());
                        println!("\n{who}:\n{text}");
                    }
                    OrchestratorEvent::UserMessage { text } => println!("\nyou:\n{text}"),
                    OrchestratorEvent::ToolActivity { agent, summary } => {
                        let who = labels.get(&agent).cloned().unwrap_or_else(|| "?".into());
                        println!("  ⚙ {who}: {summary}");
                    }
                    OrchestratorEvent::RecommendationReady {
                        executor,
                        rationale,
                    } => {
                        println!("\n★ recommended executor: {executor}\n{rationale}");
                    }
                    OrchestratorEvent::TurnComplete {
                        cost_usd: Some(c), ..
                    } => {
                        println!("  (turn cost ${c:.4})");
                    }
                    OrchestratorEvent::Fatal { msg } => println!("✗ fatal: {msg}"),
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn mk_ctx(
    agent: Uuid,
    label: &str,
    cwd: &std::path::Path,
    model: Option<String>,
    sandbox: &str,
    permission_mode: &str,
    allowed_tools: Option<Vec<String>>,
) -> SpawnCtx {
    SpawnCtx {
        agent,
        label: label.to_string(),
        cwd: cwd.to_path_buf(),
        model,
        permission_mode: permission_mode.to_string(),
        sandbox: sandbox.to_string(),
        allowed_tools,
    }
}

async fn is_git_repo(cwd: &std::path::Path) -> bool {
    tokio::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    prompt: String,
    drafter: String,
    critic: String,
    execute: String,
    drafter_model: Option<String>,
    critic_model: Option<String>,
    turns: usize,
    cwd: Option<String>,
    sandbox: String,
    worktree: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    tales_core::agent::validate_roster(&[drafter.clone(), critic.clone()])?;
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let (bus, mut commands_rx) = EventBus::new(2048, 64);
    let commands_tx = bus.commands();

    let printer = spawn_printer(&bus);

    // The executor must be one of the two participants.
    let exec_lc = execute.to_lowercase();
    let exec_is_drafter = drafter.to_lowercase() == exec_lc;
    let exec_is_critic = critic.to_lowercase() == exec_lc;
    if !exec_is_drafter && !exec_is_critic {
        printer.abort();
        return Err(format!(
            "--execute '{execute}' must be one of the participants ('{drafter}', '{critic}')"
        )
        .into());
    }

    // Agents run on acceptEdits (Write/Edit auto-approved). The Claude executor
    // is additionally restricted to file tools so it cannot stall on an
    // unapproved Bash call in headless mode (Write creates parent dirs, so no
    // shell mkdir is needed). Non-executors and Codex are unrestricted.
    let tools_for = |label: &str| -> Option<Vec<String>> {
        if label.to_lowercase() == exec_lc && label.to_lowercase() == "claude" {
            Some(vec![
                "Write".into(),
                "Edit".into(),
                "MultiEdit".into(),
                "Read".into(),
            ])
        } else {
            None
        }
    };

    let drafter_id = Uuid::new_v4();
    let critic_id = Uuid::new_v4();
    let exec_id = if exec_is_drafter {
        drafter_id
    } else {
        critic_id
    };

    // Optional git-worktree isolation: the executor runs in its own worktree,
    // and the result is merged back after execution.
    let use_wt = worktree && is_git_repo(&cwd).await;
    if worktree && !use_wt {
        eprintln!("note: --worktree ignored (cwd is not a git repository)");
    }
    let mut wt_mgr: Option<WorktreeManager> = None;
    let mut exec_cwd = cwd.clone();
    if use_wt {
        let run_id = format!("run-{}", &exec_id.simple().to_string()[..8]);
        let mut mgr = WorktreeManager::init(&cwd, run_id).await?;
        let path = mgr.create(exec_id, &execute).await?;
        println!("● executor worktree: {}", path.display());
        exec_cwd = path;
        wt_mgr = Some(mgr);
    }
    // The orchestrator executes the FIRST roster entry whose label matches, so
    // when both roles share a label the drafter is the executor. Give the
    // worktree cwd only to that one agent; the other stays on the base cwd.
    let drafter_cwd = if exec_is_drafter {
        exec_cwd.clone()
    } else {
        cwd.clone()
    };
    let critic_cwd = if exec_is_critic && !exec_is_drafter {
        exec_cwd.clone()
    } else {
        cwd.clone()
    };

    // Run inside a block so that — no matter where it fails — we still abort the
    // printer task and prune the executor's worktree (its on-disk dir + branch).
    let run_result: Result<RunOutcome, Box<dyn std::error::Error>> = async {
        let mut orch = Orchestrator::new(bus.clone());
        orch.add_agent(
            make_adapter(&drafter)?,
            mk_ctx(
                drafter_id,
                &drafter,
                &drafter_cwd,
                drafter_model,
                &sandbox,
                "acceptEdits",
                tools_for(&drafter),
            ),
            Role::Drafter,
        )
        .await?;
        orch.add_agent(
            make_adapter(&critic)?,
            mk_ctx(
                critic_id,
                &critic,
                &critic_cwd,
                critic_model,
                &sandbox,
                "acceptEdits",
                tools_for(&critic),
            ),
            Role::Critic,
        )
        .await?;

        // Auto-confirm the executor: queued before the run, it is remembered and
        // honored when the gate opens (non-interactive equivalent of /confirm).
        commands_tx
            .send(UserCommand::ConfirmExecution {
                executor: execute.clone(),
            })
            .await?;

        println!("\n=== run: {prompt}  (executor: {execute}) ===");
        let outcome = orch
            .run_interactive(&prompt, turns, &mut commands_rx)
            .await?;
        orch.shutdown().await;
        Ok(outcome)
    }
    .await;

    sleep(Duration::from_millis(200)).await;
    printer.abort();

    // Merge the executor's worktree on success, and prune it on EVERY path.
    if let Some(mut mgr) = wt_mgr {
        if let Ok(RunOutcome::Executed { .. }) = &run_result {
            match mgr.diff(exec_id).await {
                Ok(d) => println!(
                    "\n=== executor changes ({} files) ===\n{}",
                    d.files_changed,
                    d.stat.trim()
                ),
                Err(e) => eprintln!("diff failed: {e}"),
            }
            match mgr.commit_and_merge(exec_id).await {
                Ok(MergeOutcome::Clean) => {
                    println!("✓ merged the executor's worktree into the base branch")
                }
                Ok(MergeOutcome::NoChanges) => println!("(executor produced no changes)"),
                Ok(MergeOutcome::Conflict { files }) => {
                    println!("⚠ merge conflicts — resolve in base: {}", files.join(", "))
                }
                Err(e) => eprintln!("merge failed: {e}"),
            }
        }
        if let Err(e) = mgr.remove(exec_id).await {
            eprintln!("worktree cleanup: {e}");
        }
    }

    let outcome = run_result?;
    println!("\n=== outcome: {outcome:?} ===");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_script_quotes_and_includes_flags() {
        let s = build_open_script(
            "/path/to/tales-tui",
            "/proj dir",
            &["claude".into()],
            Some("build it's thing"),
        );
        assert!(s.starts_with("#!/bin/bash\n"), "{s}");
        assert!(s.contains("cd '/proj dir' || exit 1"), "{s}");
        assert!(s.contains("exec '/path/to/tales-tui'"), "{s}");
        assert!(s.contains("--connect 'claude'"), "{s}");
        // The apostrophe in the task is safely escaped.
        assert!(s.contains("--prefill 'build it'\\''s thing'"), "{s}");
    }

    #[test]
    fn open_script_omits_empty_task() {
        let s = build_open_script("tales-tui", "/p", &["codex".into()], Some("   "));
        assert!(!s.contains("--prefill"), "{s}");
        let s2 = build_open_script("tales-tui", "/p", &["codex".into()], None);
        assert!(!s2.contains("--prefill"), "{s2}");
    }
}
