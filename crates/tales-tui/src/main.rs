//! `tales-tui` — the live chat interface.
//!
//! Watch Claude and Codex discuss in real time, see the recommendation and
//! outcomes, and type into the same chat to steer them — you are the
//! human-in-the-loop. The core orchestration runs in a background task wired to
//! the same event/command bus the UI uses; the UI is just one frontend.

mod app;

use std::io::Stdout;
use std::path::PathBuf;

use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use uuid::Uuid;

use tales_core::agent::claude::ClaudeAdapter;
use tales_core::agent::codex::CodexAdapter;
use tales_core::agent::mock::MockAdapter;
use tales_core::agent::{AgentAdapter, SpawnCtx};
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::{OrchestratorEvent, UserCommand};
use tales_core::orchestrator::Orchestrator;

use crate::app::App;

#[derive(Parser, Debug, Clone)]
#[command(name = "tales-tui", about = "Live chat: watch & steer Claude and Codex collaborate")]
struct Args {
    /// The task the agents discuss (and, on your confirmation, execute).
    task: String,
    /// Drafter agent: claude | codex.
    #[arg(long, default_value = "claude")]
    drafter: String,
    /// Critic agent: claude | codex.
    #[arg(long, default_value = "codex")]
    critic: String,
    /// Model for the drafter (optional).
    #[arg(long)]
    drafter_model: Option<String>,
    /// Model for the critic (optional).
    #[arg(long)]
    critic_model: Option<String>,
    /// Total discussion turns before the recommendation.
    #[arg(long, default_value_t = 4)]
    turns: usize,
    /// Working directory the agents operate in.
    #[arg(long)]
    cwd: Option<String>,
    /// Codex sandbox policy.
    #[arg(long, default_value = "workspace-write")]
    sandbox: String,
    /// Run with scripted mock agents — no API calls. Great for trying the UI.
    #[arg(long)]
    demo: bool,
}

/// Restores the terminal on drop, even on panic.
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Bus shared by the engine (background task) and the UI (this task).
    let (bus, commands_rx) = EventBus::new(4096, 256);

    // Launch the orchestration session.
    {
        let bus = bus.clone();
        let args = args.clone();
        tokio::spawn(async move {
            run_session(bus, commands_rx, args).await;
        });
    }

    // Terminal up.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let res = run_ui(&mut terminal, &bus, args.task.clone()).await;

    // Terminal restored by `_guard` on drop.
    res
}

async fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    bus: &EventBus,
    task: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(task);
    let commands = bus.commands();
    let mut events = bus.subscribe();
    let mut keys = EventStream::new();

    loop {
        terminal.draw(|f| draw(f, &app))?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe_key = keys.next() => {
                // Input stream closed (EOF / terminal gone) — exit instead of
                // busy-looping on a perpetually-ready None.
                if maybe_key.is_none() {
                    let _ = commands.send(UserCommand::Shutdown).await;
                    break;
                }
                if let Some(Ok(Event::Key(key))) = maybe_key {
                    if key.kind == KeyEventKind::Release { continue; }
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            let _ = commands.send(UserCommand::Shutdown).await;
                            break;
                        }
                        (KeyCode::Enter, _) => {
                            if let Some(cmd) = app.submit_input() {
                                let _ = commands.send(cmd).await;
                            }
                        }
                        (KeyCode::Backspace, _) => { app.input.pop(); }
                        (KeyCode::Esc, _) => { app.input.clear(); }
                        (KeyCode::Char(c), _) => { app.input.push(c); }
                        _ => {}
                    }
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(e) => app.apply(e),
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => {}
                }
            }
        }
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .split(f.area());

    // Header.
    let header = Paragraph::new(format!(
        "Tales · task: {}   ·   phase: {}",
        app.task, app.phase
    ))
    .style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(header, chunks[0]);

    // Conversation (auto-tailed to the bottom).
    let body = chunks[1];
    let inner_w = body.width.saturating_sub(2) as usize;
    let inner_h = body.height.saturating_sub(2) as usize;
    let mut lines = app.render_lines(inner_w);
    if lines.len() > inner_h {
        lines = lines.split_off(lines.len() - inner_h);
    }
    let convo = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("conversation"));
    f.render_widget(convo, body);

    // Input.
    let title = "you — type to talk · /confirm [agent] · /reject · /quit · Ctrl-C";
    let input = Paragraph::new(format!("> {}", app.input))
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(input, chunks[2]);
}

async fn run_session(bus: EventBus, mut commands_rx: mpsc::Receiver<UserCommand>, args: Args) {
    let mut orch = Orchestrator::new(bus.clone());

    let result = async {
        let cwd = args
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);

        if args.demo {
            let vote = r#"{"recommended_executor":"claude","confidence":0.9,"rationale":"strongest at writing the code"}"#;
            let drafter = MockAdapter::new(vec![
                "Draft plan:\n- token-bucket limiter per API key\n- Redis-backed counters\n- 429 + Retry-After".into(),
                vote.into(),
                "Executed: created limiter.rs and wired the middleware.".into(),
            ]);
            let critic = MockAdapter::new(vec![
                "Concern: token-bucket vs fixed-window mismatch; what's the burst size? Who is unauthenticated identity?".into(),
                r#"{"recommended_executor":"claude","confidence":0.7,"rationale":"agree"}"#.into(),
            ]);
            orch.add_agent(Box::new(drafter), ctx(Uuid::new_v4(), "claude", &cwd, None, &args.sandbox), Role::Drafter).await?;
            orch.add_agent(Box::new(critic), ctx(Uuid::new_v4(), "codex", &cwd, None, &args.sandbox), Role::Critic).await?;
        } else {
            orch.add_agent(
                make_adapter(&args.drafter).map_err(tales_core::TalesError::Other)?,
                ctx(Uuid::new_v4(), &args.drafter, &cwd, args.drafter_model.clone(), &args.sandbox),
                Role::Drafter,
            )
            .await?;
            orch.add_agent(
                make_adapter(&args.critic).map_err(tales_core::TalesError::Other)?,
                ctx(Uuid::new_v4(), &args.critic, &cwd, args.critic_model.clone(), &args.sandbox),
                Role::Critic,
            )
            .await?;
        }

        orch.run_interactive(&args.task, args.turns, &mut commands_rx).await
    }
    .await;

    match result {
        Ok(outcome) => bus.emit(OrchestratorEvent::Log {
            level: "info".to_string(),
            msg: format!("run finished: {outcome:?}"),
        }),
        Err(e) => bus.emit(OrchestratorEvent::Fatal { msg: e.to_string() }),
    }
    orch.shutdown().await;
}

fn ctx(agent: Uuid, label: &str, cwd: &PathBuf, model: Option<String>, sandbox: &str) -> SpawnCtx {
    SpawnCtx {
        agent,
        label: label.to_string(),
        cwd: cwd.clone(),
        model,
        permission_mode: "acceptEdits".to_string(),
        sandbox: sandbox.to_string(),
        allowed_tools: None,
    }
}

fn make_adapter(name: &str) -> Result<Box<dyn AgentAdapter>, String> {
    match name {
        "codex" => Ok(Box::new(CodexAdapter::new())),
        "claude" => Ok(Box::new(ClaudeAdapter::new())),
        other => Err(format!("unknown agent '{other}' (expected: claude | codex)")),
    }
}
