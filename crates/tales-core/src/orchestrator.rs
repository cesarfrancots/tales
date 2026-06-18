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

use tokio::sync::mpsc;

use crate::agent::{AgentAdapter, AgentCommand, AgentEvent, Attachment, SpawnCtx};
use crate::blackboard::Blackboard;
use crate::bus::EventBus;
use crate::conductor::{Conductor, Role, RosterEntry, RuleConductor};
use crate::event::{OrchestratorEvent, UserCommand};
use crate::recommend::{aggregate, parse_vote, ExecutionVote, Recommendation};
use crate::{AgentId, Result, TalesError};

/// How an interactive run ended.
#[derive(Clone, Debug)]
pub enum RunOutcome {
    /// The user confirmed; the executor ran and produced this output.
    Executed { executor: String, output: String },
    /// The user rejected the plan.
    Rejected,
    /// The run was shut down / the command channel closed.
    Aborted,
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

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.bus.emit(OrchestratorEvent::PhaseChanged {
            phase: format!("{phase:?}").to_lowercase(),
        });
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
            let prompt = compose_prompt(plan.role, task, &self.blackboard, resumable, &delta, first_time);
            self.bus.emit(OrchestratorEvent::Log {
                level: "info".to_string(),
                msg: format!("→ {} speaking as {:?}", plan.label, plan.role),
            });

