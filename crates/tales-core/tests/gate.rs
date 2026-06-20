//! Tests for the recommendation stage and the execution gate. The critical
//! invariant: the engine cannot enter `Executing` except via
//! `confirm_execution` from `AwaitingConfirmation`.

use std::path::PathBuf;

use tales_core::agent::mock::MockAdapter;
use tales_core::agent::SpawnCtx;
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::OrchestratorEvent;
use tales_core::orchestrator::{Orchestrator, Phase};
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

async fn two_agent_orch(
    drafter_responses: Vec<String>,
    critic_responses: Vec<String>,
) -> Orchestrator {
    let (bus, _rx) = EventBus::new(512, 64);
    let mut orch = Orchestrator::new(bus);
    orch.add_agent(
        Box::new(MockAdapter::new(drafter_responses)),
        ctx(Uuid::new_v4(), "claude"),
        Role::Drafter,
    )
    .await
    .unwrap();
    orch.add_agent(
        Box::new(MockAdapter::new(critic_responses)),
        ctx(Uuid::new_v4(), "codex"),
        Role::Critic,
    )
    .await
    .unwrap();
    orch
}

#[tokio::test]
async fn recommendation_then_confirm_reaches_executing() {
    let vote = r#"{"recommended_executor":"claude","confidence":0.9,"rationale":"best at code"}"#;
    let vote2 = r#"{"recommended_executor":"claude","confidence":0.7,"rationale":"agreed"}"#;
    let mut orch = two_agent_orch(
        vec!["draft".into(), vote.into()],
        vec!["critique".into(), vote2.into()],
    )
    .await;

    orch.run_discussion("build a thing", 2).await.unwrap();
    let rec = orch.run_recommendation().await.unwrap();

    assert_eq!(rec.executor, "claude");
    assert_eq!(orch.phase(), Phase::AwaitingConfirmation);

    let chosen = orch.confirm_execution("claude").unwrap();
    assert_eq!(chosen, "claude");
    assert_eq!(orch.phase(), Phase::Executing);

    orch.shutdown().await;
}

#[tokio::test]
async fn tied_recommendation_is_not_confident_and_gate_says_choose() {
    let (bus, _rx) = EventBus::new(512, 64);
    let mut events = bus.subscribe();
    let mut orch = Orchestrator::new(bus);
    let claude_vote =
        r#"{"recommended_executor":"claude","confidence":0.7,"rationale":"local context"}"#;
    let codex_vote =
        r#"{"recommended_executor":"codex","confidence":0.7,"rationale":"cheaper execution"}"#;

    orch.add_agent(
        Box::new(MockAdapter::new(vec!["draft".into(), claude_vote.into()])),
        ctx(Uuid::new_v4(), "claude"),
        Role::Drafter,
    )
    .await
    .unwrap();
    orch.add_agent(
        Box::new(MockAdapter::new(vec!["critique".into(), codex_vote.into()])),
        ctx(Uuid::new_v4(), "codex"),
        Role::Critic,
    )
    .await
    .unwrap();

    orch.run_discussion("build a thing", 2).await.unwrap();
    let rec = orch.run_recommendation().await.unwrap();

    assert_eq!(rec.executor, "claude");
    assert!(!rec.confident);
    assert!(
        rec.rationale.contains("tied executor vote"),
        "{}",
        rec.rationale
    );
    assert_eq!(orch.phase(), Phase::AwaitingConfirmation);

    let mut saw_choose_prompt = false;
    let mut saw_structured_recommendation = false;
    while let Ok(event) = events.try_recv() {
        match event {
            OrchestratorEvent::AwaitingConfirmation { prompt }
                if prompt.contains("No clear executor consensus")
                    && prompt.contains("Preselected executor: claude") =>
            {
                saw_choose_prompt = true;
            }
            OrchestratorEvent::RecommendationReady {
                executor,
                confident,
                scores,
                ..
            } => {
                assert_eq!(executor, "claude");
                assert!(!confident);
                assert_eq!(scores[0], ("claude".to_string(), 0.7));
                assert_eq!(scores[1], ("codex".to_string(), 0.7));
                saw_structured_recommendation = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_choose_prompt,
        "expected not-confident gate prompt on bus"
    );
    assert!(
        saw_structured_recommendation,
        "expected structured recommendation metadata on bus"
    );

    orch.shutdown().await;
}

#[tokio::test]
async fn cannot_execute_before_recommendation() {
    let mut orch = two_agent_orch(vec!["draft".into()], vec!["critique".into()]).await;
    orch.run_discussion("x", 2).await.unwrap();

    // Still in Planning — the gate must refuse.
    assert_eq!(orch.phase(), Phase::Planning);
    assert!(orch.confirm_execution("claude").is_err());
    assert_ne!(orch.phase(), Phase::Executing);

    orch.shutdown().await;
}

#[tokio::test]
async fn confirm_rejects_unknown_executor() {
    let vote = r#"{"recommended_executor":"claude","confidence":0.8,"rationale":"x"}"#;
    let mut orch = two_agent_orch(
        vec!["draft".into(), vote.into()],
        vec!["critique".into(), vote.into()],
    )
    .await;
    orch.run_discussion("x", 2).await.unwrap();
    orch.run_recommendation().await.unwrap();

    assert!(orch.confirm_execution("nonexistent").is_err());
    // A failed confirm must not advance the phase.
    assert_eq!(orch.phase(), Phase::AwaitingConfirmation);

    orch.shutdown().await;
}

#[tokio::test]
async fn reject_ends_run_without_executing() {
    let vote = r#"{"recommended_executor":"codex","confidence":0.6,"rationale":"x"}"#;
    let mut orch = two_agent_orch(
        vec!["draft".into(), vote.into()],
        vec!["critique".into(), vote.into()],
    )
    .await;
    orch.run_discussion("x", 2).await.unwrap();
    orch.run_recommendation().await.unwrap();

    orch.reject().unwrap();
    assert_eq!(orch.phase(), Phase::Done);

    orch.shutdown().await;
}
