//! The orchestration engine — runs the multi-agent discussion.
//!
//! Agents are enrolled via [`Orchestrator::add_agent`], each spawned through its
//! [`AgentAdapter`] with a clone of one shared event sender (fan-in). The
//! [`Conductor`] decides whose turn it is; the engine relays each agent's output
//! into the [`Blackboard`] and forward as the next speaker's input, bridging
//! normalized [`AgentEvent`]s into [`OrchestratorEvent`]s on the bus so any
//! frontend can render the conversation live.
//!
//! M4 covers the discussion loop. The recommendation/confirmation gate (M5) and
//! gated execution + merge (M6) build on this same engine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::agent::{AgentAdapter, AgentCommand, AgentEvent, Attachment, SpawnCtx};
use crate::blackboard::Blackboard;
use crate::bus::EventBus;
use crate::conductor::{Conductor, Role, RosterEntry, RuleConductor};
use crate::event::{OrchestratorEvent, UserCommand};
use crate::project_context::{local_change_summary_status_json, LocalChangeSummary};
use crate::recommend::{
    aggregate, parse_vote_lenient, ExecutionVote, Recommendation, VoteParseSource,
};
use crate::{AgentId, Result, TalesError, TokenUsage};

/// Default prompt context budget for stateless adapters. Resumable adapters keep
/// their native session history, so this mainly caps re-sent transcripts for
/// generic CLIs and one-shot execution prompts.
pub const DEFAULT_CONTEXT_BUDGET_CHARS: usize = 24_000;
const VOTE_REPAIR_PREVIEW_CHARS: usize = 1_200;

/// How an interactive run ended.
#[derive(Clone, Debug)]
pub enum RunOutcome {
    /// The discussion finished without entering the execution gate.
    Discussed,
    /// The user confirmed; the executor ran and produced this output.
    Executed { executor: String, output: String },
    /// The user rejected the plan.
    Rejected,
    /// The run was shut down / the command channel closed.
    Aborted,
}

#[derive(Clone, Debug, Default)]
struct PromptStats {
    prompts: usize,
    chars_sent: usize,
    max_chars: usize,
}

/// Metadata about the cached project context injected into planning prompts.
#[derive(Clone, Debug)]
pub struct ProjectContextReport {
    pub cache_hit: bool,
    pub cache_path: String,
    pub chars: usize,
    pub max_files: usize,
    pub max_manifest_chars: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PromptPhase {
    Planning,
    PlanningSynthesis,
    Recommendation,
    VoteRepair,
    Execution,
}

impl PromptPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::PlanningSynthesis => "planning_synthesis",
            Self::Recommendation => "recommendation_vote",
            Self::VoteRepair => "vote_repair",
            Self::Execution => "execution",
        }
    }
}

/// The phase of the run. The phase ordering is enforced: the engine can only
/// enter [`Phase::Executing`] from [`Phase::AwaitingConfirmation`] via an
/// explicit user confirmation — there is no auto-execute path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Planning,
    Recommending,
    AwaitingConfirmation,
    Executing,
    Done,
}

/// Drives a set of agents through a discussion and out onto the bus.
pub struct Orchestrator {
    bus: EventBus,
    roster: Vec<RosterEntry>,
    cmd_txs: HashMap<AgentId, mpsc::Sender<AgentCommand>>,
    events_tx: mpsc::Sender<AgentEvent>,
    events_rx: mpsc::Receiver<AgentEvent>,
    blackboard: Blackboard,
    phase: Phase,
    /// Max wall-clock a single agent turn may take before the engine gives up
    /// on it — a liveness backstop so no adapter bug can deadlock the run.
    turn_timeout: Duration,
    /// A Confirm/Reject received before the gate opened, remembered so an early
    /// decision is honored rather than discarded.
    pending_decision: Option<UserCommand>,
    /// Media the human attached, shared with every agent. `media_delivered`
    /// tracks how many of these each agent has already received, so each file
    /// is sent to each agent exactly once (on its next turn).
    pending_media: Vec<Attachment>,
    media_delivered: HashMap<AgentId, usize>,
    /// Skills each connected tool exposes (discovered at startup).
    skills: HashMap<AgentId, Vec<String>>,
    /// Agents shut down after a turn timeout — never scheduled again (sending to
    /// a dead command channel would otherwise abort the whole run).
    terminated: std::collections::HashSet<AgentId>,
    /// Whether each agent's adapter resumes its session across turns (so its own
    /// history lives server-side and need not be re-pasted into every prompt).
    /// Captured from `AgentCaps::resumable` at enrollment; absent ⇒ false.
    resumable: HashMap<AgentId, bool>,
    /// How many transcript utterances each agent has already been shown, so a
    /// resumable agent receives only the new tail (delta) — not the whole
    /// transcript — each turn. Used by the sequential planning loop.
    last_seen: HashMap<AgentId, usize>,
    /// Run the planning discussion as parallel rounds (planners speak
    /// concurrently) instead of sequential ping-pong. Off by default; the
    /// human-in-the-loop `run_interactive` stays sequential regardless.
    parallel_rounds: bool,
    /// Maximum characters of old transcript/plan context to paste into prompts
    /// that must carry context explicitly. `None` disables trimming.
    context_budget_chars: Option<usize>,
    /// Latest executor recommendation, retained so the execution prompt can
    /// include a compact decision packet instead of the full debate transcript.
    last_recommendation: Option<Recommendation>,
    /// Latest session-cumulative USD cost reported by each adapter, if any.
    latest_cost_usd: HashMap<AgentId, f64>,
    /// Provider-reported token usage accumulated from adapters that expose it.
    reported_token_usage: HashMap<AgentId, TokenUsage>,
    /// Local prompt-size telemetry, measured before sending prompts to adapters.
    prompt_stats: HashMap<AgentId, PromptStats>,
    /// Same telemetry aggregated by run phase, so reports show where prompt budget
    /// is being spent.
    prompt_phase_stats: HashMap<PromptPhase, PromptStats>,
    /// Cached project map/manifest summary injected into first planning prompts
    /// so agents do not spend tokens rediscovering the repo shape every run.
    project_context: Option<String>,
    /// Report-only metadata for the cached context above.
    project_context_report: Option<ProjectContextReport>,
    /// Compact local git status snapshot captured before agents execute, used
    /// for cheaper handoff/review context and to warn executors about dirty
    /// files they should preserve.
    local_changes: Option<LocalChangeSummary>,
    /// Count of non-JSON votes salvaged from an unambiguous candidate mention,
    /// avoiding a repair prompt while still recording the structured-output gap.
    salvaged_votes: usize,
}

impl Orchestrator {
    pub fn new(bus: EventBus) -> Self {
        let (events_tx, events_rx) = mpsc::channel(256);
        Self {
            bus,
            roster: Vec::new(),
            cmd_txs: HashMap::new(),
            events_tx,
            events_rx,
            blackboard: Blackboard::default(),
            phase: Phase::Idle,
            turn_timeout: Duration::from_secs(300),
            pending_decision: None,
            pending_media: Vec::new(),
            media_delivered: HashMap::new(),
            skills: HashMap::new(),
            terminated: std::collections::HashSet::new(),
            resumable: HashMap::new(),
            last_seen: HashMap::new(),
            parallel_rounds: false,
            context_budget_chars: Some(DEFAULT_CONTEXT_BUDGET_CHARS),
            last_recommendation: None,
            latest_cost_usd: HashMap::new(),
            reported_token_usage: HashMap::new(),
            prompt_stats: HashMap::new(),
            prompt_phase_stats: HashMap::new(),
            project_context: None,
            project_context_report: None,
            local_changes: None,
            salvaged_votes: 0,
        }
    }

    /// Skills discovered for each connected tool.
    pub fn skills(&self) -> &HashMap<AgentId, Vec<String>> {
        &self.skills
    }

    fn record_skills(&mut self, agent: AgentId, skills: Vec<String>) {
        let label = self
            .roster
            .iter()
            .find(|r| r.agent == agent)
            .map(|r| r.label.clone())
            .unwrap_or_default();
        let n = skills.len();
        let preview: Vec<String> = skills.iter().take(14).cloned().collect();
        let more = if n > 14 {
            format!(" +{} more", n - 14)
        } else {
            String::new()
        };
        self.bus.emit(OrchestratorEvent::Log {
            level: "skills".to_string(),
            msg: format!("{label} has {n} skills: {}{more}", preview.join(", ")),
        });
        self.skills.insert(agent, skills);
    }

    fn emit_turn_complete(
        &mut self,
        agent: AgentId,
        cost_usd: Option<f64>,
        token_usage: Option<TokenUsage>,
    ) {
        if let Some(cost) = cost_usd {
            self.latest_cost_usd.insert(agent, cost);
        }
        if let Some(usage) = token_usage {
            self.reported_token_usage
                .entry(agent)
                .or_default()
                .add(usage);
        }
        self.bus.emit(OrchestratorEvent::TurnComplete {
            agent,
            cost_usd,
            token_usage,
        });
    }

    fn record_prompt_sent(&mut self, agent: AgentId, phase: PromptPhase, prompt: &str) {
        let chars = prompt.chars().count();
        let stats = self.prompt_stats.entry(agent).or_default();
        stats.prompts += 1;
        stats.chars_sent += chars;
        stats.max_chars = stats.max_chars.max(chars);

        let phase_stats = self.prompt_phase_stats.entry(phase).or_default();
        phase_stats.prompts += 1;
        phase_stats.chars_sent += chars;
        phase_stats.max_chars = phase_stats.max_chars.max(chars);
    }

    /// Attachments this agent hasn't yet received (marks them delivered).
    fn media_for(&mut self, agent: AgentId) -> Vec<Attachment> {
        let delivered = self.media_delivered.entry(agent).or_insert(0);
        if *delivered >= self.pending_media.len() {
            return Vec::new();
        }
        let out = self.pending_media[*delivered..].to_vec();
        *delivered = self.pending_media.len();
        out
    }

    /// Override the per-turn timeout (default 300s).
    pub fn set_turn_timeout(&mut self, timeout: Duration) {
        self.turn_timeout = timeout;
    }

    /// Whether this agent's adapter resumes its session across turns. Absent ⇒
    /// false (the safe, full-context fallback used by stateless adapters).
    fn is_resumable(&self, agent: AgentId) -> bool {
        *self.resumable.get(&agent).unwrap_or(&false)
    }

    /// Run the planning discussion as parallel rounds — planners speak
    /// concurrently, so a round costs `max` latency instead of `sum`. Off by
    /// default; the human-in-the-loop path stays sequential regardless.
    pub fn set_parallel_rounds(&mut self, on: bool) {
        self.parallel_rounds = on;
    }

    /// Cap the explicit context pasted into prompts for stateless agents and
    /// separate executors. `None` disables trimming. Resumable agents still use
    /// their session state and receive lean prompts.
    pub fn set_context_budget_chars(&mut self, chars: Option<usize>) {
        self.context_budget_chars = chars.filter(|n| *n > 0);
    }

