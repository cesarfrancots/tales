//! Process-supervision helpers.
//!
//! In M1 each adapter wires its own reader/writer/waiter tasks and relies on
//! `Command::kill_on_drop(true)` plus closing stdin for graceful teardown. This
//! module provides a defensive backstop — [`ChildGuard`] — for code paths that
//! hold a [`tokio::process::Child`] directly and must guarantee the OS process
//! dies if the guard is dropped (panic, early return). Later milestones grow a
//! full `ProcessSupervisor` that owns every child and fans shutdown out over a
//! cancellation token.

use tokio::process::Child;

/// Kills the wrapped child on drop. A last line of defense against zombies.
pub struct ChildGuard(pub Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort: signal the process to die. `kill_on_drop` on the
        // `Command` already covers the common case; this covers children moved
        // out of a builder that did not set it.
        let _ = self.0.start_kill();
    }
}

impl std::ops::Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
