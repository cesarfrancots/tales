//! `tales-tui` — the interactive Tales terminal workspace.
//!
//! Run `tales` (or `tales-tui`) with no task and it opens a Warp-style terminal
//! workspace: the default pane is the Tales orchestrator, and sibling panes can
//! run shells, Codex, Claude Code, or Open Code. Use `--classic` for the older
//! connect → prompt → plan screen. Pass a task positionally (or `--demo`) to
//! skip setup and start the planner immediately.
//!
//! The core orchestration runs in a background task wired to the same
//! event/command bus the UI uses; the UI is just one frontend.

mod app;
mod connect;
mod prompt;
mod terminal_app;
mod theme;

use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use uuid::Uuid;

use tales_core::agent::mock::MockAdapter;
use tales_core::agent::{make_adapter, validate_roster, SpawnCtx};
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::{OrchestratorEvent, UserCommand};
use tales_core::orchestrator::Orchestrator;

use crate::app::App;
use crate::connect::{ConnectScreen, ToolChoice};
use crate::prompt::{PromptOutcome, PromptScreen};
use crate::theme::{ACCENT, DIM, TEXT};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "tales-tui",
    about = "Interactive Tales terminal workspace: plan in Tales, run shells and agent CLIs beside it"
)]
struct Args {
    /// The task to plan (and, on your confirmation, execute). Omit it to open
    /// the terminal workspace with Tales as the default pane.
    task: Option<String>,
    /// Pre-select tools for the Tales orchestrator roster (repeatable):
    /// `--connect claude --connect codex`.
    #[arg(long = "connect")]
    connect: Vec<String>,
    /// Pre-fill the Tales orchestrator prompt in terminal-workspace mode.
    #[arg(long)]
    prefill: Option<String>,
    /// Drafter agent for immediate (task-given) mode: claude | codex | opencode.
    #[arg(long, default_value = "claude")]
    drafter: String,
    /// Critic agent for immediate (task-given) mode: claude | codex | opencode.
    #[arg(long, default_value = "codex")]
    critic: String,
    /// Model for the drafter (optional).
    #[arg(long)]
    drafter_model: Option<String>,
    /// Model for the critic (optional).
    #[arg(long)]
    critic_model: Option<String>,
    /// Reasoning effort for the drafter (e.g. Codex low|medium|high).
    #[arg(long)]
    drafter_effort: Option<String>,
    /// Reasoning effort for the critic (e.g. Codex low|medium|high).
    #[arg(long)]
    critic_effort: Option<String>,
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
    /// Open the original connect → prompt → plan screen instead of the
    /// Warp-style terminal workspace.
    #[arg(long)]
    classic: bool,
}

/// One connected tool and the role it plays in the discussion.
#[derive(Clone)]
struct Connection {
    tool: String,
    role: Role,
    model: Option<String>,
    effort: Option<String>,
}