    /// Set cached project context for first-turn planning prompts.
    pub fn set_project_context(&mut self, context: Option<String>) {
        self.project_context = context.and_then(|text| {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        if self.project_context.is_none() {
            self.project_context_report = None;
        }
    }

    /// Attach report-only metadata for the cached project context.
    pub fn set_project_context_report(&mut self, report: Option<ProjectContextReport>) {
        self.project_context_report = report.filter(|_| self.project_context.is_some());
    }

    /// Set a compact local git status snapshot for execution handoff/reporting.
    pub fn set_local_changes(&mut self, changes: Option<LocalChangeSummary>) {
        self.local_changes = changes;
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        let phase_name = match phase {
            Phase::Planning => "discussion".to_string(),
            _ => format!("{phase:?}").to_lowercase(),
        };
        self.bus
            .emit(OrchestratorEvent::PhaseChanged { phase: phase_name });
    }

    /// Enroll an agent: spawn its adapter (fanning events into the shared
    /// stream) and register it in the roster with a discussion role.
    pub async fn add_agent(
        &mut self,
        mut adapter: Box<dyn AgentAdapter>,
        ctx: SpawnCtx,
        role: Role,
    ) -> Result<AgentId> {
        if ctx.label.trim().is_empty() {
            return Err(TalesError::Other(
                "agent label must be non-empty".to_string(),
            ));
        }
        let agent = ctx.agent;
        let label = ctx.label.clone();
        // Capture caps BEFORE spawn moves the adapter — drives delta/lean prompts.
        let resumable = adapter.caps().resumable;
        let cmd_tx = adapter.spawn(ctx, self.events_tx.clone()).await?;
        self.cmd_txs.insert(agent, cmd_tx);
        self.resumable.insert(agent, resumable);
        self.roster.push(RosterEntry {
            agent,
            label: label.clone(),
            role,
        });
        self.bus.emit(OrchestratorEvent::AgentSpawned {
            agent,
            label,
            session_id: String::new(),
        });
        Ok(agent)
    }

    /// Read-only access to the accumulated discussion.
    pub fn blackboard(&self) -> &Blackboard {
        &self.blackboard
    }

    /// Run the drafter/critic discussion for up to `max_turns` turns, returning
    /// the full transcript text.
    pub async fn run_discussion(&mut self, task: &str, max_turns: usize) -> Result<String> {
        self.blackboard.task = task.to_string();
        self.set_phase(Phase::Planning);
        if self.parallel_rounds {
            self.run_discussion_parallel(task, max_turns).await?;
        } else {
            self.run_discussion_sequential(task, max_turns).await?;
        }
        self.emit_session_report(&RunOutcome::Discussed);
        Ok(self.blackboard.transcript_text())
    }

    /// Sequential ping-pong (drafter→critic→…). Resumable planners get only the
    /// unseen delta each turn; stateless ones get the full transcript.
    async fn run_discussion_sequential(&mut self, task: &str, max_turns: usize) -> Result<()> {
        let mut conductor = RuleConductor::new(self.roster.clone(), max_turns);

        while let Some(plan) = conductor.next_turn(&self.blackboard) {
            // Skip an agent that was terminated after a timeout — its channel is
            // dead and a send would abort the run.
            if self.terminated.contains(&plan.agent) {
                continue;
            }
            let resumable = self.is_resumable(plan.agent);
            let seen = *self.last_seen.get(&plan.agent).unwrap_or(&0);
            let first_time = !self.last_seen.contains_key(&plan.agent);
            let delta = self.blackboard.transcript_text_from(seen);
            let prompt = compose_prompt(
                plan.role,
                task,
                &self.blackboard,
                resumable,
                &delta,
                first_time,
                self.context_budget_chars,
                self.project_context.as_deref(),
            );
            self.bus.emit(OrchestratorEvent::Log {
                level: "info".to_string(),
                msg: format!("→ {} speaking as {:?}", plan.label, plan.role),
            });

            let tx = self
                .cmd_txs
                .get(&plan.agent)
                .ok_or_else(|| TalesError::Other(format!("no command channel for {}", plan.agent)))?
                .clone();
            self.record_prompt_sent(plan.agent, PromptPhase::Planning, &prompt);
            tx.send(AgentCommand::StartTurn {
                prompt,
                attachments: Vec::new(),
            })
            .await
            .map_err(|e| TalesError::Other(format!("send to agent failed: {e}")))?;

            let text = self.collect_turn(plan.agent).await?;
            self.blackboard
                .record(plan.label.clone(), plan.role, no_output_placeholder(text));
            // The agent has now seen everything up to and including its own turn.
            self.last_seen
                .insert(plan.agent, self.blackboard.transcript.len());
        }
        Ok(())
    }

    /// Parallel rounds: planners speak CONCURRENTLY, so a round costs `max`
    /// latency, not `sum`. Round 1 = independent drafts; later rounds = the
    /// roster-first planner SYNTHESIZES a merged plan while the other(s) run an
    /// adversarial cross-review. Results are recorded in roster order so the
    /// transcript stays deterministic regardless of who finished first.
    async fn run_discussion_parallel(&mut self, task: &str, max_turns: usize) -> Result<()> {
        let rounds = self.round_count(max_turns);
        let mut prev: HashMap<AgentId, String> = HashMap::new();
        for round in 0..rounds {
            if self.active_planner_count() == 0 {
                break;
            }
            prev = self.run_one_round(task, round, &prev).await?;
        }
        Ok(())
    }

    /// Rounds to run in the parallel path: keep ~the same contributions per
    /// planner as the sequential default (4 turns / 2 planners = 2 each → 2
    /// rounds), but always ≥2 so there is a draft round and a synthesis round.
    fn round_count(&self, max_turns: usize) -> usize {
        let n = self
            .roster
            .iter()
            .filter(|r| r.role.is_planner())
            .count()
            .max(1);
        (max_turns / n).clamp(2, 6)
    }

    /// Planners still in play (not timed out) — when this hits zero the rounds
    /// loop stops.
    fn active_planner_count(&self) -> usize {
        self.roster
            .iter()
            .filter(|r| r.role.is_planner() && !self.terminated.contains(&r.agent))
            .count()
    }

    /// Run ONE parallel planning round: fire every active planner concurrently
    /// (round 0 = independent drafts; later rounds = roster-first SYNTHESIZES a
    /// merged plan while the rest cross-review), collect them off the shared
    /// stream via [`Self::collect_round`], and record in deterministic roster
    /// order. Returns this round's per-planner output to feed the next round's
    /// delta. Shared by the batch (`run_discussion`) and interactive paths.
    async fn run_one_round(
        &mut self,
        task: &str,
        round: usize,
        prev: &HashMap<AgentId, String>,
    ) -> Result<HashMap<AgentId, String>> {
        let active: Vec<RosterEntry> = self
            .roster
            .iter()
            .filter(|r| r.role.is_planner() && !self.terminated.contains(&r.agent))
            .cloned()
            .collect();
        if active.is_empty() {
            return Ok(HashMap::new());
        }
        let synth = active.first().map(|e| e.agent);

        for entry in &active {
            let resumable = self.is_resumable(entry.agent);
            let prompt = if round == 0 {
                compose_round1_prompt(
                    task,
                    self.project_context.as_deref(),
                    self.context_budget_chars,
                )
            } else {
                let others: String = active
                    .iter()
                    .filter(|o| o.agent != entry.agent)
                    .filter_map(|o| prev.get(&o.agent))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");
                let own = prev.get(&entry.agent).cloned().unwrap_or_default();
                compose_round_synth_prompt(
                    task,
                    &others,
                    Some(entry.agent) == synth,
                    resumable,
                    &own,
                    self.context_budget_chars,
                )
            };
            self.bus.emit(OrchestratorEvent::TurnStarted {
                agent: entry.agent,
                role: format!("{:?}", entry.role),
            });
            let attachments = self.media_for(entry.agent);
            let tx = self
                .cmd_txs
                .get(&entry.agent)
                .ok_or_else(|| {
                    TalesError::Other(format!("no command channel for {}", entry.agent))
                })?
                .clone();
            let phase = if round == 0 {
                PromptPhase::Planning
            } else {
                PromptPhase::PlanningSynthesis
            };
            self.record_prompt_sent(entry.agent, phase, &prompt);
            tx.send(AgentCommand::StartTurn {
                prompt,
                attachments,
            })
            .await
            .map_err(|e| TalesError::Other(format!("send to agent failed: {e}")))?;
        }

        let active_ids: Vec<AgentId> = active.iter().map(|e| e.agent).collect();
        let finals = self.collect_round(&active_ids).await;

        // Record in roster order (deterministic) regardless of completion race,
        // and return outputs for the next round's delta.
        let mut out: HashMap<AgentId, String> = HashMap::new();
        for entry in &active {
            let text = no_output_placeholder(finals.get(&entry.agent).cloned().unwrap_or_default());
            out.insert(entry.agent, text.clone());
            self.blackboard
                .record(entry.label.clone(), entry.role, text);
            self.last_seen
                .insert(entry.agent, self.blackboard.transcript.len());
        }
        Ok(out)
    }

    /// Collect a whole PARALLEL round from the single shared event stream by
    /// demultiplexing on each event's agent UUID into per-agent buffers — never
    /// dropping a sibling's output the way `collect_turn` does. One absolute
    /// deadline bounds the round; any planner that hasn't finished by then is
    /// shut down, terminated, and reported, so a stuck agent can't hang the run.
    async fn collect_round(&mut self, members: &[AgentId]) -> HashMap<AgentId, String> {
        use std::collections::HashSet;
        let mut pending: HashSet<AgentId> = members.iter().copied().collect();
        let mut finals: HashMap<AgentId, String> = HashMap::new();
        let deadline = tokio::time::Instant::now() + self.turn_timeout;

        while !pending.is_empty() {
            let event = match tokio::time::timeout_at(deadline, self.events_rx.recv()).await {
                Ok(Some(e)) => e,
                Ok(None) => break, // all senders dropped
                Err(_elapsed) => {
                    // Round budget exhausted — fail exactly the laggards.
                    for a in pending.drain() {
                        self.bus.emit(OrchestratorEvent::Log {
                            level: "error".to_string(),
                            msg: format!(
                                "turn timed out after {:?}; terminating this agent",
                                self.turn_timeout
                            ),
                        });
                        if let Some(tx) = self.cmd_txs.get(&a) {
                            let _ = tx.send(AgentCommand::Shutdown).await;
                        }
                        self.terminated.insert(a);
                        self.emit_turn_complete(a, None, None);
                    }
                    break;
                }
            };

            let aid = event_agent(&event);
            match event {
                // Skill discovery can arrive for any agent — handle it regardless
                // of round membership.
                AgentEvent::Skills { agent, skills } => self.record_skills(agent, skills),
                AgentEvent::TokenDelta { agent, text, .. } if pending.contains(&aid) => {
                    self.bus.emit(OrchestratorEvent::Token { agent, text });
                }
                AgentEvent::MessageFinal { agent, text, .. } if pending.contains(&aid) => {
                    finals.insert(agent, text.clone());
                    self.bus.emit(OrchestratorEvent::Message { agent, text });
                }
                AgentEvent::ToolCall { agent, name, .. } if pending.contains(&aid) => {
                    self.bus.emit(OrchestratorEvent::ToolActivity {
                        agent,
                        summary: name,
                    });
                }
                AgentEvent::TurnComplete {
                    agent,
                    cost_usd,
                    token_usage,
                    ..
                } if pending.contains(&aid) => {
                    self.emit_turn_complete(agent, cost_usd, token_usage);
                    pending.remove(&agent);
                }
                AgentEvent::Error {
                    agent,
                    message,
                    fatal,
                } if pending.contains(&aid) => {
                    self.bus.emit(OrchestratorEvent::Log {
                        level: if fatal { "error" } else { "warn" }.to_string(),
                        msg: message,
                    });
                    if fatal {
                        self.emit_turn_complete(agent, None, None);
                        pending.remove(&agent);
                    }
                }
                AgentEvent::Exited { agent, code } if pending.contains(&aid) => {
                    self.bus
                        .emit(OrchestratorEvent::AgentExited { agent, code });
                    self.emit_turn_complete(agent, None, None);
                    pending.remove(&agent);
                }
                // Stray from a finished/foreign agent, or SessionReady/TurnStarted.
                _ => {}
            }
        }
        finals
    }

    /// Drain the shared event stream until the active agent finishes its turn,
    /// bridging each event onto the bus. Only the active agent emits during its
    /// turn; stray events from idle agents (e.g. an early `SessionReady`) are
    /// ignored.
    async fn collect_turn(&mut self, agent: AgentId) -> Result<String> {
        let mut final_text = String::new();

        loop {
            let event = match tokio::time::timeout(self.turn_timeout, self.events_rx.recv()).await {
                Ok(Some(e)) => e,
                Ok(None) => break, // all senders dropped
                Err(_elapsed) => {
                    self.bus.emit(OrchestratorEvent::Log {
                        level: "error".to_string(),
                        msg: format!(
                            "turn timed out after {:?}; terminating this agent",
                            self.turn_timeout
                        ),
                    });
                    // Shut the stuck agent down so it can't keep running and
                    // emit stray output into a later turn. Mark it terminated so
                    // the turn loops stop scheduling it (its command channel is
                    // now dead; another StartTurn would abort the whole run).
                    if let Some(tx) = self.cmd_txs.get(&agent) {
                        let _ = tx.send(AgentCommand::Shutdown).await;
                    }
                    self.terminated.insert(agent);
                    // Signal the turn's end so any frontend's live partial
                    // finalizes instead of spinning forever on a dead agent.
                    self.emit_turn_complete(agent, None, None);
                    return Ok(final_text);
                }
            };

            match event {
                AgentEvent::TokenDelta { agent: a, text, .. } if a == agent => {
                    self.bus.emit(OrchestratorEvent::Token { agent, text });
                }
                AgentEvent::MessageFinal { agent: a, text, .. } if a == agent => {
                    final_text = text.clone();
                    self.bus.emit(OrchestratorEvent::Message { agent, text });
                }
                AgentEvent::ToolCall { agent: a, name, .. } if a == agent => {
                    self.bus.emit(OrchestratorEvent::ToolActivity {
                        agent,
                        summary: name,
                    });
                }
                AgentEvent::TurnComplete {
                    agent: a,
                    cost_usd,
                    token_usage,
                    ..
                } if a == agent => {
                    self.emit_turn_complete(agent, cost_usd, token_usage);
                    return Ok(final_text);
                }
                AgentEvent::Error {
                    agent: a,
                    message,
                    fatal,
                } if a == agent => {
                    self.bus.emit(OrchestratorEvent::Log {
                        level: if fatal { "error" } else { "warn" }.to_string(),
                        msg: message,
                    });
                    if fatal {
                        self.emit_turn_complete(agent, None, None);
                        return Ok(final_text);
                    }
                }
                AgentEvent::Exited { agent: a, code } if a == agent => {
                    self.bus
                        .emit(OrchestratorEvent::AgentExited { agent, code });
                    self.emit_turn_complete(agent, None, None);
                    return Ok(final_text);
                }
                // Skill discovery can arrive for any agent (at startup).
                AgentEvent::Skills { agent: a, skills } => self.record_skills(a, skills),
                // SessionReady, TurnStarted, and any event from another agent.
                _ => {}
            }
        }

        // The event channel closed before a terminal event — still signal the
        // turn's end so a frontend's live partial finalizes.
        self.emit_turn_complete(agent, None, None);
        Ok(final_text)
    }

    /// Ask each agent (with the discussion in its context) to nominate an
    /// executor, tally the confidence-weighted votes, and surface the
    /// recommendation. Transitions to [`Phase::AwaitingConfirmation`]; it does
    /// **not** start execution.
    pub async fn run_recommendation(&mut self) -> Result<Recommendation> {
        self.set_phase(Phase::Recommending);
        let candidates: Vec<String> = self.roster.iter().map(|r| r.label.clone()).collect();
        let roster = self.roster.clone();
        let mut votes: Vec<ExecutionVote> = Vec::new();

        for entry in &roster {
            // Only planners vote (a separate executor isn't part of the debate);
            // but it stays in `candidates` above so it can be recommended.
            if !entry.role.is_planner() {
                continue;
            }
            // A timed-out agent can't vote — skip it rather than send to a dead channel.
            if self.terminated.contains(&entry.agent) {
                continue;
            }
            let vote_context = compose_vote_context(&self.blackboard, self.context_budget_chars);
            let prompt = compose_vote_prompt(
                &self.blackboard.task,
                &candidates,
                self.is_resumable(entry.agent),
                &vote_context,
            );
            let tx = self
                .cmd_txs
                .get(&entry.agent)
                .ok_or_else(|| {
                    TalesError::Other(format!("no command channel for {}", entry.agent))
                })?
                .clone();
            self.record_prompt_sent(entry.agent, PromptPhase::Recommendation, &prompt);
            tx.send(AgentCommand::StartTurn {
                prompt,
                attachments: Vec::new(),
            })
            .await
            .map_err(|e| TalesError::Other(format!("send to agent failed: {e}")))?;
            let mut text = self.collect_turn(entry.agent).await?;
            let mut parsed = parse_vote_lenient(&text, &candidates);
            if matches!(
                parsed.as_ref().map(|vote| vote.source),
                Some(VoteParseSource::CandidateMention)
            ) {
                self.salvaged_votes += 1;
                self.bus.emit(OrchestratorEvent::Log {
                    level: "info".to_string(),
                    msg: format!(
                        "{} returned prose vote; salvaged unambiguous candidate without repair",
                        entry.label
                    ),
                });
            }

            if parsed.is_none() {
                self.bus.emit(OrchestratorEvent::Log {
                    level: "warn".to_string(),
                    msg: format!(
                        "{} returned no parseable vote; asking once for JSON",
                        entry.label
                    ),
                });
                let repair = compose_vote_repair_prompt(
                    &self.blackboard.task,
                    &candidates,
                    self.is_resumable(entry.agent),
                    &vote_context,
                    &text,
                );
                self.record_prompt_sent(entry.agent, PromptPhase::VoteRepair, &repair);
                tx.send(AgentCommand::StartTurn {
                    prompt: repair,
                    attachments: Vec::new(),
                })
                .await
                .map_err(|e| TalesError::Other(format!("send vote repair failed: {e}")))?;
                text = self.collect_turn(entry.agent).await?;
                parsed = parse_vote_lenient(&text, &candidates);
                if matches!(
                    parsed.as_ref().map(|vote| vote.source),
                    Some(VoteParseSource::CandidateMention)
                ) {
                    self.salvaged_votes += 1;
                    self.bus.emit(OrchestratorEvent::Log {
                        level: "info".to_string(),
                        msg: format!(
                            "{} repair returned prose vote; salvaged unambiguous candidate",
                            entry.label
                        ),
                    });
                }
            }

            match parsed {
                Some(vote) => votes.push(ExecutionVote {
                    voter: entry.agent,
                    voter_label: entry.label.clone(),
                    recommended_executor: vote.recommended_executor,
                    confidence: vote.confidence,
                    needs_plan: vote.needs_plan,
                    rationale: vote.rationale,
                    parse_source: vote.source,
                }),
                None => self.bus.emit(OrchestratorEvent::Log {
                    level: "warn".to_string(),
                    msg: format!("{} returned no parseable vote", entry.label),
                }),
            }
        }

        let recommendation = aggregate(votes, &candidates)
            .ok_or_else(|| TalesError::Other("no candidates to recommend".to_string()))?;
        self.last_recommendation = Some(recommendation.clone());

        self.bus.emit(OrchestratorEvent::RecommendationReady {
            executor: recommendation.executor.clone(),
            rationale: recommendation.rationale.clone(),
            confident: recommendation.confident,
            scores: recommendation.scores.clone(),
        });
        self.set_phase(Phase::AwaitingConfirmation);
        let gate_prompt = if recommendation.confident {
            if recommendation.needs_plan {
                format!(
                    "Both agents recommend a formal plan first. Preselected executor: {}. Confirm, override, or reject.",
                    recommendation.executor
                )
            } else {
                format!(
                    "Recommended executor: {}. Confirm, override, or reject.",
                    recommendation.executor
                )
            }
        } else {
            format!(
                "No clear executor consensus. Preselected executor: {}. Confirm, override, or reject.",
                recommendation.executor
            )
        };
        self.bus.emit(OrchestratorEvent::AwaitingConfirmation {
            prompt: gate_prompt,
        });

        Ok(recommendation)
    }

    /// The execution gate. Releases the engine into [`Phase::Executing`] with
    /// the user-chosen executor. This is the ONLY transition into execution —
    /// callable only from [`Phase::AwaitingConfirmation`], and only with a
    /// label that exists in the roster. Returns the canonical executor label.
    pub fn confirm_execution(&mut self, executor: &str) -> Result<String> {
        if self.phase != Phase::AwaitingConfirmation {
            return Err(TalesError::Other(format!(
                "cannot confirm execution from phase {:?}",
                self.phase
            )));
        }
        let needle = executor.trim();
        if needle.is_empty() {
            return Err(TalesError::Other("executor must be non-empty".to_string()));
        }
        let chosen = self
            .roster
            .iter()
            .find(|r| r.label.eq_ignore_ascii_case(needle))
            .map(|r| r.label.clone())
            .ok_or_else(|| TalesError::Other(format!("unknown executor '{executor}'")))?;
        self.set_phase(Phase::Executing);
        Ok(chosen)
    }

    /// The user declined to execute the plan.
    pub fn reject(&mut self) -> Result<()> {
        if self.phase != Phase::AwaitingConfirmation {
            return Err(TalesError::Other(format!(
                "cannot reject from phase {:?}",
                self.phase
            )));
        }
        self.set_phase(Phase::Done);
        Ok(())
    }

    /// Run the whole pipeline interactively, driven by [`UserCommand`]s from
    /// the bus — this is the human-in-the-loop entry point the live chat uses.
    ///
    /// Flow: discussion (human notes folded in between turns) → recommendation
    /// → **block at the gate** for the user's decision → on confirm, an
    /// execution turn by the chosen executor. Returns the [`RunOutcome`].
    pub async fn run_interactive(
        &mut self,
        task: &str,
        max_turns: usize,
        commands: &mut mpsc::Receiver<UserCommand>,
    ) -> Result<RunOutcome> {
        self.blackboard.task = task.to_string();
        self.set_phase(Phase::Planning);
        // Only planners take discussion turns; a separate executor sits out the
        // debate and runs the agreed plan at the gate.
        let planners: Vec<RosterEntry> = self
            .roster
            .iter()
            .filter(|r| r.role.is_planner())
            .cloned()
            .collect();

        if self.parallel_rounds {
            // Parallel rounds: planners speak concurrently each round, with human
            // notes folded BETWEEN rounds (next-round, not next-speaker, semantics).
            let rounds = self.round_count(max_turns);
            let mut prev: HashMap<AgentId, String> = HashMap::new();
            for round in 0..rounds {
                if self.drain_user_notes(commands) {
                    let outcome = RunOutcome::Aborted;
                    self.emit_session_report(&outcome);
                    return Ok(outcome);
                }
                if self.active_planner_count() == 0 {
                    break;
                }
                prev = self.run_one_round(task, round, &prev).await?;
            }
        } else {
            let mut turn_idx = 0;
            while turn_idx < max_turns && !planners.is_empty() {
                // Fold any pending human interjections into the conversation first.
                if self.drain_user_notes(commands) {
                    let outcome = RunOutcome::Aborted;
                    self.emit_session_report(&outcome);
                    return Ok(outcome);
                }
                let entry = &planners[turn_idx % planners.len()];
                // Skip a timed-out agent (dead channel) rather than abort the run.
                if self.terminated.contains(&entry.agent) {
                    turn_idx += 1;
                    continue;
                }
                let resumable = self.is_resumable(entry.agent);
                let seen = *self.last_seen.get(&entry.agent).unwrap_or(&0);
                let first_time = !self.last_seen.contains_key(&entry.agent);
                let delta = self.blackboard.transcript_text_from(seen);
                let prompt = compose_prompt(
                    entry.role,
                    task,
                    &self.blackboard,
                    resumable,
                    &delta,
                    first_time,
                    self.context_budget_chars,
                    self.project_context.as_deref(),
                );
                self.bus.emit(OrchestratorEvent::TurnStarted {
                    agent: entry.agent,
                    role: format!("{:?}", entry.role),
                });
                self.bus.emit(OrchestratorEvent::Log {
                    level: "info".to_string(),
                    msg: format!("{} speaking as {:?}", entry.label, entry.role),
                });
                let attachments = self.media_for(entry.agent);
                let agent_id = entry.agent;
                let label = entry.label.clone();
                let role = entry.role;
                let tx = self
                    .cmd_txs
                    .get(&agent_id)
                    .ok_or_else(|| TalesError::Other(format!("no channel for {agent_id}")))?
                    .clone();
                self.record_prompt_sent(agent_id, PromptPhase::Planning, &prompt);
                tx.send(AgentCommand::StartTurn {
                    prompt,
                    attachments,
                })
                .await
                .map_err(|e| TalesError::Other(format!("send failed: {e}")))?;
                let text = self.collect_turn(agent_id).await?;
                self.blackboard
                    .record(label, role, no_output_placeholder(text));
                self.last_seen
                    .insert(agent_id, self.blackboard.transcript.len());
                turn_idx += 1;
            }
        }

        let _recommendation = self.run_recommendation().await?;

        // The gate: honor an early decision made during planning if there was
        // one, otherwise block until the human decides. Interjections are still
        // accepted (recorded); only Confirm/Reject move the run forward.
        loop {
            let cmd = match self.pending_decision.take() {
                Some(c) => c,
                None => match commands.recv().await {
                    Some(c) => c,
                    None => {
                        let outcome = RunOutcome::Aborted;
                        self.emit_session_report(&outcome);
                        return Ok(outcome);
                    }
                },
            };
            match cmd {
                UserCommand::ConfirmExecution { executor } => {
                    let chosen = match self.confirm_execution(&executor) {
                        Ok(c) => c,
                        Err(e) => {
                            self.bus.emit(OrchestratorEvent::Log {
                                level: "warn".to_string(),
                                msg: e.to_string(),
                            });
                            continue; // invalid choice — stay at the gate
                        }
                    };
                    let output = self.run_execution(&chosen, task).await?;
                    let outcome = RunOutcome::Executed {
                        executor: chosen,
                        output,
                    };
                    self.emit_session_report(&outcome);
                    return Ok(outcome);
                }
                UserCommand::Reject => {
                    self.reject()?;
                    let outcome = RunOutcome::Rejected;
                    self.emit_session_report(&outcome);
                    return Ok(outcome);
                }
                UserCommand::InjectNote {
                    text, attachments, ..
                } => self.record_human(text, attachments),
                UserCommand::StartTurn { .. } => {}
                UserCommand::Shutdown => {
                    let outcome = RunOutcome::Aborted;
                    self.emit_session_report(&outcome);
                    return Ok(outcome);
                }
            }
        }
    }

    fn emit_session_report(&self, outcome: &RunOutcome) {
        let reported_costs = self.reported_costs();
        let reported_token_usage = self.reported_token_usage();
        let prompt_stats = self.reported_prompt_stats();
        let prompt_phase_stats = self.reported_prompt_phase_stats();
        let markdown = compose_session_report(
            &self.blackboard,
            self.last_recommendation.as_ref(),
            outcome,
            self.context_budget_chars,
            self.project_context_report.as_ref(),
            self.local_changes.as_ref(),
            self.salvaged_votes,
            &reported_costs,
            &reported_token_usage,
            &prompt_stats,
            &prompt_phase_stats,
        );
        let summary = compose_session_summary(
            &self.blackboard,
            self.last_recommendation.as_ref(),
            outcome,
            self.context_budget_chars,
            self.project_context_report.as_ref(),
            self.local_changes.as_ref(),
            self.salvaged_votes,
            &reported_costs,
            &reported_token_usage,
            &prompt_stats,
            &prompt_phase_stats,
        );
        self.bus
            .emit(OrchestratorEvent::SessionReport { markdown, summary });
    }

    fn reported_costs(&self) -> Vec<(String, f64)> {
        self.roster
            .iter()
            .filter_map(|entry| {
                self.latest_cost_usd
                    .get(&entry.agent)
                    .map(|cost| (format!("{} ({:?})", entry.label, entry.role), *cost))
            })
            .collect()
    }

    fn reported_token_usage(&self) -> Vec<(String, TokenUsage)> {
        self.roster
            .iter()
            .filter_map(|entry| {
                self.reported_token_usage
                    .get(&entry.agent)
                    .copied()
                    .map(|usage| (format!("{} ({:?})", entry.label, entry.role), usage))
            })
            .collect()
    }

    fn reported_prompt_stats(&self) -> Vec<(String, PromptStats)> {
        self.roster
            .iter()
            .filter_map(|entry| {
                self.prompt_stats
                    .get(&entry.agent)
                    .cloned()
                    .map(|stats| (format!("{} ({:?})", entry.label, entry.role), stats))
            })
            .collect()
    }

    fn reported_prompt_phase_stats(&self) -> Vec<(String, PromptStats)> {
        const ORDER: [PromptPhase; 5] = [
            PromptPhase::Planning,
            PromptPhase::PlanningSynthesis,
            PromptPhase::Recommendation,
            PromptPhase::VoteRepair,
            PromptPhase::Execution,
        ];
        ORDER
            .iter()
            .filter_map(|phase| {
                self.prompt_phase_stats
                    .get(phase)
                    .cloned()
                    .map(|stats| (phase.label().to_string(), stats))
            })
            .collect()
    }

    /// Non-blocking drain of pending user commands during the discussion.
    /// Returns `true` if a shutdown was requested.
    fn drain_user_notes(&mut self, commands: &mut mpsc::Receiver<UserCommand>) -> bool {
        loop {
            match commands.try_recv() {
                Ok(UserCommand::InjectNote {
                    text, attachments, ..
                }) => self.record_human(text, attachments),
                Ok(UserCommand::Shutdown) => return true,
                // Remember an early decision for the gate instead of dropping it.
                Ok(cmd @ (UserCommand::ConfirmExecution { .. } | UserCommand::Reject)) => {
                    self.pending_decision = Some(cmd);
                }
                Ok(UserCommand::StartTurn { .. }) => {}
                Err(mpsc::error::TryRecvError::Empty)
                | Err(mpsc::error::TryRecvError::Disconnected) => return false,
            }
        }
    }

    /// Record a human interjection so agents see it in the transcript, echo it
    /// onto the bus, and queue any attached media to share with the agents.
    fn record_human(&mut self, mut text: String, attachments: Vec<PathBuf>) {
        if !attachments.is_empty() {
            let names: Vec<String> = attachments
                .iter()
                .map(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string()
                })
                .collect();
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[attached: {}]", names.join(", ")));
            for p in attachments {
                self.pending_media.push(Attachment::new(p));
            }
        }
        self.bus
            .emit(OrchestratorEvent::UserMessage { text: text.clone() });
        self.blackboard.record("you".to_string(), Role::Human, text);
    }

    /// Have the chosen executor implement the agreed discussion, streamed like any
    /// other turn.
    async fn run_execution(&mut self, executor_label: &str, task: &str) -> Result<String> {
        let entry = self
            .roster
            .iter()
            .find(|r| r.label == executor_label)
            .cloned()
            .ok_or_else(|| {
                TalesError::Other(format!("executor '{executor_label}' not in roster"))
            })?;
        let execution_packet = compose_execution_packet(
            task,
            executor_label,
            &self.blackboard,
            self.last_recommendation.as_ref(),
            self.context_budget_chars,
            self.local_changes.as_ref(),
        );
        let has_session_plan = entry.role.is_planner() && self.is_resumable(entry.agent);
        self.bus.emit(OrchestratorEvent::ExecutionPacket {
            executor: executor_label.to_string(),
            text: execution_packet.clone(),
            included_in_prompt: !has_session_plan,
        });
        let prompt = compose_execution_prompt(
            task,
            &execution_packet,
            has_session_plan,
            self.context_budget_chars,
            self.local_changes.as_ref(),
        );
        self.bus.emit(OrchestratorEvent::TurnStarted {
            agent: entry.agent,
            role: "Executor".to_string(),
        });
        self.bus.emit(OrchestratorEvent::Log {
            level: "info".to_string(),
            msg: format!("executing with {executor_label}"),
        });
        let attachments = self.media_for(entry.agent);
        let tx = self
            .cmd_txs
            .get(&entry.agent)
            .ok_or_else(|| TalesError::Other(format!("no channel for {}", entry.agent)))?
            .clone();
        self.record_prompt_sent(entry.agent, PromptPhase::Execution, &prompt);
        tx.send(AgentCommand::StartTurn {
            prompt,
            attachments,
        })
        .await
        .map_err(|e| TalesError::Other(format!("send failed: {e}")))?;
        let output = self.collect_turn(entry.agent).await?;
        self.set_phase(Phase::Done);
        Ok(output)
    }

    /// Gracefully stop every agent.
    pub async fn shutdown(&mut self) {
        for tx in self.cmd_txs.values() {
            let _ = tx.send(AgentCommand::Shutdown).await;
        }
    }
}

/// A turn that produced no text (a crash, a kill, or a tool-only turn with no
/// final message) shouldn't be recorded as the agent saying nothing — that
/// silently misleads the next speaker. Mark it explicitly instead.
fn no_output_placeholder(text: String) -> String {
    if text.trim().is_empty() {
        "(no output this turn)".to_string()
    } else {
        text
    }
}

/// Keep the most recent prompt context and mark that older material was omitted.
/// Recent discussion usually carries the actionable plan, while this prevents
/// stateless adapters from paying to reread an unbounded transcript.
fn bounded_context(text: &str, max_chars: Option<usize>) -> String {
    let trimmed = text.trim();
    let Some(max_chars) = max_chars else {
        return trimmed.to_string();
    };
    if max_chars == 0 || trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let tail: String = trimmed
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("(earlier context omitted; showing last {max_chars} chars)\n{tail}")
}

fn split_context_budget(max_chars: Option<usize>, sections: usize) -> Option<usize> {
    match (max_chars, sections) {
        (Some(chars), sections) if chars > 0 && sections > 1 => Some((chars / sections).max(1)),
        _ => max_chars,
    }
}

fn estimated_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

fn format_token_usage_line(label: &str, usage: TokenUsage) -> String {
    let mut parts = Vec::new();
    if let Some(total) = usage.total_or_sum() {
        parts.push(format!("total={total}"));
    }
    if let Some(input) = usage.input_tokens {
        parts.push(format!("input={input}"));
    }
    if let Some(cached) = usage.cached_input_tokens {
        parts.push(format!("cached_input={cached}"));
    }
    if let Some(output) = usage.output_tokens {
        parts.push(format!("output={output}"));
    }
    if let Some(reasoning) = usage.reasoning_output_tokens {
        parts.push(format!("reasoning_output={reasoning}"));
    }
    if parts.is_empty() {
        format!("- {label}: unavailable\n")
    } else {
        format!("- {label}: {}\n", parts.join(", "))
    }
}

fn reported_input_tokens(reported_token_usage: &[(String, TokenUsage)]) -> Option<u64> {
    let mut total = 0u64;
    let mut seen = false;
    for (_, usage) in reported_token_usage {
        if let Some(input) = usage.input_tokens {
            total = total.saturating_add(input);
            seen = true;
        }
    }
    seen.then_some(total)
}

fn ratio_percent(numerator: u64, denominator: u64) -> u64 {
    numerator
        .saturating_mul(100)
        .checked_div(denominator)
        .unwrap_or(0)
}

/// Prompt asking an agent to nominate an executor as a JSON object.
///
/// Resumable agents already have the discussion in their native session, so keep
/// this tiny. Stateless adapters get a compact discussion context instead of the
/// raw full transcript, so they can vote without rereading old back-and-forth.
fn compose_vote_prompt(
    task: &str,
    candidates: &[String],
    resumable: bool,
    planning_context: &str,
) -> String {
    let context = if resumable {
        "Use the discussion already in this session.".to_string()
    } else if planning_context.trim().is_empty() {
        "No discussion context is available yet.".to_string()
    } else {
        format!("Discussion context:\n{}", planning_context.trim())
    };
    format!(
        "Based on our discussion of the task:\n{task}\n\n\
         {context}\n\n\
         Which agent should execute this? Candidates: {}.\n\
         Set needs_plan=true only if this should become a formal plan before execution; \
         if either planner says false, Tales will proceed to executor choice without making a plan.\n\
         Reply with ONLY a JSON object, no prose and no code fence:\n\
         {{\"recommended_executor\": \"<one of: {}>\", \
         \"confidence\": <number 0.0-1.0>, \"needs_plan\": <true|false>, \
         \"rationale\": \"<one short sentence>\"}}",
        candidates.join(", "),
        candidates.join(" | ")
    )
}

/// One-shot repair prompt for a malformed vote. This is intentionally tiny for
/// resumable agents and compact-contextual for stateless ones.
fn compose_vote_repair_prompt(
    task: &str,
    candidates: &[String],
    resumable: bool,
    planning_context: &str,
    previous_response: &str,
) -> String {
    let context = if resumable {
        "Use the discussion already in this session.".to_string()
    } else if planning_context.trim().is_empty() {
        "No discussion context is available yet.".to_string()
    } else {
        format!("Discussion context:\n{}", planning_context.trim())
    };
    format!(
        "Your previous executor vote was not valid JSON.\n\
         Task: {task}\n\n\
         {context}\n\n\
         Previous response, for reference:\n{}\n\n\
         Reply with ONLY this JSON object, no prose and no code fence:\n\
         {{\"recommended_executor\": \"<one of: {}>\", \
         \"confidence\": <number 0.0-1.0>, \"needs_plan\": <true|false>, \
         \"rationale\": \"<one short sentence>\"}}",
        bounded_context(previous_response, Some(VOTE_REPAIR_PREVIEW_CHARS)),
        candidates.join(" | ")
    )
}

/// Compact discussion context for stateless recommendation votes. It preserves
/// recent human direction and the latest planner outputs, while dropping older
/// debate mechanics that rarely help choose an executor.
fn compose_vote_context(bb: &Blackboard, context_budget_chars: Option<usize>) -> String {
    let mut body = String::new();

    let mut human_notes: Vec<_> = bb
        .transcript
        .iter()
        .filter(|u| u.role == Role::Human)
        .rev()
        .take(3)
        .collect();
    human_notes.reverse();
    if !human_notes.is_empty() {
        body.push_str("Latest human direction:\n");
        for note in human_notes {
            body.push_str(&format!("[{}]\n{}\n", note.label, note.text.trim()));
        }
        body.push('\n');
    }

    let mut planner_outputs: Vec<_> = bb
        .transcript
        .iter()
        .filter(|u| u.role.is_planner())
        .rev()
        .take(4)
        .collect();
    planner_outputs.reverse();
    body.push_str("Latest planner outputs:\n");
    if planner_outputs.is_empty() {
        body.push_str("(no planner output recorded)\n");
    } else {
        for u in planner_outputs {
            body.push_str(&format!(
                "[{} — {:?}]\n{}\n\n",
                u.label,
                u.role,
                u.text.trim()
            ));
        }
    }

    bounded_context(&body, context_budget_chars)
}

/// Build a compact deterministic handoff for executors that do not already have
/// the planning session in memory. It keeps the chosen executor, recommendation
/// rationale, latest human direction, and most recent planner outputs while
/// dropping old back-and-forth.
fn compose_execution_packet(
    task: &str,
    executor_label: &str,
    bb: &Blackboard,
    recommendation: Option<&Recommendation>,
    context_budget_chars: Option<usize>,
    local_changes: Option<&LocalChangeSummary>,
) -> String {
    let mut header = format!(
        "Execution packet\n\
         Task: {task}\n\
         Chosen executor: {executor_label}\n\
         Context policy: compact packet; older debate may be omitted.\n"
    );

    let mut sections = Vec::new();
    if let Some(rec) = recommendation {
        let mut section = String::new();
        section.push_str("Recommendation:\n");
        section.push_str(&format!(
            "- recommended: {} (confident: {})\n",
            rec.executor, rec.confident
        ));
        section.push_str(&format!(
            "- formal_plan_requested_by_both: {}\n",
            rec.needs_plan
        ));
        if !rec.rationale.trim().is_empty() {
            section.push_str("- rationale:\n");
            section.push_str(rec.rationale.trim());
            section.push('\n');
        }
        if !rec.scores.is_empty() {
            let scores = rec
                .scores
                .iter()
                .map(|(label, score)| format!("{label}={score:.2}"))
                .collect::<Vec<_>>()
                .join(", ");
            section.push_str(&format!("- scores: {scores}\n"));
        }
        sections.push(section);
    }

    let mut human_notes: Vec<_> = bb
        .transcript
        .iter()
        .filter(|u| u.role == Role::Human)
        .rev()
        .take(3)
        .collect();
    human_notes.reverse();
    if !human_notes.is_empty() {
        let mut section = String::new();
        section.push_str("Latest human direction:\n");
        for note in human_notes {
            section.push_str(&format!("[{}]\n{}\n", note.label, note.text.trim()));
        }
        sections.push(section);
    }

    let mut planner_outputs: Vec<_> = bb
        .transcript
        .iter()
        .filter(|u| u.role.is_planner())
        .rev()
        .take(4)
        .collect();
    planner_outputs.reverse();
    let mut planner_section = String::new();
    planner_section.push_str("Latest planner outputs:\n");
    if planner_outputs.is_empty() {
        planner_section.push_str("(no planner output recorded)\n");
    } else {
        for u in planner_outputs {
            planner_section.push_str(&format!(
                "[{} — {:?}]\n{}\n\n",
                u.label,
                u.role,
                u.text.trim()
            ));
        }
    }
    sections.push(planner_section);

    if let Some(changes) = local_changes {
        sections.push(changes.to_handoff_text());
    }

    header.push_str(&budgeted_execution_sections(
        &sections,
        context_budget_chars,
    ));
    header
}

fn budgeted_execution_sections(sections: &[String], context_budget_chars: Option<usize>) -> String {
    let sections: Vec<&str> = sections
        .iter()
        .map(|section| section.trim())
        .filter(|section| !section.is_empty())
        .collect();
    if sections.is_empty() {
        return String::new();
    }

    let mut body = sections
        .iter()
        .map(|section| format!("\n{section}\n"))
        .collect::<String>();
    let Some(max_chars) = context_budget_chars else {
        return body;
    };
    if body.trim().chars().count() <= max_chars {
        return body;
    }

    body.clear();
    let section_budget = split_context_budget(Some(max_chars), sections.len());
    for section in sections {
        body.push('\n');
        body.push_str(&bounded_execution_section(section, section_budget));
        body.push('\n');
    }
    body
}

fn bounded_execution_section(section: &str, max_chars: Option<usize>) -> String {
    let mut lines = section.lines();
    let heading = lines.next().unwrap_or_default();
    let rest = lines.collect::<Vec<_>>().join("\n");
    if rest.trim().is_empty() {
        return heading.to_string();
    }
    format!("{heading}\n{}", bounded_context(&rest, max_chars))
}

#[allow(clippy::too_many_arguments)]
fn compose_session_report(
    bb: &Blackboard,
    recommendation: Option<&Recommendation>,
    outcome: &RunOutcome,
    context_budget_chars: Option<usize>,
    project_context_report: Option<&ProjectContextReport>,
    local_changes: Option<&LocalChangeSummary>,
    salvaged_votes: usize,
    reported_costs: &[(String, f64)],
    reported_token_usage: &[(String, TokenUsage)],
    prompt_stats: &[(String, PromptStats)],
    prompt_phase_stats: &[(String, PromptStats)],
) -> String {
    let mut report = String::new();
    report.push_str("# Tales session report\n\n");
    report.push_str("## Task\n");
    report.push_str(bb.task.trim());
    report.push_str("\n\n## Outcome\n");
    match outcome {
        RunOutcome::Discussed => report.push_str("- status: discussed\n"),
        RunOutcome::Executed { executor, output } => {
            report.push_str(&format!("- status: executed\n- executor: {executor}\n"));
            if !output.trim().is_empty() {
                report.push_str("- executor output:\n");
                report.push_str(&indent_block(&bounded_context(
                    output,
                    context_budget_chars.or(Some(4_000)),
                )));
                report.push('\n');
            }
        }
        RunOutcome::Rejected => report.push_str("- status: rejected by user\n"),
        RunOutcome::Aborted => report.push_str("- status: aborted\n"),
    }

    report.push_str("\n## Resume packet\n");
    report.push_str(&compose_resume_packet(
        bb,
        recommendation,
        outcome,
        project_context_report,
        local_changes,
    ));

    if let Some(rec) = recommendation {
        report.push_str("\n## Recommendation\n");
        report.push_str(&format!(
            "- recommended: {} (confident: {})\n",
            rec.executor, rec.confident
        ));
        report.push_str(&format!(
            "- formal_plan_requested_by_both: {}\n",
            rec.needs_plan
        ));
        if !rec.scores.is_empty() {
            let scores = rec
                .scores
                .iter()
                .map(|(label, score)| format!("{label}={score:.2}"))
                .collect::<Vec<_>>()
                .join(", ");
            report.push_str(&format!("- scores: {scores}\n"));
        }
        if !rec.rationale.trim().is_empty() {
            report.push_str("- rationale:\n");
            report.push_str(&indent_block(&bounded_context(
                rec.rationale.trim(),
                Some(2_000),
            )));
            report.push('\n');
        }
    }

    report.push_str("\n## Project context\n");
    match project_context_report {
        Some(ctx) => {
            report.push_str(&format!(
                "- status: injected ({})\n- cache_path: {}\n- chars: {}\n- budgets: max_files={}, max_manifest_chars={}\n",
                if ctx.cache_hit { "cache hit" } else { "rebuilt" },
                ctx.cache_path,
                ctx.chars,
                ctx.max_files,
                ctx.max_manifest_chars
            ));
        }
        None => report.push_str("- status: not injected\n"),
    }

    report.push_str("\n## Local changes\n");
    match local_changes {
        Some(changes) => report.push_str(&changes.to_handoff_text()),
        None => {
            report.push_str("- status: unavailable (not a git repository or git unavailable)\n")
        }
    }

    report.push_str("\n## Reported cost\n");
    if reported_costs.is_empty() {
        report.push_str(
            "- usd: unavailable (no adapter reported machine-readable USD for this run)\n",
        );
    } else {
        let total: f64 = reported_costs.iter().map(|(_, cost)| *cost).sum();
        report.push_str(&format!("- reported_total_usd: ${total:.4}\n"));
        report.push_str(
            "- note: values are latest session-cumulative adapter totals, summed per agent\n",
        );
        for (label, cost) in reported_costs {
            report.push_str(&format!("- {label}: ${cost:.4}\n"));
        }
    }

    report.push_str("\n## Reported token usage\n");
    if reported_token_usage.is_empty() {
        report
            .push_str("- tokens: unavailable (no adapter reported machine-readable token usage)\n");
    } else {
        let mut total = TokenUsage::default();
        for (_, usage) in reported_token_usage {
            total.add(*usage);
        }
        report.push_str(&format_token_usage_line("reported_total", total));
        for (label, usage) in reported_token_usage {
            report.push_str(&format_token_usage_line(label, *usage));
        }
    }

    report.push_str("\n## Prompt telemetry\n");
    if prompt_stats.is_empty() {
        report.push_str("- prompts_sent: 0\n");
    } else {
        let total_prompts: usize = prompt_stats.iter().map(|(_, s)| s.prompts).sum();
        let total_chars: usize = prompt_stats.iter().map(|(_, s)| s.chars_sent).sum();
        let max_chars: usize = prompt_stats
            .iter()
            .map(|(_, s)| s.max_chars)
            .max()
            .unwrap_or(0);
        report.push_str(&format!(
            "- prompts_sent: {total_prompts}\n- prompt_chars_sent: {total_chars}\n- prompt_tokens_estimate: ~{}\n- max_prompt_chars: {max_chars}\n- max_prompt_tokens_estimate: ~{}\n",
            estimated_tokens(total_chars),
            estimated_tokens(max_chars)
        ));
        match context_budget_chars {
            Some(chars) => report.push_str(&format!(
                "- context_budget_chars: {chars} (~{} tokens)\n",
                estimated_tokens(chars)
            )),
            None => report.push_str("- context_budget_chars: unlimited\n"),
        }
        report.push_str(
            "- note: token estimates use a local chars/4 heuristic before model calls; excludes provider-side resumed session memory and media tokenization\n",
        );
        if let Some(input_tokens) = reported_input_tokens(reported_token_usage) {
            let estimate = estimated_tokens(total_chars);
            report.push_str(&format!(
                "- provider_input_tokens: {input_tokens} (local_prompt_estimate: ~{estimate}, ratio: {}%)\n",
                ratio_percent(input_tokens, estimate as u64)
            ));
        }
        for (label, stats) in prompt_stats {
            report.push_str(&format!(
                "- {label}: prompts={} chars={} tokens~{} max={} max_tokens~{}\n",
                stats.prompts,
                stats.chars_sent,
                estimated_tokens(stats.chars_sent),
                stats.max_chars,
                estimated_tokens(stats.max_chars)
            ));
        }
        if !prompt_phase_stats.is_empty() {
            report.push_str("\n## Prompt phase breakdown\n");
            for (phase, stats) in prompt_phase_stats {
                report.push_str(&format!(
                    "- {phase}: prompts={} chars={} tokens~{} max={} max_tokens~{}\n",
                    stats.prompts,
                    stats.chars_sent,
                    estimated_tokens(stats.chars_sent),
                    stats.max_chars,
                    estimated_tokens(stats.max_chars)
                ));
            }
        }
    }

    report.push_str("\n## Optimization hints\n");
    report.push_str(&compose_optimization_hints(
        context_budget_chars,
        project_context_report,
        local_changes,
        salvaged_votes,
        reported_token_usage,
        prompt_stats,
        prompt_phase_stats,
    ));

    let human_notes: Vec<_> = bb
        .transcript
        .iter()
        .filter(|u| u.role == Role::Human)
        .rev()
        .take(3)
        .collect();
    if !human_notes.is_empty() {
        report.push_str("\n## Latest human direction\n");
        for note in human_notes.into_iter().rev() {
            report.push_str(&format!(
                "- {}: {}\n",
                note.label,
                one_line(&note.text, 240)
            ));
        }
    }

    let planners: Vec<_> = bb
        .transcript
        .iter()
        .filter(|u| u.role.is_planner())
        .rev()
        .take(4)
        .collect();
    report.push_str("\n## Recent planner outputs\n");
    if planners.is_empty() {
        report.push_str("(none)\n");
    } else {
        for u in planners.into_iter().rev() {
            report.push_str(&format!("\n### {} ({:?})\n", u.label, u.role));
            report.push_str(&bounded_context(&u.text, Some(2_000)));
            report.push('\n');
        }
    }

    report.push_str("\n## Transcript stats\n");
    report.push_str(&format!(
        "- utterances: {}\n- characters: {}\n",
        bb.transcript.len(),
        bb.transcript
            .iter()
            .map(|u| u.text.chars().count())
            .sum::<usize>()
    ));
    report
}

#[allow(clippy::too_many_arguments)]
fn compose_session_summary(
    bb: &Blackboard,
    recommendation: Option<&Recommendation>,
    outcome: &RunOutcome,
    context_budget_chars: Option<usize>,
    project_context_report: Option<&ProjectContextReport>,
    local_changes: Option<&LocalChangeSummary>,
    salvaged_votes: usize,
    reported_costs: &[(String, f64)],
    reported_token_usage: &[(String, TokenUsage)],
    prompt_stats: &[(String, PromptStats)],
    prompt_phase_stats: &[(String, PromptStats)],
) -> Value {
    let total_prompt_chars: usize = prompt_stats.iter().map(|(_, stats)| stats.chars_sent).sum();
    let total_prompt_count: usize = prompt_stats.iter().map(|(_, stats)| stats.prompts).sum();
    let max_prompt_chars: usize = prompt_stats
        .iter()
        .map(|(_, stats)| stats.max_chars)
        .max()
        .unwrap_or(0);
    let provider_input_tokens = reported_input_tokens(reported_token_usage);
    let local_prompt_tokens_estimate = estimated_tokens(total_prompt_chars);
    let latest_human_direction = bb
        .transcript
        .iter()
        .rev()
        .find(|u| u.role == Role::Human)
        .map(|u| {
            json!({
                "label": u.label,
                "text": one_line(&u.text, 240),
            })
        });
    let latest_planner_output = bb
        .transcript
        .iter()
        .rev()
        .find(|u| u.role.is_planner())
        .map(|u| {
            json!({
                "label": u.label,
                "role": format!("{:?}", u.role),
                "text": one_line(&u.text, 240),
            })
        });
    let optimization_hints = compose_optimization_hints(
        context_budget_chars,
        project_context_report,
        local_changes,
        salvaged_votes,
        reported_token_usage,
        prompt_stats,
        prompt_phase_stats,
    )
    .lines()
    .map(|line| line.trim_start_matches("- ").to_string())
    .filter(|line| !line.trim().is_empty())
    .collect::<Vec<_>>();

    json!({
        "kind": "tales_session_summary",
        "schema_version": 1,
        "build": crate::build_info::json(),
        "task": bb.task.trim(),
        "outcome": outcome_summary_json(outcome),
        "next_step": resume_next_step(outcome),
        "resume_packet": compose_resume_packet(
            bb,
            recommendation,
            outcome,
            project_context_report,
            local_changes,
        ),
        "recommendation": recommendation_summary_json(recommendation),
        "project_context": project_context_report_summary_json(project_context_report),
        "local_changes": local_change_summary_status_json(local_changes),
        "reported_cost": reported_cost_summary_json(reported_costs),
        "reported_token_usage": reported_token_usage_summary_json(reported_token_usage),
        "prompt_telemetry": {
            "prompts_sent": total_prompt_count,
            "prompt_chars_sent": total_prompt_chars,
            "prompt_tokens_estimate": local_prompt_tokens_estimate,
            "max_prompt_chars": max_prompt_chars,
            "max_prompt_tokens_estimate": estimated_tokens(max_prompt_chars),
            "context_budget_chars": context_budget_chars,
            "context_budget_tokens_estimate": context_budget_chars.map(estimated_tokens),
            "provider_input_tokens": provider_input_tokens,
            "provider_input_ratio_percent": provider_input_tokens
                .map(|tokens| ratio_percent(tokens, local_prompt_tokens_estimate as u64)),
            "by_agent": prompt_stats.iter().map(|(label, stats)| {
                prompt_stats_json(label, stats)
            }).collect::<Vec<_>>(),
            "by_phase": prompt_phase_stats.iter().map(|(phase, stats)| {
                prompt_stats_json(phase, stats)
            }).collect::<Vec<_>>(),
        },
        "optimization_hints": optimization_hints,
        "latest_human_direction": latest_human_direction,
        "latest_planner_output": latest_planner_output,
        "transcript": {
            "utterances": bb.transcript.len(),
            "chars": bb.transcript.iter().map(|u| u.text.chars().count()).sum::<usize>(),
        },
    })
}

fn outcome_summary_json(outcome: &RunOutcome) -> Value {
    match outcome {
        RunOutcome::Discussed => json!({ "status": "discussed" }),
        RunOutcome::Executed { executor, output } => json!({
            "status": "executed",
            "executor": executor,
            "executor_output_chars": output.chars().count(),
            "executor_output_preview": one_line(output, 500),
        }),
        RunOutcome::Rejected => json!({ "status": "rejected" }),
        RunOutcome::Aborted => json!({ "status": "aborted" }),
    }
}

fn recommendation_summary_json(recommendation: Option<&Recommendation>) -> Value {
    match recommendation {
        Some(rec) => json!({
            "available": true,
            "executor": rec.executor,
            "confident": rec.confident,
            "needs_plan": rec.needs_plan,
            "rationale": rec.rationale,
            "scores": rec.scores.iter().map(|(executor, score)| json!({
                "executor": executor,
                "score": score,
            })).collect::<Vec<_>>(),
            "votes": rec.votes.iter().map(|vote| json!({
                "voter": vote.voter.to_string(),
                "voter_label": vote.voter_label,
                "recommended_executor": vote.recommended_executor,
                "confidence": vote.confidence,
                "needs_plan": vote.needs_plan,
                "rationale": vote.rationale,
                "parse_source": vote.parse_source.as_str(),
            })).collect::<Vec<_>>(),
        }),
        None => json!({ "available": false }),
    }
}

fn project_context_report_summary_json(report: Option<&ProjectContextReport>) -> Value {
    match report {
        Some(ctx) => json!({
            "enabled": true,
            "cache_hit": ctx.cache_hit,
            "cache_path": ctx.cache_path,
            "chars": ctx.chars,
            "tokens_estimate": estimated_tokens(ctx.chars),
            "budgets": {
                "max_files": ctx.max_files,
                "max_manifest_chars": ctx.max_manifest_chars,
            },
        }),
        None => json!({ "enabled": false }),
    }
}

fn reported_cost_summary_json(reported_costs: &[(String, f64)]) -> Value {
    if reported_costs.is_empty() {
        return json!({
            "available": false,
            "total_usd": null,
            "entries": [],
        });
    }
    let total: f64 = reported_costs.iter().map(|(_, cost)| *cost).sum();
    json!({
        "available": true,
        "total_usd": total,
        "entries": reported_costs.iter().map(|(label, cost)| json!({
            "label": label,
            "usd": cost,
        })).collect::<Vec<_>>(),
    })
}

fn reported_token_usage_summary_json(reported_token_usage: &[(String, TokenUsage)]) -> Value {
    if reported_token_usage.is_empty() {
        return json!({
            "available": false,
            "total": null,
            "entries": [],
        });
    }
    let mut total = TokenUsage::default();
    for (_, usage) in reported_token_usage {
        total.add(*usage);
    }
    json!({
        "available": true,
        "total": token_usage_json(total),
        "entries": reported_token_usage.iter().map(|(label, usage)| json!({
            "label": label,
            "usage": token_usage_json(*usage),
        })).collect::<Vec<_>>(),
    })
}

fn token_usage_json(usage: TokenUsage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "cached_input_tokens": usage.cached_input_tokens,
        "output_tokens": usage.output_tokens,
        "reasoning_output_tokens": usage.reasoning_output_tokens,
        "total_tokens": usage.total_tokens,
        "total_or_sum": usage.total_or_sum(),
    })
}