            let tx = self
                .cmd_txs
                .get(&plan.agent)
                .ok_or_else(|| {
                    TalesError::Other(format!("no command channel for {}", plan.agent))
                })?
                .clone();
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
                compose_round1_prompt(task)
            } else {
                let others: String = active
                    .iter()
                    .filter(|o| o.agent != entry.agent)
                    .filter_map(|o| prev.get(&o.agent))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");
                let own = prev.get(&entry.agent).cloned().unwrap_or_default();
                compose_round_synth_prompt(task, &others, Some(entry.agent) == synth, resumable, &own)
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
            self.blackboard.record(entry.label.clone(), entry.role, text);
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
                        self.bus
                            .emit(OrchestratorEvent::TurnComplete { agent: a, cost_usd: None });
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
                    self.bus
                        .emit(OrchestratorEvent::ToolActivity { agent, summary: name });
                }
                AgentEvent::TurnComplete {
                    agent, cost_usd, ..
                } if pending.contains(&aid) => {
                    self.bus
                        .emit(OrchestratorEvent::TurnComplete { agent, cost_usd });
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
                        self.bus
                            .emit(OrchestratorEvent::TurnComplete { agent, cost_usd: None });
                        pending.remove(&agent);
                    }
                }
                AgentEvent::Exited { agent, code } if pending.contains(&aid) => {
                    self.bus
                        .emit(OrchestratorEvent::AgentExited { agent, code });
                    self.bus
                        .emit(OrchestratorEvent::TurnComplete { agent, cost_usd: None });
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
                    self.bus.emit(OrchestratorEvent::TurnComplete {
                        agent,
                        cost_usd: None,
                    });
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
                    agent: a, cost_usd, ..
                } if a == agent => {
                    self.bus
                        .emit(OrchestratorEvent::TurnComplete { agent, cost_usd });
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
                        self.bus.emit(OrchestratorEvent::TurnComplete {
                            agent,
                            cost_usd: None,
                        });
                        return Ok(final_text);
                    }
                }
                AgentEvent::Exited { agent: a, code } if a == agent => {
                    self.bus
                        .emit(OrchestratorEvent::AgentExited { agent, code });
                    self.bus.emit(OrchestratorEvent::TurnComplete {
                        agent,
                        cost_usd: None,
                    });
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
        self.bus.emit(OrchestratorEvent::TurnComplete {
            agent,
            cost_usd: None,
        });
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
            let prompt = compose_vote_prompt(&self.blackboard.task, &candidates);
            let tx = self.cmd_txs.get(&entry.agent).ok_or_else(|| {
                TalesError::Other(format!("no command channel for {}", entry.agent))
            })?;
            tx.send(AgentCommand::StartTurn {
                prompt,
                attachments: Vec::new(),
            })
            .await
            .map_err(|e| TalesError::Other(format!("send to agent failed: {e}")))?;
            let text = self.collect_turn(entry.agent).await?;

            match parse_vote(&text) {
                Some((executor, confidence, rationale)) => votes.push(ExecutionVote {
                    voter: entry.agent,
                    voter_label: entry.label.clone(),
                    recommended_executor: executor,
                    confidence,
                    rationale,
                }),
                None => self.bus.emit(OrchestratorEvent::Log {
                    level: "warn".to_string(),
                    msg: format!("{} returned no parseable vote", entry.label),
                }),
            }
        }

        let recommendation = aggregate(votes, &candidates)
            .ok_or_else(|| TalesError::Other("no candidates to recommend".to_string()))?;

        self.bus.emit(OrchestratorEvent::RecommendationReady {
            executor: recommendation.executor.clone(),
            rationale: recommendation.rationale.clone(),
        });
        self.set_phase(Phase::AwaitingConfirmation);
        self.bus.emit(OrchestratorEvent::AwaitingConfirmation {
            prompt: format!(
                "Recommended executor: {}. Confirm, override, or reject.",
                recommendation.executor
            ),
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
                    return Ok(RunOutcome::Aborted);
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
                return Ok(RunOutcome::Aborted);
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
            let prompt =
                compose_prompt(entry.role, task, &self.blackboard, resumable, &delta, first_time);
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
                    None => return Ok(RunOutcome::Aborted),
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
                    return Ok(RunOutcome::Executed {
                        executor: chosen,
                        output,
                    });
                }
                UserCommand::Reject => {
                    self.reject()?;
                    return Ok(RunOutcome::Rejected);
                }
                UserCommand::InjectNote {
                    text, attachments, ..
                } => self.record_human(text, attachments),
                UserCommand::StartTurn { .. } => {}
                UserCommand::Shutdown => return Ok(RunOutcome::Aborted),
            }
        }
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

    /// Have the chosen executor implement the agreed plan, streamed like any
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
        let plan = self.blackboard.transcript_text();
        let prompt = format!(
            "You are now EXECUTING the plan the team agreed on.\n\
             Task: {task}\n\n\
             Discussion and plan:\n{plan}\n\
             Implement it now. Use your file-writing tool (Write/Edit) to create \
             every file — it creates parent directories automatically, so do NOT \
             use shell commands like mkdir. Create all the files the plan calls \
             for in this turn. When finished, briefly summarize what you wrote."
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
            .ok_or_else(|| TalesError::Other(format!("no channel for {}", entry.agent)))?;
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

/// Prompt asking an agent to nominate an executor as a JSON object.
fn compose_vote_prompt(task: &str, candidates: &[String]) -> String {
    format!(
        "Based on our discussion of the task:\n{task}\n\n\
         Which agent should EXECUTE the plan? Candidates: {}.\n\
         Reply with ONLY a JSON object, no prose and no code fence:\n\
         {{\"recommended_executor\": \"<one of: {}>\", \
         \"confidence\": <number 0.0-1.0>, \"rationale\": \"<one short sentence>\"}}",
        candidates.join(", "),
        candidates.join(" | ")
    )
}

/// Build the prompt for a SEQUENTIAL planning turn. A *resumable* adapter keeps
/// its own prior turns in its session, so it is sent only the role intro (the
/// first time it speaks) plus the unseen `delta` — never the whole transcript
/// re-pasted. A stateless adapter has no cross-turn memory, so it falls back to
/// the full-context prompt below. `delta` is the transcript tail the agent
/// hasn't been shown yet; `first_time` is whether it has spoken before.
fn compose_prompt(
    role: Role,
    task: &str,
    bb: &Blackboard,
    resumable: bool,
    delta: &str,
    first_time: bool,
) -> String {
    if !resumable {
        return compose_prompt_full(role, task, bb);
    }
    let delta = delta.trim();
    match role {
        Role::Drafter if first_time => format!(
            "You are the DRAFTER collaborating with a critic.\n\
             Task: {task}\n\n\
             Write a first, concise draft of the plan/solution. \
             Keep it tight — bullet points are fine."
        ),
        Role::Drafter => format!(
            "The critic responded:\n{delta}\n\n\
             Revise your draft to address these points. \
             Be concise; show only the updated draft."
        ),
        Role::Critic if first_time => format!(
            "You are the CRITIC reviewing a draft.\n\
             Task: {task}\n\n\
             Latest draft:\n{delta}\n\n\
             List the concrete problems and the clarifying questions you'd \
             ask. Be specific and brief — no preamble."
        ),
        Role::Critic => format!(
            "The drafter revised:\n{delta}\n\n\
             Critique the update — list the remaining problems and gaps. \
             Be specific and brief."
        ),
        Role::Human | Role::Executor => task.to_string(),
    }
}

/// The full-context prompt: re-sends the whole discussion and restates the role
/// every turn. Used for stateless adapters (no session to resume), and it is the
/// exact behavior the engine had before delta context — so the sequential path
/// with a stateless adapter is byte-for-byte unchanged.
fn compose_prompt_full(role: Role, task: &str, bb: &Blackboard) -> String {
    match role {
        Role::Drafter => {
            if bb.transcript.is_empty() {
                format!(
                    "You are the DRAFTER collaborating with a critic.\n\
                     Task: {task}\n\n\
                     Write a first, concise draft of the plan/solution. \
                     Keep it tight — bullet points are fine."
                )
            } else {
                format!(
                    "You are the DRAFTER. Task: {task}\n\n\
                     Discussion so far:\n{}\n\
                     Revise your draft to address the critic's points. \
                     Be concise; show only the updated draft.",
                    bb.transcript_text()
                )
            }
        }
        Role::Critic => {
            let latest = bb.last_text().unwrap_or("(no draft yet)");
            format!(
                "You are the CRITIC reviewing a draft.\n\
                 Task: {task}\n\n\
                 Latest draft:\n{latest}\n\n\
                 List the concrete problems and the clarifying questions you'd \
                 ask. Be specific and brief — no preamble."
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
fn compose_round1_prompt(task: &str) -> String {
    format!(
        "You are one of two expert planners working the SAME task in parallel.\n\
         Task: {task}\n\n\
         Produce your own concise, COMPLETE plan/solution — specific enough to \
         implement directly. You'll be shown the other planner's proposal next to \
         compare against. Bullet points are fine; no preamble."
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
) -> String {
    let context = if resumable || own.trim().is_empty() {
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
             proposal, then produce the SINGLE merged plan — resolve every conflict \
             and keep the strongest ideas from both. Output only the final merged plan."
        )
    } else {
        format!(
            "Task: {task}\n\n{context}\n\n\
             Adversarially review the competing proposal: list its concrete \
             problems, risks, and missing edge cases an implementer must handle. \
             Be specific and brief."
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

    #[test]
    fn resumable_revision_sends_only_the_delta_not_role_or_own_history() {
        // A resumable drafter on a later turn: prompt carries the critic's delta,
        // NOT the "You are the DRAFTER" intro and NOT its own earlier draft.
        let bb = bb_with(&[
            ("claude", Role::Drafter, "MY-OWN-DRAFT"),
            ("codex", Role::Critic, "CRITIC-DELTA"),
        ]);
        let p = compose_prompt(Role::Drafter, "task", &bb, true, "CRITIC-DELTA", false);
        assert!(p.contains("CRITIC-DELTA"), "delta missing: {p}");
        assert!(!p.contains("You are the DRAFTER"), "re-sent role intro: {p}");
        assert!(!p.contains("MY-OWN-DRAFT"), "re-pasted own history: {p}");
    }

    #[test]
    fn resumable_first_turn_introduces_role() {
        let bb = Blackboard::default();
        let p = compose_prompt(Role::Critic, "task", &bb, true, "THE-DRAFT", true);
        assert!(p.contains("You are the CRITIC"), "{p}");
        assert!(p.contains("THE-DRAFT"), "{p}");
    }

    #[test]
    fn stateless_adapter_keeps_full_context_behavior() {
        // resumable=false → role restated and full transcript re-sent (unchanged
        // pre-delta behavior), so existing stateless flows are byte-compatible.
        let bb = bb_with(&[("claude", Role::Drafter, "EARLIER-DRAFT")]);
        let p = compose_prompt(Role::Drafter, "task", &bb, false, "ignored", false);
        assert!(p.contains("You are the DRAFTER"), "{p}");
        assert!(p.contains("EARLIER-DRAFT"), "full transcript expected: {p}");
    }

    #[test]
    fn round2_synth_merges_and_review_critiques() {
        let synth = compose_round_synth_prompt("task", "OTHER-PLAN", true, true, "OWN");
        assert!(synth.contains("OTHER-PLAN"));
        assert!(synth.to_lowercase().contains("merged"));
        // resumable synth doesn't re-paste its own proposal.
        assert!(!synth.contains("OWN"), "{synth}");

        let review = compose_round_synth_prompt("task", "OTHER-PLAN", false, true, "OWN");
        assert!(review.to_lowercase().contains("review"));
        assert!(review.contains("OTHER-PLAN"));
    }

    #[test]
    fn stateless_round2_includes_own_proposal() {
        // A stateless planner can't recall its round-1 output, so it must be
        // re-sent alongside the competitor's.
        let p = compose_round_synth_prompt("task", "OTHER-PLAN", true, false, "MY-PLAN");
        assert!(p.contains("MY-PLAN"), "{p}");
        assert!(p.contains("OTHER-PLAN"), "{p}");
    }
}
