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
use std::time::Duration;

use tokio::sync::mpsc;

use crate::agent::{AgentAdapter, AgentCommand, AgentEvent, SpawnCtx};
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
        }
    }

    /// Override the per-turn timeout (default 300s).
    pub fn set_turn_timeout(&mut self, timeout: Duration) {
        self.turn_timeout = timeout;
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
            return Err(TalesError::Other("agent label must be non-empty".to_string()));
        }
        let agent = ctx.agent;
        let label = ctx.label.clone();
        let cmd_tx = adapter.spawn(ctx, self.events_tx.clone()).await?;
        self.cmd_txs.insert(agent, cmd_tx);
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
        let mut conductor = RuleConductor::new(self.roster.clone(), max_turns);

        while let Some(plan) = conductor.next_turn(&self.blackboard) {
            let prompt = compose_prompt(plan.role, task, &self.blackboard);
            self.bus.emit(OrchestratorEvent::Log {
                level: "info".to_string(),
                msg: format!("→ {} speaking as {:?}", plan.label, plan.role),
            });

            let tx = self
                .cmd_txs
                .get(&plan.agent)
                .ok_or_else(|| TalesError::Other(format!("no command channel for {}", plan.agent)))?;
            tx.send(AgentCommand::StartTurn { prompt })
                .await
                .map_err(|e| TalesError::Other(format!("send to agent failed: {e}")))?;

            let text = self.collect_turn(plan.agent).await?;
            self.blackboard.record(plan.label.clone(), plan.role, text);
        }

        Ok(self.blackboard.transcript_text())
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
                    // emit stray output into a later turn.
                    if let Some(tx) = self.cmd_txs.get(&agent) {
                        let _ = tx.send(AgentCommand::Shutdown).await;
                    }
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
                        return Ok(final_text);
                    }
                }
                AgentEvent::Exited { agent: a, code } if a == agent => {
                    self.bus.emit(OrchestratorEvent::AgentExited { agent, code });
                    return Ok(final_text);
                }
                // SessionReady, TurnStarted, and any event from another agent.
                _ => {}
            }
        }

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
            let prompt = compose_vote_prompt(&self.blackboard.task, &candidates);
            let tx = self
                .cmd_txs
                .get(&entry.agent)
                .ok_or_else(|| TalesError::Other(format!("no command channel for {}", entry.agent)))?;
            tx.send(AgentCommand::StartTurn { prompt })
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
        let roster = self.roster.clone();

        let mut turn_idx = 0;
        while turn_idx < max_turns {
            // Fold any pending human interjections into the conversation first.
            if self.drain_user_notes(commands) {
                return Ok(RunOutcome::Aborted);
            }
            let entry = &roster[turn_idx % roster.len()];
            let prompt = compose_prompt(entry.role, task, &self.blackboard);
            self.bus.emit(OrchestratorEvent::TurnStarted {
                agent: entry.agent,
                role: format!("{:?}", entry.role),
            });
            self.bus.emit(OrchestratorEvent::Log {
                level: "info".to_string(),
                msg: format!("{} speaking as {:?}", entry.label, entry.role),
            });
            let tx = self
                .cmd_txs
                .get(&entry.agent)
                .ok_or_else(|| TalesError::Other(format!("no channel for {}", entry.agent)))?;
            tx.send(AgentCommand::StartTurn { prompt })
                .await
                .map_err(|e| TalesError::Other(format!("send failed: {e}")))?;
            let text = self.collect_turn(entry.agent).await?;
            self.blackboard.record(entry.label.clone(), entry.role, text);
            turn_idx += 1;
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
                UserCommand::InjectNote { text, .. } => self.record_human(text),
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
                Ok(UserCommand::InjectNote { text, .. }) => self.record_human(text),
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

    /// Record a human interjection so agents see it in the transcript, and echo
    /// it onto the bus for the chat view.
    fn record_human(&mut self, text: String) {
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
            .ok_or_else(|| TalesError::Other(format!("executor '{executor_label}' not in roster")))?;
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
        let tx = self
            .cmd_txs
            .get(&entry.agent)
            .ok_or_else(|| TalesError::Other(format!("no channel for {}", entry.agent)))?;
        tx.send(AgentCommand::StartTurn { prompt })
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

/// Build the prompt for a turn from its role and the discussion so far.
fn compose_prompt(role: Role, task: &str, bb: &Blackboard) -> String {
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
        // Humans are recorded out-of-band, never scheduled to "speak" a prompt.
        Role::Human => task.to_string(),
    }
}