fn prompt_stats_json(label: &str, stats: &PromptStats) -> Value {
    json!({
        "label": label,
        "prompts": stats.prompts,
        "chars": stats.chars_sent,
        "tokens_estimate": estimated_tokens(stats.chars_sent),
        "max_chars": stats.max_chars,
        "max_tokens_estimate": estimated_tokens(stats.max_chars),
    })
}

fn compose_resume_packet(
    bb: &Blackboard,
    recommendation: Option<&Recommendation>,
    outcome: &RunOutcome,
    project_context_report: Option<&ProjectContextReport>,
    local_changes: Option<&LocalChangeSummary>,
) -> String {
    let mut packet = String::new();
    packet.push_str(&format!("- task: {}\n", one_line(&bb.task, 220)));
    packet.push_str(&format!("- status: {}\n", outcome_status(outcome)));

    match outcome {
        RunOutcome::Executed { executor, output } => {
            packet.push_str(&format!("- executor: {executor}\n"));
            if !output.trim().is_empty() {
                packet.push_str(&format!("- executor_output: {}\n", one_line(output, 220)));
            }
        }
        RunOutcome::Discussed | RunOutcome::Rejected | RunOutcome::Aborted => {}
    }

    if let Some(rec) = recommendation {
        packet.push_str(&format!(
            "- recommendation: {} (confident: {})\n",
            rec.executor, rec.confident
        ));
        packet.push_str(&format!(
            "- formal_plan_requested_by_both: {}\n",
            rec.needs_plan
        ));
        if !rec.rationale.trim().is_empty() {
            packet.push_str(&format!(
                "- recommendation_rationale: {}\n",
                one_line(&rec.rationale, 220)
            ));
        }
    }

    if let Some(note) = bb.transcript.iter().rev().find(|u| u.role == Role::Human) {
        packet.push_str(&format!(
            "- latest_human_direction: {}: {}\n",
            note.label,
            one_line(&note.text, 220)
        ));
    }

    if let Some(plan) = bb.transcript.iter().rev().find(|u| u.role.is_planner()) {
        packet.push_str(&format!(
            "- latest_planner_output: {} ({:?}): {}\n",
            plan.label,
            plan.role,
            one_line(&plan.text, 220)
        ));
    }

    match project_context_report {
        Some(ctx) => packet.push_str(&format!(
            "- project_context: {} ({} chars, max_files={}, max_manifest_chars={})\n",
            if ctx.cache_hit {
                "cache hit"
            } else {
                "rebuilt"
            },
            ctx.chars,
            ctx.max_files,
            ctx.max_manifest_chars
        )),
        None => packet.push_str("- project_context: not injected\n"),
    }

    match local_changes {
        Some(changes) => packet.push_str(&format!(
            "- local_changes: {}\n",
            one_line(&changes.summary_line(), 220)
        )),
        None => packet.push_str("- local_changes: unavailable\n"),
    }

    packet.push_str(&format!("- next_step: {}\n", resume_next_step(outcome)));
    packet
}

