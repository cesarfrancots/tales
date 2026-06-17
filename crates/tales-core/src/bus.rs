//! The event/command bus that decouples the core from any frontend.

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
            },
            commands_rx,
        )
    }

    /// Subscribe a new frontend to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<OrchestratorEvent> {
        self.events.subscribe()
    }

    /// Publish an event. Returns silently if there are no subscribers.
    pub fn emit(&self, event: OrchestratorEvent) {
        let _ = self.events.send(event);
    }

    /// A sender frontends use to push user commands into the core.
    pub fn commands(&self) -> mpsc::Sender<UserCommand> {
        self.commands_tx.clone()
    }
}
