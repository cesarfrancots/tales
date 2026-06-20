//! A scripted adapter for deterministic, API-free tests and demos.
//!
//! Each [`AgentCommand::StartTurn`] consumes the next canned response, streamed
//! word-by-word as `TokenDelta`s and then delivered whole as a `MessageFinal`,
//! followed by `TurnComplete` — mirroring a real adapter's event shape so the
//! orchestrator can be exercised without spending API calls.

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{AgentAdapter, AgentCaps, AgentCommand, AgentEvent, SpawnCtx};
use crate::Result;

/// An adapter that replays a fixed list of responses.
pub struct MockAdapter {
    responses: Vec<String>,
}

impl MockAdapter {
    pub fn new(responses: Vec<String>) -> Self {
        Self { responses }
    }
}

#[async_trait]
impl AgentAdapter for MockAdapter {
    fn caps(&self) -> AgentCaps {
        AgentCaps {
            midturn_injection: true,
            structured_output: false,
            resumable: false,
            mcp: false,
        }
    }

    async fn spawn(
        &mut self,
        ctx: SpawnCtx,
        events_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<mpsc::Sender<AgentCommand>> {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<AgentCommand>(64);
        let agent = ctx.agent;
        let mut responses = std::mem::take(&mut self.responses).into_iter();

        tokio::spawn(async move {
            let _ = events_tx
                .send(AgentEvent::SessionReady {
                    agent,
                    native_session_id: format!("mock-{agent}"),
                })
                .await;

            let mut turn = 0u64;
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    AgentCommand::StartTurn { .. } | AgentCommand::InjectMessage { .. } => {
                        turn += 1;
                        let _ = events_tx
                            .send(AgentEvent::TurnStarted { agent, turn })
                            .await;

                        let reply = responses
                            .next()
                            .unwrap_or_else(|| "(mock: no more responses)".to_string());
                        for word in reply.split_whitespace() {
                            let _ = events_tx
                                .send(AgentEvent::TokenDelta {
                                    agent,
                                    turn,
                                    text: format!("{word} "),
                                })
                                .await;
                        }
                        let _ = events_tx
                            .send(AgentEvent::MessageFinal {
                                agent,
                                turn,
                                text: reply,
                            })
                            .await;
                        let _ = events_tx
                            .send(AgentEvent::TurnComplete {
                                agent,
                                turn,
                                cost_usd: None,
                                token_usage: None,
                            })
                            .await;
                    }
                    AgentCommand::Shutdown => break,
                }
            }

            let _ = events_tx
                .send(AgentEvent::Exited {
                    agent,
                    code: Some(0),
                })
                .await;
        });

        Ok(cmd_tx)
    }
}