fn outcome_status(outcome: &RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Discussed => "discussed",
        RunOutcome::Executed { .. } => "executed",
        RunOutcome::Rejected => "rejected",
        RunOutcome::Aborted => "aborted",
    }
}

fn resume_next_step(outcome: &RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Discussed => "continue planning or run recommendation before execution",
        RunOutcome::Executed { .. } => "review executor output and local diff",
        RunOutcome::Rejected => "revise the plan with the user before any execution",
        RunOutcome::Aborted => "resume from the latest planner output and user direction",
    }
}

fn indent_block(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn one_line(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (idx, ch) in normalized.chars().enumerate() {
        if idx == max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn compose_optimization_hints(
    context_budget_chars: Option<usize>,
    project_context_report: Option<&ProjectContextReport>,
    local_changes: Option<&LocalChangeSummary>,
    salvaged_votes: usize,
    reported_token_usage: &[(String, TokenUsage)],
    prompt_stats: &[(String, PromptStats)],
    prompt_phase_stats: &[(String, PromptStats)],
) -> String {
    let mut hints = String::new();

    match project_context_report {
        Some(ctx) if ctx.cache_hit => hints.push_str(
            "- project_context_cache: hit; first-turn orientation reused the cached repo map.\n",
        ),
        Some(_) => hints.push_str(
            "- project_context_cache: rebuilt; an unchanged next run should be cheaper from cache.\n",
        ),
        None => hints.push_str(
            "- project_context_cache: off; enable cached project context when agents need repo orientation.\n",
        ),
    }

    match local_changes {
        Some(changes) if !changes.is_clean() => hints.push_str(&format!(
            "- local_changes: execution handoff included {}; this avoids rediscovering the dirty tree and helps preserve unrelated work.\n",
            changes.summary_line()
        )),
        Some(_) => hints.push_str(
            "- local_changes: clean; no dirty-tree handoff context was needed beyond the status line.\n",
        ),
        None => hints.push_str(
            "- local_changes: unavailable; run inside a git repository for cheaper dirty-tree handoffs.\n",
        ),
    }

    if let Some((label, usage, total)) = reported_token_usage
        .iter()
        .filter_map(|(label, usage)| usage.total_or_sum().map(|total| (label, usage, total)))
        .max_by_key(|(_, _, total)| *total)
    {
        let mut details = Vec::new();
        if let Some(input) = usage.input_tokens {
            details.push(format!("input={input}"));
        }
        if let Some(cached) = usage.cached_input_tokens {
            details.push(format!("cached_input={cached}"));
        }
        if let Some(output) = usage.output_tokens {
            details.push(format!("output={output}"));
        }
        if let Some(reasoning) = usage.reasoning_output_tokens {
            details.push(format!("reasoning_output={reasoning}"));
        }
        if details.is_empty() {
            hints.push_str(&format!(
                "- provider_tokens: largest reported usage was {label} with {total} token(s).\n"
            ));
        } else {
            hints.push_str(&format!(
                "- provider_tokens: largest reported usage was {label} with {total} token(s) ({}).\n",
                details.join(", ")
            ));
        }
    }

    let cached_input_tokens: u64 = reported_token_usage
        .iter()
        .filter_map(|(_, usage)| usage.cached_input_tokens)
        .sum();
    if cached_input_tokens > 0 {
        hints.push_str(&format!(
            "- provider_cache: adapters reported {cached_input_tokens} cached input token(s); resumed/provider-side caching is reducing prompt reprocessing where supported.\n"
        ));
    } else if !reported_token_usage.is_empty() {
        hints.push_str(
            "- provider_cache: no cached input tokens were reported; favor resumable adapters and stable prefixes when provider caching is available.\n",
        );
    }

    if prompt_stats.is_empty() {
        hints.push_str("- prompt_budget: no prompt telemetry was recorded for this run.\n");
        return hints;
    }

    let total_chars: usize = prompt_stats.iter().map(|(_, stats)| stats.chars_sent).sum();
    let max_prompt_chars: usize = prompt_stats
        .iter()
        .map(|(_, stats)| stats.max_chars)
        .max()
        .unwrap_or(0);
    let local_prompt_estimate = estimated_tokens(total_chars) as u64;

    if let Some(input_tokens) = reported_input_tokens(reported_token_usage) {
        let ratio = ratio_percent(input_tokens, local_prompt_estimate);
        if ratio >= 200 {
            hints.push_str(&format!(
                "- provider_input_calibration: provider input tokens were {ratio}% of the local prompt estimate; resumed session/tool context likely dominates actual usage.\n"
            ));
        } else if ratio <= 50 {
            hints.push_str(&format!(
                "- provider_input_calibration: provider input tokens were {ratio}% of the local prompt estimate; local chars/4 estimates are conservative for this run.\n"
            ));
        } else {
            hints.push_str(&format!(
                "- provider_input_calibration: provider input tokens were {ratio}% of the local prompt estimate; local forecasting is reasonably aligned.\n"
            ));
        }
    }

    if let Some((label, stats)) = prompt_stats
        .iter()
        .max_by_key(|(_, stats)| stats.chars_sent)
    {
        hints.push_str(&format!(
            "- largest_agent_prompt_spend: {label} sent {} chars (~{} tokens) across {} prompt(s).\n",
            stats.chars_sent,
            estimated_tokens(stats.chars_sent),
            stats.prompts
        ));
    }

    if let Some((phase, stats)) = prompt_phase_stats
        .iter()
        .max_by_key(|(_, stats)| stats.chars_sent)
    {
        hints.push_str(&format!(
            "- largest_phase_prompt_spend: {phase} sent {} chars (~{} tokens).\n",
            stats.chars_sent,
            estimated_tokens(stats.chars_sent)
        ));
    }

    match context_budget_chars {
        Some(budget) if max_prompt_chars >= budget.saturating_mul(9) / 10 => hints.push_str(
            "- context_budget: largest prompt used at least 90% of the cap; lower project-context budgets or keep more agents resumable.\n",
        ),
        Some(budget) if max_prompt_chars <= budget / 2 => hints.push_str(
            "- context_budget: largest prompt stayed under half the cap; current budget has room.\n",
        ),
        Some(_) => hints.push_str(
            "- context_budget: prompt sizes stayed within the configured cap.\n",
        ),
        None => hints.push_str(
            "- context_budget: unlimited; set a context budget for cheaper stateless adapters on long sessions.\n",
        ),
    }

    if let Some((_, repair)) = prompt_phase_stats
        .iter()
        .find(|(phase, _)| phase == "vote_repair")
    {
        hints.push_str(&format!(
            "- structured_output: vote repair cost {} extra prompt(s) (~{} tokens); prefer tools/models that follow JSON-only votes reliably.\n",
            repair.prompts,
            estimated_tokens(repair.chars_sent)
        ));
    }
    if salvaged_votes > 0 {
        hints.push_str(&format!(
            "- structured_output: salvaged {salvaged_votes} prose vote(s) from unambiguous candidate mentions, avoiding up to {salvaged_votes} repair prompt(s); structured-capable tools should still emit JSON.\n",
        ));
    }

    let recommendation_chars = prompt_phase_stats
        .iter()
        .find(|(phase, _)| phase == "recommendation_vote")
        .map(|(_, stats)| stats.chars_sent)
        .unwrap_or(0);
    if total_chars > 0 && recommendation_chars.saturating_mul(3) > total_chars {
        hints.push_str(
            "- recommendation_vote: voting was a large share of prompt spend; keep the executor roster tight for cheaper decisions.\n",
        );
    }

    hints
}

/// Build the execution prompt. If the executor was a resumable planner, the
/// native agent session already contains the debate and recommendation, so avoid
/// re-pasting the full transcript. A separate executor or stateless planner gets
/// the full discussion because it has no reliable session context.
fn compose_execution_prompt(
    task: &str,
    plan: &str,
    has_session_plan: bool,
    context_budget_chars: Option<usize>,
    local_changes: Option<&LocalChangeSummary>,
) -> String {
    let execution_rules =
        "Implement it now. If the discussion or execution packet says both planners requested \
         a formal plan, write that concise plan first and ask the user before editing; otherwise \
         proceed directly. Use your file-writing tool (Write/Edit) to create \
         every file — it creates parent directories automatically, so do NOT \
         use shell commands like mkdir. Create all the files the task needs \
         for in this turn. When finished, briefly summarize what you wrote.";
    let local_change_note = local_changes
        .map(|changes| {
            format!(
                "\n\n{}\n",
                bounded_context(&changes.to_handoff_text(), Some(2_000))
            )
        })
        .unwrap_or_default();

    if has_session_plan {
        format!(
            "You are now EXECUTING from the team discussion in this session.\n\
             Task: {task}\n\n\
             Use the discussion already in your session memory; do not make a \
             formal plan unless both planners explicitly requested one.\n\
             {local_change_note}\
             {execution_rules}"
        )
    } else {
        format!(
            "You are now EXECUTING from the team discussion.\n\
             Task: {task}\n\n\
             Discussion:\n{}\n\
             {execution_rules}",
            bounded_execution_prompt_context(plan, context_budget_chars)
        )
    }
}

fn bounded_execution_prompt_context(context: &str, context_budget_chars: Option<usize>) -> String {
    let context = context.trim();
    if context.starts_with("Execution packet\n") {
        return context.to_string();
    }
    bounded_context(context, context_budget_chars)
}

fn project_context_block(
    project_context: Option<&str>,
    context_budget_chars: Option<usize>,
) -> String {
    project_context
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| {
            let text = bounded_project_context(text, context_budget_chars);
            format!("\nCached project context:\n{text}\n")
        })
        .unwrap_or_default()
}

fn bounded_project_context(text: &str, max_chars: Option<usize>) -> String {
    let trimmed = text.trim();
    let Some(max_chars) = max_chars else {
        return trimmed.to_string();
    };
    if max_chars == 0 || trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let sections = split_project_context_sections(trimmed);
    if sections.len() <= 1 {
        return bounded_context(trimmed, Some(max_chars));
    }

    let section_budget = split_context_budget(Some(max_chars), sections.len());
    let mut out = format!(
        "(project context capped; preserving {} sections)\n",
        sections.len()
    );
    for section in sections {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&bounded_project_context_section(&section, section_budget));
        out.push('\n');
    }
    out.trim().to_string()
}

fn split_project_context_sections(text: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if is_project_context_section_heading(line) && !current.trim().is_empty() {
            sections.push(current.trim().to_string());
            current.clear();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        sections.push(current.trim().to_string());
    }
    sections
}

fn is_project_context_section_heading(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("Project context ")
        || matches!(
            line,
            "Important manifests:" | "Git working tree:" | "File map:" | "Manifest excerpts:"
        )
}

fn bounded_project_context_section(section: &str, max_chars: Option<usize>) -> String {
    let mut lines = section.lines();
    let heading = lines.next().unwrap_or_default();
    let rest = lines.collect::<Vec<_>>().join("\n");
    if is_project_context_section_heading(heading) && !rest.trim().is_empty() {
        let body_budget =
            max_chars.map(|chars| chars.saturating_sub(heading.chars().count() + 1).max(1));
        format!("{heading}\n{}", bounded_context_edges(&rest, body_budget))
    } else {
        bounded_context(section, max_chars)
    }
}

fn bounded_context_edges(text: &str, max_chars: Option<usize>) -> String {
    let trimmed = text.trim();
    let Some(max_chars) = max_chars else {
        return trimmed.to_string();
    };
    if max_chars == 0 || trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let head_chars = (max_chars / 2).max(1);
    let tail_chars = max_chars.saturating_sub(head_chars).max(1);
    let head: String = trimmed.chars().take(head_chars).collect();
    let tail: String = trimmed
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "(middle context omitted; showing first {head_chars} and last {tail_chars} chars)\n{head}\n...\n{tail}"
    )
}

