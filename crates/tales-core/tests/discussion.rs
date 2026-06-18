//! Deterministic test of the discussion relay using scripted mock agents —
//! no API calls. Proves drafter/critic turn-taking, transcript accumulation,
//! and that the conversation is bridged onto the event bus.

use std::path::PathBuf;

use tales_core::agent::mock::MockAdapter;
use tales_core::agent::SpawnCtx;
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::OrchestratorEvent;
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
async fn drafter_critic_relay_builds_transcript() {
    let (bus, _commands_rx) = EventBus::new(512, 64);
    let mut events = bus.subscribe();

    let mut orch = Orchestrator::new(bus.clone());

    let drafter = Uuid::new_v4();
    let critic = Uuid::new_v4();

    // Drafter speaks on turns 1 and 3; critic on turn 2.
    orch.add_agent(
        Box::new(MockAdapter::new(vec![
            "draft-one".to_string(),
            "revised-three".to_string(),
        ])),
        ctx(drafter, "claude"),
        Role::Drafter,
    )
    .await
    .unwrap();

    orch.add_agent(
        Box::new(MockAdapter::new(vec!["critique-two".to_string()])),
        ctx(critic, "codex"),
        Role::Critic,
    )
    .await
    .unwrap();

    let transcript = orch.run_discussion("build a widget", 3).await.unwrap();

    // All three turns landed, in order, on the blackboard.
    assert!(transcript.contains("draft-one"), "transcript: {transcript}");
    assert!(
        transcript.contains("critique-two"),
        "transcript: {transcript}"
    );
    assert!(
        transcript.contains("revised-three"),
        "transcript: {transcript}"
    );

    let bb = orch.blackboard();
    assert_eq!(bb.transcript.len(), 3);
    assert_eq!(bb.transcript[0].role, Role::Drafter);
    assert_eq!(bb.transcript[1].role, Role::Critic);
    assert_eq!(bb.transcript[2].role, Role::Drafter);

    orch.shutdown().await;

    // The conversation was mirrored onto the bus: collect the Message events.
    let mut messages = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let OrchestratorEvent::Message { text, .. } = event {
            messages.push(text);
        }
    }
    assert!(messages.iter().any(|m| m == "draft-one"));
    assert!(messages.iter().any(|m| m == "critique-two"));
    assert!(messages.iter().any(|m| m == "revised-three"));
}