/// A two-agent roster (drafter, critic) by tool key, models/efforts defaulted.
fn fixed_roster(drafter: &str, critic: &str) -> Vec<Connection> {
    vec![
        Connection {
            tool: drafter.to_string(),
            role: Role::Drafter,
            model: None,
            effort: None,
        },
        Connection {
            tool: critic.to_string(),
            role: Role::Critic,
            model: None,
            effort: None,
        },
    ]
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

    // Terminal up — held across every screen by one guard.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;
    let mut keys = EventStream::new();

    if args.task.is_none() && !args.demo && !args.classic {
        return terminal_app::run_terminal_workspace(&mut terminal, &mut keys, &args).await;
    }

    // Decide the roster + task: immediate (task or --demo given) or interactive.
    let setup = if args.demo {
        // The demo is a fixed canned script (claude drafter, codex critic) — its
        // mock votes name "claude", so the roster/candidates must match exactly,
        // regardless of any --drafter/--critic flags.
        let task = args
            .task
            .clone()
            .unwrap_or_else(|| "Plan and improve this project".to_string());
        Some((fixed_roster("claude", "codex"), task))
    } else if let Some(task) = args.task.clone() {
        Some((
            vec![
                Connection {
                    tool: args.drafter.clone(),
                    role: Role::Drafter,
                    model: args.drafter_model.clone(),
                    effort: args.drafter_effort.clone(),
                },
                Connection {
                    tool: args.critic.clone(),
                    role: Role::Critic,
                    model: args.critic_model.clone(),
                    effort: args.critic_effort.clone(),
                },
            ],
            task,
        ))
    } else {
        interactive_setup(&mut terminal, &mut keys, &args).await?
    };

    // User quit before starting — the guard restores the terminal on return.
    let Some((roster, task)) = setup else {
        return Ok(());
    };

    // Spawn the engine now that we know who's connected and what to build.
    let candidates: Vec<String> = roster.iter().map(|c| c.tool.clone()).collect();
    let cwd = args
        .cwd
        .clone()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    // Subscribe BEFORE the engine starts so the first "joined" events (emitted as
    // agents enroll) can't be missed by a late subscriber.
    let events = bus.subscribe();
    {
        let bus = bus.clone();
        let task = task.clone();
        let sandbox = args.sandbox.clone();
        let turns = args.turns;
        let demo = args.demo;
        tokio::spawn(async move {
            run_session(bus, commands_rx, roster, task, cwd, sandbox, turns, demo).await;
        });
    }

    // Terminal restored by `_guard` on drop.
    run_ui(&mut terminal, &mut keys, &bus, events, task, candidates).await
}

/// The interactive setup: connect screen → prompt screen, looping if the user
/// backs out of the prompt to re-pick tools. Returns `None` if they quit.
async fn interactive_setup(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    keys: &mut EventStream,
    args: &Args,
) -> Result<Option<(Vec<Connection>, String)>, Box<dyn std::error::Error>> {
    // Carried across Back so re-picking tools doesn't reset the user's choices
    // or wipe the prompt they already typed.
    let mut preselect = if args.connect.is_empty() {
        vec!["claude".to_string(), "codex".to_string()]
    } else {
        args.connect.clone()
    };
    let mut prefill = args.prefill.clone().unwrap_or_default();

    loop {
        let chosen = match run_connect(terminal, keys, &preselect).await? {
            Some(c) => c,
            None => return Ok(None),
        };
        let keys_only: Vec<String> = chosen.iter().map(|c| c.key.clone()).collect();
        preselect = keys_only.clone();
        match run_prompt(terminal, keys, &keys_only, &prefill).await? {
            PromptOutcome::Start(task) => {
                let roster = chosen
                    .iter()
                    .enumerate()
                    .map(|(i, c)| Connection {
                        tool: c.key.clone(),
                        role: if i == 0 { Role::Drafter } else { Role::Critic },
                        model: c.model.clone(),
                        effort: c.effort.clone(),
                    })
                    .collect();
                return Ok(Some((roster, task)));
            }
            PromptOutcome::Back(typed) => {
                prefill = typed;
                continue;
            }
            PromptOutcome::Quit => return Ok(None),
        }
    }
}