/// The DRAFTER role intro for a planner's first turn. Shared by the resumable
/// and stateless paths so the wording can't drift between them. `context_block`
/// is the already-rendered cached-project-context section (may be empty).
fn drafter_intro(task: &str, context_block: &str) -> String {
    format!(
        "You are the DRAFTER in an implementation discussion with a critic.\n\
         Task: {task}\n\
         {context_block}\n\
         Propose the simplest viable implementation approach, the concrete \
         files or components you would change, and the key risks. State whether \
         this task really needs a formal plan. Ground each claim in the code \
         shown above when it is provided rather than guessing. Be specific and \
         brief; no preamble. Do not write a full plan unless the task clearly \
         needs one."
    )
}

/// The CRITIC role intro reviewing a `draft`. Shared by both paths. Asks the
/// critic to green-light a sound approach instead of manufacturing concerns, so
/// the discussion can converge toward execution.
fn critic_intro(task: &str, context_block: &str, draft: &str) -> String {
    format!(
        "You are the CRITIC reviewing an implementation approach.\n\
         Task: {task}\n\n\
         {context_block}\
         Latest draft:\n{draft}\n\n\
         Challenge concrete problems, missing edge cases, and whether a formal \
         plan is actually needed. If the approach is sound with no blocking \
         issues, say so plainly instead of inventing concerns. Be specific and \
         brief; no preamble."
    )
}

