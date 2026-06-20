//! The event/command bus that decouples the core from any frontend.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, mpsc};

use crate::event::{OrchestratorEvent, UserCommand};

/// Owns the broadcast sender for events and the mpsc sender for commands.
///
/// Clone freely and hand a clone to every frontend. Events use a lossy
/// `broadcast` channel (a slow subscriber lags and recovers, it never blocks
/// the engine); commands use `mpsc` (many interchangeable frontend producers,
/// one engine consumer).
#[derive(Clone)]
pub struct EventBus {
    events: broadcast::Sender<OrchestratorEvent>,
    commands_tx: mpsc::Sender<UserCommand>,
    history: Arc<Mutex<EventHistory>>,
}

struct EventHistory {
    capacity: usize,
    events: VecDeque<OrchestratorEvent>,
}

impl EventBus {
    /// Build a bus, returning it alongside the single command receiver the
    /// engine drains.
    pub fn new(event_buffer: usize, command_buffer: usize) -> (Self, mpsc::Receiver<UserCommand>) {
        let (events, _) = broadcast::channel(event_buffer);
        let (commands_tx, commands_rx) = mpsc::channel(command_buffer);
        (
            Self {
                events,
                commands_tx,
                history: Arc::new(Mutex::new(EventHistory {
                    capacity: event_buffer,
                    events: VecDeque::with_capacity(event_buffer.min(1024)),
                })),
            },
            commands_rx,
        )
    }

    /// Subscribe a new frontend to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<OrchestratorEvent> {
        self.events.subscribe()
    }

    /// Subscribe and receive a bounded replay of durable events that were
    /// already emitted. Token deltas are intentionally excluded from replay:
    /// completed [`OrchestratorEvent::Message`] events carry the durable turn
    /// text, while streaming deltas would make reconnects noisy and expensive.
    pub fn subscribe_with_history(
        &self,
    ) -> (
        broadcast::Receiver<OrchestratorEvent>,
        Vec<OrchestratorEvent>,
    ) {
        let history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let receiver = self.events.subscribe();
        let replay = history.events.iter().cloned().collect();
        (receiver, replay)
    }

    /// Publish an event. Returns silently if there are no subscribers.
    pub fn emit(&self, event: OrchestratorEvent) {
        if should_replay(&event) {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if history.capacity > 0 {
                while history.events.len() >= history.capacity {
                    history.events.pop_front();
                }
                history.events.push_back(event.clone());
            }
        }
        let _ = self.events.send(event);
    }

    /// A sender frontends use to push user commands into the core.
    pub fn commands(&self) -> mpsc::Sender<UserCommand> {
        self.commands_tx.clone()
    }
}

fn should_replay(event: &OrchestratorEvent) -> bool {
    !matches!(event, OrchestratorEvent::Token { .. })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn history_replays_durable_events_but_not_token_deltas() {
        let (bus, _rx) = EventBus::new(8, 8);
        let agent = Uuid::new_v4();

        bus.emit(OrchestratorEvent::AgentSpawned {
            agent,
            label: "claude".to_string(),
            session_id: "s1".to_string(),
        });
        bus.emit(OrchestratorEvent::Token {
            agent,
            text: "streaming".to_string(),
        });
        bus.emit(OrchestratorEvent::Message {
            agent,
            text: "complete".to_string(),
        });

        let (_events, replay) = bus.subscribe_with_history();
        assert_eq!(replay.len(), 2);
        assert!(matches!(replay[0], OrchestratorEvent::AgentSpawned { .. }));
        assert!(matches!(replay[1], OrchestratorEvent::Message { .. }));
    }

    #[test]
    fn history_is_bounded_by_event_buffer() {
        let (bus, _rx) = EventBus::new(2, 8);

        bus.emit(OrchestratorEvent::Log {
            level: "info".to_string(),
            msg: "one".to_string(),
        });
        bus.emit(OrchestratorEvent::Log {
            level: "info".to_string(),
            msg: "two".to_string(),
        });
        bus.emit(OrchestratorEvent::Log {
            level: "info".to_string(),
            msg: "three".to_string(),
        });

        let (_events, replay) = bus.subscribe_with_history();
        let messages: Vec<_> = replay
            .iter()
            .filter_map(|event| match event {
                OrchestratorEvent::Log { msg, .. } => Some(msg.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(messages, vec!["two", "three"]);
    }
}
