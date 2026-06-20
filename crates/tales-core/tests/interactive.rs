//! Tests the human-in-the-loop interactive run: a user note is folded into the
//! discussion, the gate blocks until the user confirms, and confirmation drives
//! an execution turn — all without real API calls.

use std::path::PathBuf;

use tales_core::agent::mock::MockAdapter;
use tales_core::agent::SpawnCtx;
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::{OrchestratorEvent, UserCommand};
use tales_core::orchestrator::{Orchestrator, RunOutcome};
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
async fn interactive_run_folds_human_note_then_executes_on_confirm() {
    let (bus, mut commands_rx) = EventBus::new(1024, 64);
    let commands_tx = bus.commands();
    let mut events = bus.subscribe();

    // Drafter responds to: draft, (vote), execution-summary.
    let vote = r#"{"recommended_executor":"claude","confidence":0.9,"rationale":"x"}"#;
    let drafter = MockAdapter::new(vec![
        "initial draft".into(),
        vote.into(),
        "executed: wrote files".into(),
    ]);
    let critic = MockAdapter::new(vec!["a critique".into(), vote.into()]);

    let mut orch = Orchestrator::new(bus.clone());
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

    // Pre-queue a human interjection and the gate decision. They sit in the
    // channel; the orchestrator drains the note between turns and blocks on the
    // confirm at the gate.
    commands_tx
        .send(UserCommand::InjectNote {
            agent: Uuid::nil(),
            text: "please consider rate limits".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    commands_tx
        .send(UserCommand::ConfirmExecution {
            executor: "claude".into(),
        })
        .await
        .unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
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

    // The human's note must have been recorded in the transcript so agents saw it.
    let transcript = orch.blackboard().transcript_text();
    assert!(
        transcript.contains("rate limits"),
        "transcript: {transcript}"
    );
    assert!(
        transcript.contains("you — Human"),
        "transcript: {transcript}"
    );

    orch.shutdown().await;

    // The chat bus carried a UserMessage echo, handoff packet, and local report.
    let mut saw_user_msg = false;
    let mut saw_execution_packet = false;
    let mut saw_session_report = false;
    while let Ok(ev) = events.try_recv() {
        match ev {
            OrchestratorEvent::UserMessage { text } if text.contains("rate limits") => {
                saw_user_msg = true;
            }
            OrchestratorEvent::ExecutionPacket {
                executor,
                text,
                included_in_prompt,
            } => {
                assert_eq!(executor, "claude");
                assert!(included_in_prompt);
                assert!(text.contains("Chosen executor: claude"), "{text}");
                assert!(text.contains("rate limits"), "{text}");
                saw_execution_packet = true;
            }
            OrchestratorEvent::SessionReport { markdown, summary } => {
                assert!(markdown.contains("# Tales session report"), "{markdown}");
                assert!(markdown.contains("- status: executed"), "{markdown}");
                assert!(markdown.contains("- executor: claude"), "{markdown}");
                assert!(markdown.contains("rate limits"), "{markdown}");
                assert_eq!(summary["kind"], "tales_session_summary");
                assert_eq!(summary["outcome"]["status"], "executed");
                assert_eq!(summary["outcome"]["executor"], "claude");
                saw_session_report = true;
            }
            _ => {}
        }
    }
    assert!(saw_user_msg, "expected a UserMessage on the bus");
    assert!(
        saw_execution_packet,
        "expected an ExecutionPacket on the bus"
    );
    assert!(saw_session_report, "expected a SessionReport on the bus");
}

#[tokio::test]
async fn interactive_run_rejected_does_not_execute() {
    let (bus, mut commands_rx) = EventBus::new(1024, 64);
    let commands_tx = bus.commands();

    let vote = r#"{"recommended_executor":"codex","confidence":0.8,"rationale":"x"}"#;
    let drafter = MockAdapter::new(vec!["draft".into(), vote.into()]);
    let critic = MockAdapter::new(vec!["critique".into(), vote.into()]);

    let mut orch = Orchestrator::new(bus.clone());
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

    commands_tx.send(UserCommand::Reject).await.unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_interactive("x", 2, &mut commands_rx),
    )
    .await
    .expect("run_interactive should not hang")
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Rejected));

    orch.shutdown().await;
}