/// Build the prompt for a SEQUENTIAL planning turn. A *resumable* adapter keeps
/// its own prior turns in its session, so it is sent only the role intro (the
/// first time it speaks) plus the unseen `delta` — never the whole transcript
/// re-pasted. A stateless adapter has no cross-turn memory, so it falls back to
/// the full-context prompt below. `delta` is the transcript tail the agent
/// hasn't been shown yet; `first_time` is whether it has spoken before.
#[allow(clippy::too_many_arguments)]
fn compose_prompt(
    role: Role,
    task: &str,
    bb: &Blackboard,
    resumable: bool,
    delta: &str,
    first_time: bool,
    context_budget_chars: Option<usize>,
    project_context: Option<&str>,
) -> String {
    let first_turn_project_context = first_time.then_some(project_context).flatten();
    if !resumable {
        return compose_prompt_full(
            role,
            task,
            bb,
            context_budget_chars,
            first_turn_project_context,
        );
    }
    let delta = bounded_context(delta, context_budget_chars);
    match role {
        Role::Drafter if first_time => drafter_intro(
            task,
            &project_context_block(first_turn_project_context, context_budget_chars),
        ),
        Role::Drafter => format!(
            "The critic responded:\n{delta}\n\n\
             Respond with the updated implementation argument, converging on \
             what is already settled rather than reopening it. If a formal plan \
             is still needed, say why; otherwise end with the concrete, \
             executable handoff (the files to change and the order of steps)."
        ),
        Role::Critic if first_time => critic_intro(
            task,
            &project_context_block(first_turn_project_context, context_budget_chars),
            &delta,
        ),
        Role::Critic => format!(
            "The drafter revised:\n{delta}\n\n\
             Critique the update: remaining problems, gaps, and whether a \
             formal plan is necessary. If nothing now blocks execution, say so \
             plainly so the team can move on. Be specific and brief."
        ),
        Role::Human | Role::Executor => task.to_string(),
    }
}

/// The full-context prompt: re-sends the whole discussion and restates the role
/// every turn. Used for stateless adapters (no session to resume), and it is the
/// exact behavior the engine had before delta context — so the sequential path
/// with a stateless adapter is behaviorally unchanged until the context budget
/// is exceeded, at which point the oldest transcript is trimmed.
fn compose_prompt_full(
    role: Role,
    task: &str,
    bb: &Blackboard,
    context_budget_chars: Option<usize>,
    project_context: Option<&str>,
) -> String {
    match role {
        Role::Drafter => {
            if bb.transcript.is_empty() {
                drafter_intro(
                    task,
                    &project_context_block(project_context, context_budget_chars),
                )
            } else {
                format!(
                    "You are the DRAFTER. Task: {task}\n\n\
                     {}\
                     Discussion so far:\n{}\n\
                     Update the implementation argument to address the critic's \
                     points, converging on what is settled rather than reopening \
                     it. If a formal plan is needed, say why; otherwise end with \
                     the concrete, executable handoff (the files to change and \
                     the order of steps).",
                    project_context_block(project_context, context_budget_chars),
                    bounded_context(&bb.transcript_text(), context_budget_chars)
                )
            }
        }
        Role::Critic => {
            let latest = bb
                .last_text()
                .map(|text| bounded_context(text, context_budget_chars))
                .unwrap_or_else(|| "(no draft yet)".to_string());
            critic_intro(
                task,
                &project_context_block(project_context, context_budget_chars),
                &latest,
            )
        }
        // Humans are recorded out-of-band; executors are filtered out of
        // planning and run via `run_execution`. Neither is scheduled here — this
        // arm is a safe fallback, not a code path, so it must never panic.
        Role::Human | Role::Executor => task.to_string(),
    }
}

/// Round-1 prompt for the PARALLEL path: each planner drafts INDEPENDENTLY (it
/// hasn't seen the other yet), so a round costs `max(planners)` latency, not the
/// sum. Two independent proposals then cross-pollinate — as good or better than
/// serial drafter→critic, and far faster.
fn compose_round1_prompt(
    task: &str,
    project_context: Option<&str>,
    context_budget_chars: Option<usize>,
) -> String {
    format!(
        "You are one of two expert implementers discussing the SAME task in parallel.\n\
         Task: {task}\n\
         {}\n\
         Produce your own concise implementation argument: simplest viable \
         approach, the concrete files or components you would change, the risks, \
         and whether this task needs a formal plan. Ground each claim in the code \
         shown above when it is provided rather than guessing. Do not write a \
         full plan unless it clearly needs one. You'll compare with the other \
         proposal next. Bullet points are fine; no preamble.",
        project_context_block(project_context, context_budget_chars)
    )
}

/// Round-2+ prompt for the PARALLEL path. The synthesizer merges into one plan;
/// the other planner runs an adversarial cross-review (preserving a real critique
/// pass). `others` is the competing proposal(s) from the previous round; for a
/// stateless adapter `own` (its own prior output) is included since it can't
/// recall it.
fn compose_round_synth_prompt(
    task: &str,
    others: &str,
    synthesize: bool,
    resumable: bool,
    own: &str,
    context_budget_chars: Option<usize>,
) -> String {
    let includes_own = !resumable && !own.trim().is_empty();
    let section_budget = if includes_own {
        split_context_budget(context_budget_chars, 2)
    } else {
        context_budget_chars
    };
    let others = bounded_context(others, section_budget);
    let own = bounded_context(own, section_budget);
    let context = if !includes_own {
        format!("The other planner proposed:\n{}", others.trim())
    } else {
        format!(
            "Your proposal:\n{}\n\nThe other planner proposed:\n{}",
            own.trim(),
            others.trim()
        )
    };
    if synthesize {
        format!(
            "Task: {task}\n\n{context}\n\n\
             Identify the concrete defects and missing edge cases in the competing \
             proposal, then produce the single merged implementation recommendation. \
             Make it concrete and executable — name the files to change and the \
             order of steps. If either side thinks no formal plan is needed, do not \
             write one; keep only the execution guidance. Output only the merged \
             recommendation."
        )
    } else {
        format!(
            "Task: {task}\n\n{context}\n\n\
             Adversarially review the competing proposal: list its concrete \
             problems, risks, missing edge cases, and whether a formal plan is \
             actually needed. If it is sound with no blocking issues, say so \
             plainly. Be specific and brief."
        )
    }
}

