//! Parallel-rounds planning: planners speak concurrently, demuxed off the
//! single shared event stream, recorded in deterministic roster order, and
//! bounded by a per-round deadline so a stuck planner can't hang the run.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tales_core::agent::mock::MockAdapter;
use tales_core::agent::{AgentAdapter, AgentCaps, AgentCommand, AgentEvent, SpawnCtx};
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::UserCommand;
use tales_core::orchestrator::{Orchestrator, RunOutcome};
use tokio::sync::mpsc;
use uuid::Uuid;

fn ctx(agent: Uuid, label: &str) -> SpawnCtx {
    SpawnCtx {
        agent,
        label: label.to_string(),
        cwd: PathBuf::from("."),
        model: None,
        effort: None,
        permission_mode: "default".to_string(),
        sandbox: "read-only".to_string(),
        allowed_tools: None,
    }
}

#[tokio::test]
async fn parallel_rounds_collect_both_planners_in_roster_order() {
    let (bus, _commands_rx) = EventBus::new(512, 64);
    let mut orch = Orchestrator::new(bus.clone());
    orch.set_parallel_rounds(true);

    let drafter = Uuid::new_v4();
    let critic = Uuid::new_v4();

    // Two rounds (max_turns 4 / 2 planners): round 1 independent drafts, round 2
    // synthesize + cross-review.
    orch.add_agent(
        Box::new(MockAdapter::new(vec![
            "draft-A".to_string(),
            "merged-plan".to_string(),
        ])),
        ctx(drafter, "claude"),
        Role::Drafter,
    )
    .await
    .unwrap();
    orch.add_agent(
        Box::new(MockAdapter::new(vec![
            "draft-B".to_string(),
            "review-notes".to_string(),
        ])),
        ctx(critic, "codex"),
        Role::Critic,
    )
    .await
    .unwrap();

    let transcript = orch.run_discussion("build a widget", 4).await.unwrap();

    // Both planners' output from BOTH rounds landed — none dropped on the floor.
    for needle in ["draft-A", "draft-B", "merged-plan", "review-notes"] {
        assert!(
            transcript.contains(needle),
            "missing {needle} in: {transcript}"
        );
    }

    let bb = orch.blackboard();
    assert_eq!(bb.transcript.len(), 4, "expected 2 rounds x 2 planners");
    // Recorded in deterministic ROSTER order regardless of completion race.
    assert_eq!(bb.transcript[0].role, Role::Drafter);
    assert_eq!(bb.transcript[1].role, Role::Critic);
    assert_eq!(bb.transcript[2].role, Role::Drafter);
    assert_eq!(bb.transcript[3].role, Role::Critic);
    // The roster-first planner synthesizes the merged plan in round 2.
    assert_eq!(bb.transcript[2].text, "merged-plan");

    orch.shutdown().await;
}

/// An adapter that ACKs a turn (TurnStarted) but never finishes it — to prove a
/// stuck planner is bounded by the round deadline and can't hang the run.
struct HangAdapter;

#[async_trait]
impl AgentAdapter for HangAdapter {
    fn caps(&self) -> AgentCaps {
        AgentCaps {
            midturn_injection: false,
            structured_output: false,
            resumable: false,
            mcp: false,
        }
    }
    async fn spawn(
        &mut self,
        ctx: SpawnCtx,
        events_tx: mpsc::Sender<AgentEvent>,
    ) -> tales_core::Result<mpsc::Sender<AgentCommand>> {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<AgentCommand>(64);
        let agent = ctx.agent;
        tokio::spawn(async move {
            let mut turn = 0u64;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    AgentCommand::StartTurn { .. } | AgentCommand::InjectMessage { .. } => {
                        turn += 1;
                        // Start the turn, then go silent forever (no TurnComplete).
                        let _ = events_tx
                            .send(AgentEvent::TurnStarted { agent, turn })
                            .await;
                    }
                    AgentCommand::Shutdown => break,
                }
            }
        });
        Ok(cmd_tx)
    }
}

#[tokio::test]
async fn parallel_round_bounds_a_stuck_planner_by_the_deadline() {
    let (bus, _commands_rx) = EventBus::new(512, 64);
    let mut orch = Orchestrator::new(bus.clone());
    orch.set_parallel_rounds(true);
    orch.set_turn_timeout(Duration::from_millis(250));

    let drafter = Uuid::new_v4();
    let critic = Uuid::new_v4();

    // Drafter is healthy; critic hangs forever.
    orch.add_agent(
        Box::new(MockAdapter::new(vec![
            "draft-A".to_string(),
            "merged-plan".to_string(),
        ])),
        ctx(drafter, "claude"),
        Role::Drafter,
    )
    .await
    .unwrap();
    orch.add_agent(Box::new(HangAdapter), ctx(critic, "codex"), Role::Critic)
        .await
        .unwrap();

    // Must return promptly (a couple of round deadlines), not hang.
    let res = tokio::time::timeout(Duration::from_secs(5), orch.run_discussion("t", 4)).await;
    let transcript = res.expect("run_discussion hung past the deadline").unwrap();

    // Healthy planner produced output; the stuck one was recorded as no-output.
    assert!(transcript.contains("draft-A"), "transcript: {transcript}");
    assert!(
        transcript.contains("(no output this turn)"),
        "stuck planner should be recorded as no-output: {transcript}"
    );

    orch.shutdown().await;
}

#[tokio::test]
async fn interactive_parallel_run_plans_in_rounds_then_executes() {
    // The benchmark path (`tales run` → run_interactive) under parallel rounds:
    // 2 rounds × 2 planners, then a recommendation vote, gate confirm, execution.
    let (bus, mut commands_rx) = EventBus::new(1024, 64);
    let commands_tx = bus.commands();

    let vote = r#"{"recommended_executor":"claude","confidence":0.9,"rationale":"x"}"#;
    // round1, round2, vote, execution-summary
    let drafter = MockAdapter::new(vec![
        "draft-A".into(),
        "merged-plan".into(),
        vote.into(),
        "executed: wrote files".into(),
    ]);
    // round1, round2, vote
    let critic = MockAdapter::new(vec!["draft-B".into(), "review".into(), vote.into()]);

    let mut orch = Orchestrator::new(bus.clone());
    orch.set_parallel_rounds(true);
    orch.add_agent(
        Box::new(drafter),
        ctx(Uuid::new_v4(), "claude"),
        Role::Drafter,
    )
    .await
    .unwrap();
    orch.add_agent(Box::new(critic), ctx(Uuid::new_v4(), "codex"), Role::Critic)
        .await
        .unwrap();

    commands_tx
        .send(UserCommand::ConfirmExecution {
            executor: "claude".into(),
        })
        .await
        .unwrap();

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        orch.run_interactive("build a service", 2, &mut commands_rx),
    )
    .await
    .expect("run_interactive should not hang")
    .unwrap();

    match outcome {
        RunOutcome::Executed { executor, output } => {
            assert_eq!(executor, "claude");
            assert!(output.contains("executed"), "output: {output}");
        }
        other => panic!("expected Executed, got {other:?}"),
    }

    // Both rounds' planning landed before the gate.
    let transcript = orch.blackboard().transcript_text();
    for needle in ["draft-A", "draft-B", "merged-plan", "review"] {
        assert!(
            transcript.contains(needle),
            "missing {needle}: {transcript}"
        );
    }
    orch.shutdown().await;
}