/// Run the connect-your-tools checklist. Returns the chosen tool keys, or `None`
/// if the user quit.
async fn run_connect(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    keys: &mut EventStream,
    preselect: &[String],
) -> Result<Option<Vec<ToolChoice>>, Box<dyn std::error::Error>> {
    let mut screen = ConnectScreen::new(preselect);
    loop {
        terminal.draw(|f| screen.draw(f))?;
        match keys.next().await {
            None => return Ok(None),
            Some(Ok(Event::Key(k))) => {
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(None),
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(None),
                    (KeyCode::Up, _) | (KeyCode::Char('k'), _) => screen.up(),
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => screen.down(),
                    (KeyCode::Char(' '), _) => screen.toggle(),
                    (KeyCode::Char('m'), _) => screen.cycle_model(),
                    (KeyCode::Char('e'), _) => screen.cycle_effort(),
                    (KeyCode::Enter, _) => {
                        if let Some(chosen) = screen.confirm() {
                            return Ok(Some(chosen));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Run the prompt entry screen. Returns the user's choice (start / back / quit).
async fn run_prompt(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    keys: &mut EventStream,
    connected: &[String],
    prefill: &str,
) -> Result<PromptOutcome, Box<dyn std::error::Error>> {
    let mut screen = PromptScreen::new(connected, Some(prefill));
    loop {
        terminal.draw(|f| screen.draw(f))?;
        match keys.next().await {
            None => return Ok(PromptOutcome::Quit),
            Some(Ok(Event::Key(k))) => {
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(PromptOutcome::Quit),
                    (KeyCode::Esc, _) => return Ok(PromptOutcome::Back(screen.input.clone())),
                    (KeyCode::Enter, _) => {
                        let task = screen.input.trim().to_string();
                        if !task.is_empty() {
                            return Ok(PromptOutcome::Start(task));
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        screen.input.pop();
                    }
                    (KeyCode::Char(c), _) => screen.input.push(c),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

async fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    keys: &mut EventStream,
    bus: &EventBus,
    mut events: tokio::sync::broadcast::Receiver<OrchestratorEvent>,
    task: String,
    candidates: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(task);
    app.set_candidates(candidates);
    let commands = bus.commands();

    loop {
        app.tick(); // advance the smooth-reveal + spinner animation
        terminal.draw(|f| draw(f, &app))?;
        if app.should_quit {
            break;
        }

        // Wake at ~30fps while text is animating (so reveal stays smooth even
        // when no events arrive), and idle slowly otherwise. Keys/events still
        // wake the loop immediately.
        let frame = if app.is_animating() {
            Duration::from_millis(33)
        } else {
            Duration::from_millis(200)
        };

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
                        // At the gate, a bare digit picks that executor; otherwise type it.
                        (KeyCode::Char(c), _) => {
                            if let Some(cmd) = app.gate_pick(c) {
                                let _ = commands.send(cmd).await;
                            } else {
                                app.input.push(c);
                            }
                        }
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
            _ = tokio::time::sleep(frame) => {}
        }
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let bold = Modifier::BOLD;

    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(1),    // transcript
        Constraint::Length(1), // input
        Constraint::Length(1), // hint
    ])
    .split(f.area());

    // Header: "❯ tales · <task>" left, "[phase]" right — same row.
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯", Style::default().fg(ACCENT).add_modifier(bold)),
            Span::styled(" tales ", Style::default().fg(TEXT).add_modifier(bold)),
            Span::styled(format!("· {}", app.task), Style::default().fg(DIM)),
        ])),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("[{}] ", app.phase),
            Style::default().fg(ACCENT),
        )))
        .alignment(Alignment::Right),
        chunks[0],
    );

    // Transcript, auto-tailed to the bottom (borderless).
    let body = chunks[1];
    let inner_w = body.width as usize;
    let inner_h = body.height as usize;
    let mut lines = app.render_lines(inner_w);
    if lines.len() > inner_h {
        lines = lines.split_off(lines.len() - inner_h);
    }
    f.render_widget(Paragraph::new(lines), body);

    // Input line (with a 📎N indicator when media is queued).
    let mut input_spans = vec![Span::styled(
        "❯ ",
        Style::default().fg(ACCENT).add_modifier(bold),
    )];
    if app.pending_count() > 0 {
        input_spans.push(Span::styled(
            format!("📎{} ", app.pending_count()),
            Style::default().fg(ACCENT),
        ));
    }
    input_spans.push(Span::styled(app.input.clone(), Style::default().fg(TEXT)));
    f.render_widget(Paragraph::new(Line::from(input_spans)), chunks[2]);

    // Hint — becomes the executor picker at the gate.
    f.render_widget(Paragraph::new(app.footer_line()), chunks[3]);
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    bus: EventBus,
    mut commands_rx: mpsc::Receiver<UserCommand>,
    roster: Vec<Connection>,
    task: String,
    cwd: PathBuf,
    sandbox: String,
    turns: usize,
    demo: bool,
) {
    let mut orch = Orchestrator::new(bus.clone());

    let result = async {
        if demo {
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
            // Enroll under the roster's labels so they match the gate's
            // numbered executor picker (the demo roster is claude/codex).
            let d_label = roster
                .first()
                .map(|c| c.tool.clone())
                .unwrap_or_else(|| "claude".into());
            let c_label = roster
                .get(1)
                .map(|c| c.tool.clone())
                .unwrap_or_else(|| "codex".into());
            orch.add_agent(Box::new(drafter), ctx(Uuid::new_v4(), &d_label, &cwd, None, None, &sandbox), Role::Drafter).await?;
            orch.add_agent(Box::new(critic), ctx(Uuid::new_v4(), &c_label, &cwd, None, None, &sandbox), Role::Critic).await?;
        } else {
            let keys: Vec<String> = roster.iter().map(|c| c.tool.clone()).collect();
            validate_roster(&keys)?;
            for c in &roster {
                let adapter = make_adapter(&c.tool)?;
                orch.add_agent(
                    adapter,
                    ctx(
                        Uuid::new_v4(),
                        &c.tool,
                        &cwd,
                        c.model.clone(),
                        c.effort.clone(),
                        &sandbox,
                    ),
                    c.role,
                )
                .await?;
            }
        }

        orch.run_interactive(&task, turns, &mut commands_rx).await
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

fn ctx(
    agent: Uuid,
    label: &str,
    cwd: &Path,
    model: Option<String>,
    effort: Option<String>,
    sandbox: &str,
) -> SpawnCtx {
    SpawnCtx {
        agent,
        label: label.to_string(),
        cwd: cwd.to_path_buf(),
        model,
        effort,
        permission_mode: "acceptEdits".to_string(),
        sandbox: sandbox.to_string(),
        allowed_tools: None,
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use uuid::Uuid;

    /// Render a representative frame to a text grid so the Warp-style layout can
    /// be eyeballed (`cargo test -p tales-tui -- --nocapture snapshot`).
    #[test]
    fn snapshot() {
        let mut app = App::new("Design a rate limiter for a public API".into());
        app.set_candidates(vec!["claude".into(), "codex".into()]);
        let c = Uuid::new_v4();
        let x = Uuid::new_v4();
        app.apply(OrchestratorEvent::AgentSpawned {
            agent: c,
            label: "claude".into(),
            session_id: String::new(),
        });
        app.apply(OrchestratorEvent::AgentSpawned {
            agent: x,
            label: "codex".into(),
            session_id: String::new(),
        });
        app.apply(OrchestratorEvent::TurnStarted {
            agent: c,
            role: "Drafter".into(),
        });
        app.apply(OrchestratorEvent::Message {
            agent: c,
            text: "Draft plan:\n- token-bucket per API key\n- 429 + Retry-After header".into(),
        });
        app.apply(OrchestratorEvent::TurnStarted {
            agent: x,
            role: "Critic".into(),
        });
        app.apply(OrchestratorEvent::Message {
            agent: x,
            text: "Which algorithm — token-bucket or fixed-window? And the burst size?".into(),
        });
        app.apply(OrchestratorEvent::RecommendationReady {
            executor: "claude".into(),
            rationale: "best at writing the code".into(),
        });
        app.phase = "awaitingconfirmation".into();
        app.awaiting = true;
        app.input = "focus on abuse cases".into();
        app.advance(10.0); // reveal the buffered message bodies

        let mut term = ratatui::Terminal::new(TestBackend::new(74, 22)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        eprintln!("\n{s}");
        assert!(s.contains("Claude Code"), "{s}");
        assert!(s.contains("Codex"));
        assert!(s.contains("DRAFTER"));
        assert!(s.contains("recommend"));
    }
}