/// The agent an [`AgentEvent`] belongs to — used to demux a parallel round's
/// interleaved events on the single shared stream.
fn event_agent(e: &AgentEvent) -> AgentId {
    match e {
        AgentEvent::SessionReady { agent, .. }
        | AgentEvent::Skills { agent, .. }
        | AgentEvent::TurnStarted { agent, .. }
        | AgentEvent::TokenDelta { agent, .. }
        | AgentEvent::MessageFinal { agent, .. }
        | AgentEvent::ToolCall { agent, .. }
        | AgentEvent::TurnComplete { agent, .. }
        | AgentEvent::Error { agent, .. }
        | AgentEvent::Exited { agent, .. } => *agent,
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    fn bb_with(items: &[(&str, Role, &str)]) -> Blackboard {
        let mut bb = Blackboard::default();
        for (label, role, text) in items {
            bb.record(label.to_string(), *role, text.to_string());
        }
        bb
    }

    fn bb_with_task(task: &str) -> Blackboard {
        Blackboard {
            task: task.into(),
            ..Blackboard::default()
        }
    }

    fn dirty_changes() -> LocalChangeSummary {
        LocalChangeSummary {
            branch: Some("main".into()),
            changes: vec![
                " M crates/tales-core/src/orchestrator.rs".into(),
                "?? landing/demo.mp4".into(),
            ],
            truncated: false,
            omitted: 1,
        }
    }

    #[test]
    fn resumable_revision_sends_only_the_delta_not_role_or_own_history() {
        // A resumable drafter on a later turn: prompt carries the critic's delta,
        // NOT the "You are the DRAFTER" intro and NOT its own earlier draft.
        let bb = bb_with(&[
            ("claude", Role::Drafter, "MY-OWN-DRAFT"),
            ("codex", Role::Critic, "CRITIC-DELTA"),
        ]);
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            true,
            "CRITIC-DELTA",
            false,
            None,
            None,
        );
        assert!(p.contains("CRITIC-DELTA"), "delta missing: {p}");
        assert!(
            !p.contains("You are the DRAFTER"),
            "re-sent role intro: {p}"
        );
        assert!(!p.contains("MY-OWN-DRAFT"), "re-pasted own history: {p}");
    }

    #[test]
    fn resumable_first_turn_introduces_role() {
        let bb = Blackboard::default();
        let p = compose_prompt(
            Role::Critic,
            "task",
            &bb,
            true,
            "THE-DRAFT",
            true,
            None,
            None,
        );
        assert!(p.contains("You are the CRITIC"), "{p}");
        assert!(p.contains("THE-DRAFT"), "{p}");
    }

    #[test]
    fn first_turn_prompts_include_cached_project_context() {
        let bb = Blackboard::default();
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            true,
            "",
            true,
            None,
            Some("Project context (cached by Tales)\n- crates/tales-core/src/lib.rs"),
        );
        assert!(p.contains("Cached project context"), "{p}");
        assert!(p.contains("crates/tales-core/src/lib.rs"), "{p}");

        let parallel = compose_round1_prompt("task", Some("File map:\n- Cargo.toml"), None);
        assert!(parallel.contains("Cached project context"), "{parallel}");
        assert!(parallel.contains("Cargo.toml"), "{parallel}");
    }

    #[test]
    fn first_turn_project_context_respects_context_budget() {
        let bb = Blackboard::default();
        let project_context = format!(
            "Project context (cached by Tales)\nEARLY-FILE\n{}\nLATE-FILE",
            "x".repeat(80)
        );
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            true,
            "",
            true,
            Some(20),
            Some(&project_context),
        );
        assert!(p.contains("Cached project context"), "{p}");
        assert!(p.contains("earlier context omitted"), "{p}");
        assert!(!p.contains("EARLY-FILE"), "{p}");
        assert!(p.contains("LATE-FILE"), "{p}");

        let parallel = compose_round1_prompt("task", Some(&project_context), Some(20));
        assert!(parallel.contains("Cached project context"), "{parallel}");
        assert!(parallel.contains("earlier context omitted"), "{parallel}");
        assert!(!parallel.contains("EARLY-FILE"), "{parallel}");
        assert!(parallel.contains("LATE-FILE"), "{parallel}");
    }

    #[test]
    fn capped_project_context_preserves_repo_sections() {
        let bb = Blackboard::default();
        let project_context = format!(
            "Project context (cached by Tales)\n\
             Root: /repo\n\
             Indexed files: 200\n\n\
             Important manifests:\n\
             - Cargo.toml\n\
             - crates/tales-core/Cargo.toml\n\n\
             Git working tree:\n\
             - branch: main\n\
             - changes:\n\
               - M crates/tales-core/src/orchestrator.rs\n\n\
             File map:\n\
             - EARLY-FILE.rs\n\
             - {}\n\
             - LATE-FILE.rs\n\n\
             Manifest excerpts:\n\
             --- Cargo.toml ---\n\
             [workspace]\n\
             members = [\"crates/tales-core\"]\n",
            "x".repeat(160)
        );
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            true,
            "",
            true,
            Some(240),
            Some(&project_context),
        );

        assert!(p.contains("project context capped"), "{p}");
        assert!(p.contains("Project context (cached by Tales)"), "{p}");
        assert!(p.contains("Important manifests:"), "{p}");
        assert!(p.contains("Cargo.toml"), "{p}");
        assert!(p.contains("Git working tree:"), "{p}");
        assert!(p.contains("branch: main"), "{p}");
        assert!(p.contains("File map:"), "{p}");
        assert!(p.contains("EARLY-FILE.rs"), "{p}");
        assert!(p.contains("LATE-FILE.rs"), "{p}");
        assert!(p.contains("Manifest excerpts:"), "{p}");
        assert!(p.contains("middle context omitted"), "{p}");
    }

    #[test]
    fn resumable_delta_respects_context_budget() {
        let bb = bb_with(&[
            ("claude", Role::Drafter, "EARLY-DRAFT"),
            ("codex", Role::Critic, "LATE-CRITIQUE"),
        ]);
        let delta = format!("EARLY-DELTA {} LATE-DELTA", "x".repeat(80));
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            true,
            &delta,
            false,
            Some(20),
            None,
        );
        assert!(p.contains("earlier context omitted"), "{p}");
        assert!(!p.contains("EARLY-DELTA"), "{p}");
        assert!(p.contains("LATE-DELTA"), "{p}");
    }

    #[test]
    fn stateless_first_turn_includes_cached_project_context() {
        let bb = Blackboard::default();
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            false,
            "",
            true,
            None,
            Some("Project context (cached by Tales)\n- Cargo.toml"),
        );
        assert!(p.contains("Cached project context"), "{p}");
        assert!(p.contains("Cargo.toml"), "{p}");
    }

    #[test]
    fn stateless_later_turn_omits_repeated_project_context() {
        let bb = bb_with(&[("claude", Role::Drafter, "EARLIER-DRAFT")]);
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            false,
            "ignored",
            false,
            None,
            Some("Project context (cached by Tales)\n- SHOULD-NOT-REPEAT"),
        );
        assert!(p.contains("You are the DRAFTER"), "{p}");
        assert!(p.contains("EARLIER-DRAFT"), "{p}");
        assert!(!p.contains("Cached project context"), "{p}");
        assert!(!p.contains("SHOULD-NOT-REPEAT"), "{p}");
    }

    #[test]
    fn stateless_adapter_keeps_full_context_behavior() {
        // resumable=false → role restated and full transcript re-sent unless a
        // context budget requires trimming.
        let bb = bb_with(&[("claude", Role::Drafter, "EARLIER-DRAFT")]);
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            false,
            "ignored",
            false,
            None,
            None,
        );
        assert!(p.contains("You are the DRAFTER"), "{p}");
        assert!(p.contains("EARLIER-DRAFT"), "full transcript expected: {p}");
    }

    #[test]
    fn stateless_adapter_context_can_be_bounded() {
        let bb = bb_with(&[("claude", Role::Drafter, "EARLY-CONTEXT LATE-CONTEXT")]);
        let p = compose_prompt(
            Role::Drafter,
            "task",
            &bb,
            false,
            "ignored",
            false,
            Some(12),
            None,
        );
        assert!(p.contains("earlier context omitted"), "{p}");
        assert!(!p.contains("EARLY-CONTEXT"), "{p}");
        assert!(p.contains("LATE-CONTEXT"), "{p}");
    }

    #[test]
    fn intros_ask_for_concrete_grounded_output_and_greenlight() {
        let bb = Blackboard::default();
        // Drafter asks for the concrete files to change.
        let drafter = compose_prompt(Role::Drafter, "task", &bb, true, "", true, None, None);
        assert!(drafter.contains("You are the DRAFTER"), "{drafter}");
        assert!(drafter.to_lowercase().contains("files"), "{drafter}");
        // Critic is told to green-light a sound approach instead of nitpicking.
        let critic = compose_prompt(Role::Critic, "task", &bb, true, "THE-DRAFT", true, None, None);
        assert!(critic.contains("THE-DRAFT"), "{critic}");
        assert!(critic.to_lowercase().contains("sound"), "{critic}");
        // The parallel merge asks for an executable, file-level conclusion.
        let synth = compose_round_synth_prompt("task", "OTHER", true, true, "OWN", None);
        assert!(synth.to_lowercase().contains("executable"), "{synth}");
    }

    #[test]
    fn round2_synth_merges_and_review_critiques() {
        let synth = compose_round_synth_prompt("task", "OTHER-PLAN", true, true, "OWN", None);
        assert!(synth.contains("OTHER-PLAN"));
        assert!(synth.to_lowercase().contains("merged"));
        // resumable synth doesn't re-paste its own proposal.
        assert!(!synth.contains("OWN"), "{synth}");

        let review = compose_round_synth_prompt("task", "OTHER-PLAN", false, true, "OWN", None);
        assert!(review.to_lowercase().contains("review"));
        assert!(review.contains("OTHER-PLAN"));
    }

    #[test]
    fn stateless_round2_includes_own_proposal() {
        // A stateless planner can't recall its round-1 output, so it must be
        // re-sent alongside the competitor's.
        let p = compose_round_synth_prompt("task", "OTHER-PLAN", true, false, "MY-PLAN", None);
        assert!(p.contains("MY-PLAN"), "{p}");
        assert!(p.contains("OTHER-PLAN"), "{p}");
    }

    #[test]
    fn stateless_round2_splits_context_budget_between_sections() {
        let p = compose_round_synth_prompt(
            "task",
            "OTHER-EARLY OTHER-LATE",
            true,
            false,
            "OWN-EARLY OWN-LATE",
            Some(10),
        );
        assert!(p.contains("Your proposal"), "{p}");
        assert!(p.contains("The other planner proposed"), "{p}");
        assert!(p.contains("showing last 5 chars"), "{p}");
        assert!(!p.contains("OWN-EARLY"), "{p}");
        assert!(!p.contains("OTHER-EARLY"), "{p}");
        assert!(p.contains("LATE"), "{p}");
    }

    #[test]
    fn resumable_round2_keeps_full_budget_for_other_proposal() {
        let p = compose_round_synth_prompt(
            "task",
            "OTHER-EARLY OTHER-LATE",
            true,
            true,
            "OWN-EARLY OWN-LATE",
            Some(10),
        );
        assert!(p.contains("showing last 10 chars"), "{p}");
        assert!(!p.contains("Your proposal"), "{p}");
    }

    #[test]
    fn vote_prompt_keeps_resumable_lean_but_stateless_contextual() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        let lean = compose_vote_prompt("task", &candidates, true, "COMPACT-CONTEXT");
        assert!(lean.contains("already in this session"), "{lean}");
        assert!(!lean.contains("COMPACT-CONTEXT"), "{lean}");

        let full = compose_vote_prompt("task", &candidates, false, "COMPACT-CONTEXT");
        assert!(full.contains("Discussion context"), "{full}");
        assert!(full.contains("COMPACT-CONTEXT"), "{full}");
    }

    #[test]
    fn vote_context_keeps_recent_decision_context() {
        let mut bb = Blackboard::default();
        bb.record("claude".into(), Role::Drafter, "STALE-PLAN".into());
        bb.record(
            "you".into(),
            Role::Human,
            "prefer the cheap executor".into(),
        );
        bb.record("codex".into(), Role::Critic, "RISK-1".into());
        bb.record("claude".into(), Role::Drafter, "PLAN-2".into());
        bb.record("codex".into(), Role::Critic, "RISK-2".into());
        bb.record("claude".into(), Role::Drafter, "FINAL-PLAN".into());

        let context = compose_vote_context(&bb, None);
        assert!(context.contains("Latest human direction"), "{context}");
        assert!(context.contains("prefer the cheap executor"), "{context}");
        assert!(context.contains("FINAL-PLAN"), "{context}");
        assert!(context.contains("RISK-2"), "{context}");
        assert!(!context.contains("STALE-PLAN"), "{context}");
    }

    #[test]
    fn vote_context_respects_budget() {
        let bb = bb_with(&[(
            "claude",
            Role::Drafter,
            "EARLY-CONTEXT MIDDLE-CONTEXT LATE-CONTEXT",
        )]);
        let context = compose_vote_context(&bb, Some(12));
        assert!(context.contains("earlier context omitted"), "{context}");
        assert!(!context.contains("EARLY-CONTEXT"), "{context}");
        assert!(context.contains("LATE-CONTEXT"), "{context}");
    }

    #[test]
    fn vote_repair_is_lean_for_resumable_and_contextual_for_stateless() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        let lean =
            compose_vote_repair_prompt("task", &candidates, true, "FULL-TRANSCRIPT", "not json");
        assert!(lean.contains("not valid JSON"), "{lean}");
        assert!(!lean.contains("FULL-TRANSCRIPT"), "{lean}");
        assert!(lean.contains("claude | codex"), "{lean}");

        let full =
            compose_vote_repair_prompt("task", &candidates, false, "COMPACT-CONTEXT", "not json");
        assert!(full.contains("Discussion context"), "{full}");
        assert!(full.contains("COMPACT-CONTEXT"), "{full}");
    }

    #[tokio::test]
    async fn recommendation_salvages_unambiguous_prose_vote_without_repair_prompt() {
        use crate::agent::mock::MockAdapter;
        use uuid::Uuid;

        let (bus, _rx) = EventBus::new(64, 16);
        let mut orch = Orchestrator::new(bus);
        orch.add_agent(
            Box::new(MockAdapter::new(vec![
                "draft".into(),
                "Claude should execute because the patch touches core files.".into(),
            ])),
            SpawnCtx {
                agent: Uuid::new_v4(),
                label: "claude".into(),
                cwd: std::env::current_dir().unwrap(),
                model: None,
                effort: None,
                permission_mode: "acceptEdits".into(),
                sandbox: "read-only".into(),
                allowed_tools: None,
            },
            Role::Drafter,
        )
        .await
        .unwrap();
        orch.add_agent(
            Box::new(MockAdapter::new(vec![
                "critique".into(),
                r#"{"recommended_executor":"claude","confidence":0.8,"rationale":"agreed"}"#.into(),
            ])),
            SpawnCtx {
                agent: Uuid::new_v4(),
                label: "codex".into(),
                cwd: std::env::current_dir().unwrap(),
                model: None,
                effort: None,
                permission_mode: "acceptEdits".into(),
                sandbox: "read-only".into(),
                allowed_tools: None,
            },
            Role::Critic,
        )
        .await
        .unwrap();

        orch.run_discussion("task", 2).await.unwrap();
        let recommendation = orch.run_recommendation().await.unwrap();

        assert_eq!(recommendation.executor, "claude");
        assert_eq!(orch.salvaged_votes, 1);
        assert!(
            !orch
                .reported_prompt_phase_stats()
                .iter()
                .any(|(phase, _)| phase == "vote_repair"),
            "salvaged vote should avoid the repair prompt"
        );
        assert!(
            recommendation
                .rationale
                .contains("salvaged from prose vote"),
            "{}",
            recommendation.rationale
        );
    }

    #[test]
    fn execution_prompt_avoids_reposting_plan_for_resumable_planner() {
        let lean = compose_execution_prompt("task", "FULL-PLAN", true, None, None);
        assert!(lean.contains("already in your session memory"), "{lean}");
        assert!(!lean.contains("FULL-PLAN"), "{lean}");

        let full = compose_execution_prompt("task", "FULL-PLAN", false, None, None);
        assert!(full.contains("FULL-PLAN"), "{full}");
    }

    #[test]
    fn execution_prompt_bounds_stateless_plan_context() {
        let full = compose_execution_prompt("task", "EARLY-PLAN LATE-PLAN", false, Some(9), None);
        assert!(full.contains("earlier context omitted"), "{full}");
        assert!(!full.contains("EARLY-PLAN"), "{full}");
        assert!(full.contains("LATE-PLAN"), "{full}");
    }

    #[test]
    fn execution_prompt_preserves_prebudgeted_packet_under_tight_cap() {
        let packet = "Execution packet\n\
Task: task\n\
Chosen executor: codex\n\
Context policy: compact packet; older debate may be omitted.\n\n\
Recommendation:\n\
- recommended: codex (confident: true)\n\
- rationale:\nimportant rationale\n\n\
Latest planner outputs:\n\
(middle context omitted; showing first 8 and last 8 chars)\n\
FINAL-PLAN";
        let prompt = compose_execution_prompt("task", packet, false, Some(12), None);

        assert!(prompt.contains("Execution packet"), "{prompt}");
        assert!(prompt.contains("Chosen executor: codex"), "{prompt}");
        assert!(prompt.contains("Recommendation:"), "{prompt}");
        assert!(prompt.contains("important rationale"), "{prompt}");
        assert!(prompt.contains("FINAL-PLAN"), "{prompt}");
        assert!(!prompt.contains("showing last 12 chars"), "{prompt}");
    }

    #[test]
    fn resumable_execution_prompt_still_gets_local_change_handoff() {
        let changes = dirty_changes();
        let lean = compose_execution_prompt("task", "FULL-PLAN", true, None, Some(&changes));
        assert!(lean.contains("already in your session memory"), "{lean}");
        assert!(!lean.contains("FULL-PLAN"), "{lean}");
        assert!(lean.contains("Local changes before execution"), "{lean}");
        assert!(lean.contains("orchestrator.rs"), "{lean}");
        assert!(
            lean.contains("preserve unrelated existing changes"),
            "{lean}"
        );
    }

    #[test]
    fn execution_packet_uses_recent_planner_outputs_and_recommendation() {
        let mut bb = Blackboard::default();
        bb.record("claude".into(), Role::Drafter, "STALE-PLAN".into());
        bb.record("you".into(), Role::Human, "keep it minimal".into());
        bb.record("codex".into(), Role::Critic, "RISK-1".into());
        bb.record("claude".into(), Role::Drafter, "PLAN-2".into());
        bb.record("codex".into(), Role::Critic, "RISK-2".into());
        bb.record("claude".into(), Role::Drafter, "FINAL-PLAN".into());
        let rec = Recommendation {
            executor: "gemini".into(),
            scores: vec![("gemini".into(), 0.8), ("claude".into(), 0.4)],
            votes: Vec::new(),
            rationale: "cheap executor can apply the agreed small diff".into(),
            needs_plan: false,
            confident: true,
        };

        let packet = compose_execution_packet("task", "gemini", &bb, Some(&rec), None, None);
        assert!(packet.contains("Execution packet"), "{packet}");
        assert!(packet.contains("Chosen executor: gemini"), "{packet}");
        assert!(packet.contains("cheap executor"), "{packet}");
        assert!(packet.contains("keep it minimal"), "{packet}");
        assert!(packet.contains("FINAL-PLAN"), "{packet}");
        assert!(packet.contains("RISK-2"), "{packet}");
        assert!(!packet.contains("STALE-PLAN"), "{packet}");
    }

    #[test]
    fn execution_packet_budget_trims_body_but_keeps_header() {
        let bb = bb_with(&[(
            "claude",
            Role::Drafter,
            "EARLY-CONTEXT MIDDLE-CONTEXT LATE-CONTEXT",
        )]);
        let packet = compose_execution_packet("task", "codex", &bb, None, Some(12), None);
        assert!(packet.contains("Task: task"), "{packet}");
        assert!(packet.contains("Chosen executor: codex"), "{packet}");
        assert!(packet.contains("earlier context omitted"), "{packet}");
        assert!(!packet.contains("EARLY-CONTEXT"), "{packet}");
        assert!(packet.contains("LATE-CONTEXT"), "{packet}");
    }

    #[test]
    fn execution_packet_budget_preserves_priority_sections() {
        let mut bb = Blackboard::default();
        bb.record(
            "you".into(),
            Role::Human,
            format!(
                "EARLY-HUMAN {}\nmust preserve user instruction",
                "x".repeat(120)
            ),
        );
        bb.record(
            "claude".into(),
            Role::Drafter,
            format!("EARLY-PLAN {}\nFINAL-PLAN", "x".repeat(120)),
        );
        let rec = Recommendation {
            executor: "codex".into(),
            scores: vec![("codex".into(), 0.9)],
            votes: Vec::new(),
            rationale: format!("EARLY-RATIONALE {}\nimportant rationale", "x".repeat(120)),
            needs_plan: false,
            confident: true,
        };
        let changes = LocalChangeSummary {
            branch: Some("main".into()),
            changes: (0..20)
                .map(|idx| format!(" M crates/tales-core/src/file_{idx}.rs"))
                .collect(),
            truncated: true,
            omitted: 0,
        };

        let packet =
            compose_execution_packet("task", "codex", &bb, Some(&rec), Some(240), Some(&changes));

        assert!(packet.contains("Chosen executor: codex"), "{packet}");
        assert!(packet.contains("Recommendation:"), "{packet}");
        assert!(packet.contains("important rationale"), "{packet}");
        assert!(packet.contains("Latest human direction:"), "{packet}");
        assert!(
            packet.contains("must preserve user instruction"),
            "{packet}"
        );
        assert!(packet.contains("Latest planner outputs:"), "{packet}");
        assert!(packet.contains("FINAL-PLAN"), "{packet}");
        assert!(
            packet.contains("Local changes before execution"),
            "{packet}"
        );
        assert!(packet.contains("earlier context omitted"), "{packet}");
        assert!(!packet.contains("EARLY-HUMAN"), "{packet}");
        assert!(!packet.contains("EARLY-RATIONALE"), "{packet}");
    }

    #[test]
    fn execution_packet_includes_local_change_summary_for_separate_executors() {
        let bb = bb_with(&[("claude", Role::Drafter, "FINAL-PLAN")]);
        let changes = dirty_changes();
        let packet = compose_execution_packet("task", "codex", &bb, None, None, Some(&changes));
        assert!(
            packet.contains("Local changes before execution"),
            "{packet}"
        );
        assert!(
            packet.contains("crates/tales-core/src/orchestrator.rs"),
            "{packet}"
        );
        assert!(packet.contains("landing/demo.mp4"), "{packet}");
        assert!(
            packet.contains("omitted_generated_or_dependency_entries: 1"),
            "{packet}"
        );
    }

    #[test]
    fn session_report_includes_cost_when_available() {
        let mut bb = bb_with_task("ship feature");
        bb.record("claude".into(), Role::Drafter, "plan".into());

        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Executed {
                executor: "claude".into(),
                output: "done".into(),
            },
            None,
            None,
            None,
            0,
            &[("claude (Drafter)".into(), 0.12345)],
            &[],
            &[],
            &[],
        );

        assert!(report.contains("## Reported cost"), "{report}");
        assert!(report.contains("reported_total_usd: $0.1235"), "{report}");
        assert!(report.contains("claude (Drafter): $0.1235"), "{report}");
    }

    #[test]
    fn session_summary_includes_machine_readable_handoff_and_telemetry() {
        let mut bb = bb_with_task("ship feature");
        bb.record(
            "you".into(),
            Role::Human,
            "prefer the cheap executor".into(),
        );
        bb.record("claude".into(), Role::Drafter, "final plan".into());
        let usage = TokenUsage {
            input_tokens: Some(120),
            cached_input_tokens: Some(20),
            output_tokens: Some(40),
            reasoning_output_tokens: None,
            total_tokens: Some(160),
        };
        let prompt_stats = vec![(
            "claude (Drafter)".into(),
            PromptStats {
                prompts: 2,
                chars_sent: 800,
                max_chars: 500,
            },
        )];

        let summary = compose_session_summary(
            &bb,
            None,
            &RunOutcome::Executed {
                executor: "codex".into(),
                output: "patched files".into(),
            },
            Some(24_000),
            None,
            Some(&dirty_changes()),
            0,
            &[("claude (Drafter)".into(), 0.12345)],
            &[("codex (Executor)".into(), usage)],
            &prompt_stats,
            &[("planning".into(), prompt_stats[0].1.clone())],
        );

        assert_eq!(summary["kind"], "tales_session_summary");
        assert_eq!(summary["outcome"]["status"], "executed");
        assert_eq!(summary["outcome"]["executor"], "codex");
        assert_eq!(summary["reported_cost"]["available"], true);
        assert_eq!(
            summary["reported_token_usage"]["total"]["total_or_sum"],
            160
        );
        assert_eq!(summary["prompt_telemetry"]["prompts_sent"], 2);
        assert_eq!(
            summary["prompt_telemetry"]["by_phase"][0]["label"],
            "planning"
        );
        assert_eq!(
            summary["latest_human_direction"]["text"],
            "prefer the cheap executor"
        );
        assert!(summary["resume_packet"]
            .as_str()
            .unwrap()
            .contains("next_step"));
    }

    #[test]
    fn session_summary_includes_recommendation_vote_parse_source() {
        let bb = bb_with_task("pick executor");
        let recommendation = Recommendation {
            executor: "claude".into(),
            scores: vec![("claude".into(), 0.5)],
            votes: vec![ExecutionVote {
                voter: AgentId::new_v4(),
                voter_label: "codex".into(),
                recommended_executor: "claude".into(),
                confidence: 0.5,
                needs_plan: false,
                rationale: "salvaged from prose vote: Claude should execute".into(),
                parse_source: VoteParseSource::CandidateMention,
            }],
            rationale: "codex nominated claude".into(),
            needs_plan: false,
            confident: true,
        };
        let prompt_stats = vec![(
            "codex (Critic)".into(),
            PromptStats {
                prompts: 1,
                chars_sent: 400,
                max_chars: 400,
            },
        )];

        let summary = compose_session_summary(
            &bb,
            Some(&recommendation),
            &RunOutcome::Discussed,
            None,
            None,
            None,
            1,
            &[],
            &[],
            &prompt_stats,
            &[],
        );

        assert_eq!(
            summary["recommendation"]["votes"][0]["parse_source"],
            "candidate_mention"
        );
        assert!(summary["optimization_hints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hint| hint.as_str().unwrap().contains("salvaged 1 prose vote")));
    }

    #[test]
    fn session_report_explains_missing_machine_cost() {
        let bb = bb_with_task("ship feature");

        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Rejected,
            None,
            None,
            None,
            0,
            &[],
            &[],
            &[],
            &[],
        );

        assert!(
            report.contains("no adapter reported machine-readable USD"),
            "{report}"
        );
    }

    #[test]
    fn session_report_includes_provider_token_usage_when_available() {
        let bb = bb_with_task("ship feature");

        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Discussed,
            None,
            None,
            None,
            0,
            &[],
            &[(
                "codex (Critic)".into(),
                TokenUsage {
                    input_tokens: Some(120),
                    cached_input_tokens: Some(40),
                    output_tokens: Some(30),
                    reasoning_output_tokens: Some(10),
                    total_tokens: Some(160),
                },
            )],
            &[],
            &[],
        );

        assert!(report.contains("## Reported token usage"), "{report}");
        assert!(
            report.contains("- reported_total: total=160, input=120, cached_input=40, output=30, reasoning_output=10"),
            "{report}"
        );
        assert!(
            report.contains("- codex (Critic): total=160, input=120, cached_input=40, output=30, reasoning_output=10"),
            "{report}"
        );
        assert!(
            report.contains(
                "provider_tokens: largest reported usage was codex (Critic) with 160 token(s)"
            ),
            "{report}"
        );
        assert!(
            report.contains("provider_cache: adapters reported 40 cached input token(s)"),
            "{report}"
        );
    }

    #[test]
    fn session_report_calibrates_provider_input_against_prompt_estimate() {
        let bb = bb_with_task("ship feature");

        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Discussed,
            Some(2_000),
            None,
            None,
            0,
            &[],
            &[(
                "codex (Critic)".into(),
                TokenUsage {
                    input_tokens: Some(600),
                    cached_input_tokens: None,
                    output_tokens: Some(60),
                    reasoning_output_tokens: None,
                    total_tokens: Some(660),
                },
            )],
            &[(
                "codex (Critic)".into(),
                PromptStats {
                    prompts: 2,
                    chars_sent: 1_000,
                    max_chars: 600,
                },
            )],
            &[],
        );

        assert!(
            report.contains(
                "- provider_input_tokens: 600 (local_prompt_estimate: ~250, ratio: 240%)"
            ),
            "{report}"
        );
        assert!(
            report.contains(
                "provider_input_calibration: provider input tokens were 240% of the local prompt estimate"
            ),
            "{report}"
        );
    }

    #[test]
    fn session_report_includes_prompt_telemetry() {
        let bb = bb_with_task("ship feature");
        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Rejected,
            Some(24_000),
            None,
            None,
            0,
            &[],
            &[],
            &[(
                "claude (Drafter)".into(),
                PromptStats {
                    prompts: 3,
                    chars_sent: 1200,
                    max_chars: 700,
                },
            )],
            &[(
                "planning".into(),
                PromptStats {
                    prompts: 2,
                    chars_sent: 900,
                    max_chars: 500,
                },
            )],
        );

        assert!(report.contains("## Prompt telemetry"), "{report}");
        assert!(report.contains("- prompts_sent: 3"), "{report}");
        assert!(report.contains("- prompt_chars_sent: 1200"), "{report}");
        assert!(
            report.contains("- prompt_tokens_estimate: ~300"),
            "{report}"
        );
        assert!(
            report.contains("- max_prompt_tokens_estimate: ~175"),
            "{report}"
        );
        assert!(
            report.contains("- context_budget_chars: 24000 (~6000 tokens)"),
            "{report}"
        );
        assert!(
            report.contains(
                "claude (Drafter): prompts=3 chars=1200 tokens~300 max=700 max_tokens~175"
            ),
            "{report}"
        );
        assert!(report.contains("## Prompt phase breakdown"), "{report}");
        assert!(
            report.contains("planning: prompts=2 chars=900 tokens~225 max=500 max_tokens~125"),
            "{report}"
        );
    }

    #[test]
    fn session_report_includes_compact_resume_packet() {
        let mut bb = bb_with_task("ship feature with a deliberately long description that should still fit in one compact resume line");
        bb.record(
            "you".into(),
            Role::Human,
            "prefer the cheaper executor and keep the diff small".into(),
        );
        bb.record(
            "claude".into(),
            Role::Drafter,
            "final plan: edit the report generator, add tests, run cargo test".into(),
        );
        let rec = Recommendation {
            executor: "codex".into(),
            scores: vec![("codex".into(), 0.9), ("claude".into(), 0.4)],
            votes: Vec::new(),
            rationale: "codex has the current local context and can apply the small patch".into(),
            needs_plan: false,
            confident: true,
        };
        let ctx = ProjectContextReport {
            cache_hit: false,
            cache_path: "/tmp/context.txt".into(),
            chars: 4096,
            max_files: 220,
            max_manifest_chars: 1200,
        };
        let changes = dirty_changes();

        let report = compose_session_report(
            &bb,
            Some(&rec),
            &RunOutcome::Executed {
                executor: "codex".into(),
                output: "patched report generator and tests passed".into(),
            },
            Some(24_000),
            Some(&ctx),
            Some(&changes),
            0,
            &[],
            &[],
            &[],
            &[],
        );

        assert!(report.contains("## Resume packet"), "{report}");
        assert!(report.contains("- status: executed"), "{report}");
        assert!(report.contains("- executor: codex"), "{report}");
        assert!(
            report.contains("- executor_output: patched report generator and tests passed"),
            "{report}"
        );
        assert!(
            report.contains("- recommendation: codex (confident: true)"),
            "{report}"
        );
        assert!(
            report.contains("latest_human_direction: you: prefer the cheaper executor"),
            "{report}"
        );
        assert!(
            report.contains("latest_planner_output: claude (Drafter): final plan"),
            "{report}"
        );
        assert!(
            report.contains(
                "- project_context: rebuilt (4096 chars, max_files=220, max_manifest_chars=1200)"
            ),
            "{report}"
        );
        assert!(
            report.contains("- local_changes: branch main; 2 visible change(s); 1 generated/dependency entry(ies) omitted"),
            "{report}"
        );
        assert!(report.contains("## Local changes"), "{report}");
        assert!(report.contains("?? landing/demo.mp4"), "{report}");
        assert!(
            report.contains("- next_step: review executor output and local diff"),
            "{report}"
        );
    }

    #[test]
    fn resume_packet_uses_outcome_specific_next_step() {
        let bb = bb_with_task("ship feature");

        let discussed = compose_session_report(
            &bb,
            None,
            &RunOutcome::Discussed,
            None,
            None,
            None,
            0,
            &[],
            &[],
            &[],
            &[],
        );
        assert!(
            discussed.contains("continue planning or run recommendation before execution"),
            "{discussed}"
        );

        let rejected = compose_session_report(
            &bb,
            None,
            &RunOutcome::Rejected,
            None,
            None,
            None,
            0,
            &[],
            &[],
            &[],
            &[],
        );
        assert!(
            rejected.contains("revise the plan with the user before any execution"),
            "{rejected}"
        );
    }

    #[test]
    fn session_report_includes_project_context_metadata() {
        let bb = bb_with_task("ship feature");
        let ctx = ProjectContextReport {
            cache_hit: true,
            cache_path: "/tmp/tales-context.txt".into(),
            chars: 2048,
            max_files: 180,
            max_manifest_chars: 900,
        };

        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Discussed,
            Some(12_000),
            Some(&ctx),
            None,
            0,
            &[],
            &[],
            &[],
            &[],
        );

        assert!(report.contains("## Project context"), "{report}");
        assert!(
            report.contains("- status: injected (cache hit)"),
            "{report}"
        );
        assert!(report.contains("/tmp/tales-context.txt"), "{report}");
        assert!(report.contains("- chars: 2048"), "{report}");
        assert!(
            report.contains("max_files=180, max_manifest_chars=900"),
            "{report}"
        );
    }

    #[test]
    fn session_report_includes_deterministic_optimization_hints() {
        let bb = bb_with_task("ship feature");
        let ctx = ProjectContextReport {
            cache_hit: true,
            cache_path: "/tmp/tales-context.txt".into(),
            chars: 2048,
            max_files: 180,
            max_manifest_chars: 900,
        };

        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Rejected,
            Some(1_000),
            Some(&ctx),
            None,
            0,
            &[],
            &[],
            &[(
                "claude (Drafter)".into(),
                PromptStats {
                    prompts: 3,
                    chars_sent: 1_200,
                    max_chars: 950,
                },
            )],
            &[
                (
                    "planning".into(),
                    PromptStats {
                        prompts: 2,
                        chars_sent: 900,
                        max_chars: 500,
                    },
                ),
                (
                    "vote_repair".into(),
                    PromptStats {
                        prompts: 1,
                        chars_sent: 300,
                        max_chars: 300,
                    },
                ),
            ],
        );

        assert!(report.contains("## Optimization hints"), "{report}");
        assert!(report.contains("project_context_cache: hit"), "{report}");
        assert!(
            report.contains(
                "largest_agent_prompt_spend: claude (Drafter) sent 1200 chars (~300 tokens) across 3 prompt(s)"
            ),
            "{report}"
        );
        assert!(
            report.contains("largest_phase_prompt_spend: planning sent 900 chars (~225 tokens)"),
            "{report}"
        );
        assert!(
            report.contains("largest prompt used at least 90% of the cap"),
            "{report}"
        );
        assert!(
            report.contains("vote repair cost 1 extra prompt(s) (~75 tokens)"),
            "{report}"
        );
    }

    #[test]
    fn optimization_hints_call_out_unlimited_budget_and_expensive_voting() {
        let bb = bb_with_task("ship feature");

        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Rejected,
            None,
            None,
            None,
            0,
            &[],
            &[],
            &[(
                "codex (Critic)".into(),
                PromptStats {
                    prompts: 2,
                    chars_sent: 900,
                    max_chars: 700,
                },
            )],
            &[(
                "recommendation_vote".into(),
                PromptStats {
                    prompts: 2,
                    chars_sent: 400,
                    max_chars: 250,
                },
            )],
        );

        assert!(report.contains("project_context_cache: off"), "{report}");
        assert!(report.contains("context_budget: unlimited"), "{report}");
        assert!(
            report.contains("recommendation_vote: voting was a large share"),
            "{report}"
        );
    }

    #[test]
    fn optimization_hints_call_out_salvaged_prose_votes() {
        let bb = bb_with_task("ship feature");

        let report = compose_session_report(
            &bb,
            None,
            &RunOutcome::Rejected,
            Some(24_000),
            None,
            None,
            2,
            &[],
            &[],
            &[(
                "claude (Drafter)".into(),
                PromptStats {
                    prompts: 1,
                    chars_sent: 400,
                    max_chars: 400,
                },
            )],
            &[(
                "recommendation_vote".into(),
                PromptStats {
                    prompts: 1,
                    chars_sent: 400,
                    max_chars: 400,
                },
            )],
        );

        assert!(report.contains("salvaged 2 prose vote(s)"), "{report}");
        assert!(
            report.contains("avoiding up to 2 repair prompt(s)"),
            "{report}"
        );
    }
}
