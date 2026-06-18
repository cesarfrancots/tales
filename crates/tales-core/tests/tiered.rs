//! Tiered execution: a separate executor agent plans nothing and votes on
//! nothing, but is a valid recommendation candidate and can be confirmed — the
//! "strong models plan, a cheap model executes" path. Mock agents, no API.

use std::path::PathBuf;

use tales_core::agent::mock::MockAdapter;
use tales_core::agent::SpawnCtx;
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::orchestrator::Orchestrator;
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
async fn executor_sits_out_planning_but_can_be_recommended() {
    let (bus, _commands_rx) = EventBus::new(512, 64);
    let mut orch = Orchestrator::new(bus.clone());

    let vote = r#"{"recommended_executor":"cheapo","confidence":0.9,"rationale":"cheap & fast"}"#;

    // Drafter speaks turns 1 & 3, then votes. Critic speaks turn 2, then votes.
    orch.add_agent(
        Box::new(MockAdapter::new(vec![
            "draft-1".into(),
            "draft-3".into(),
            vote.into(),
        ])),
        ctx(Uuid::new_v4(), "claude"),
        Role::Drafter,
    )
    .await
    .unwrap();
    orch.add_agent(
        Box::new(MockAdapter::new(vec!["critique-2".into(), vote.into()])),
        ctx(Uuid::new_v4(), "codex"),
        Role::Critic,
    )
    .await
    .unwrap();
    // The executor's only canned response would be obvious if it were ever
    // scheduled to speak or vote — it must never be consumed.
    orch.add_agent(
        Box::new(MockAdapter::new(vec!["EXECUTOR-SHOULD-NOT-SPEAK".into()])),
        ctx(Uuid::new_v4(), "cheapo"),
        Role::Executor,
    )
    .await
    .unwrap();

    // Three planning turns: with the executor filtered out, only the two
    // planners alternate (draft-1, critique-2, draft-3) — never the executor.
    let transcript = orch.run_discussion("build a thing", 3).await.unwrap();
    assert!(transcript.contains("draft-1"), "{transcript}");
    assert!(transcript.contains("critique-2"), "{transcript}");
    assert!(transcript.contains("draft-3"), "{transcript}");
    assert!(
        !transcript.contains("EXECUTOR-SHOULD-NOT-SPEAK"),
        "executor was scheduled to speak: {transcript}"
    );

    let bb = orch.blackboard();
    assert_eq!(bb.transcript.len(), 3);
    assert!(bb.transcript.iter().all(|t| t.role != Role::Executor));

    // Only planners vote, but the executor is a valid candidate and wins.
    let rec = orch.run_recommendation().await.unwrap();
    assert_eq!(rec.executor, "cheapo");

    // The gate accepts the separate executor by label.
    let chosen = orch.confirm_execution("cheapo").unwrap();
    assert_eq!(chosen, "cheapo");

    orch.shutdown().await;
}
