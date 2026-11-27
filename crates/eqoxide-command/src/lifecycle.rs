//! Lifecycle command verbs (#459 stragglers).
//!
//! Domain: `/v1/lifecycle/*` (camp/exit + respawn). Slots live in `self.lifecycle`.
//!
//! Gameplay owns camp/respawn draining and the associated timers. Camp attribution is captured
//! under the same lock as the last-wins payload so a later toggle replaces both together.
//!
//! The HTTP handlers, HUD buttons, and `/camp` keyword share these request methods.

use super::CommandState;
use eqoxide_ipc::{CampCmd, CampReq};
use std::sync::Arc;

impl CommandState {
    /// Start or toggle a camp (POST /v1/lifecycle/exit uses `Start`; POST /v1/lifecycle/camp, the
    /// HUD Camp button, and the `/camp` chat keyword use `Toggle`). See `ipc::CampCmd`.
    pub fn request_camp(&self, cmd: CampCmd) {
        self.request_camp_with_command(cmd, None);
    }

    pub fn request_camp_with_command(&self, cmd: CampCmd, command: Option<&str>) {
        // LAST-WINS (#347): `camp` is a toggle whose LATEST value is the intended one, and
        // POST /v1/lifecycle/exit (`CampCmd::Start`) must be able to override an in-progress camp —
        // it is the only way to tear down a wedged session. Refusing here would strand it.
        let mut slot = self.lifecycle.camp.lock().unwrap();
        *slot = Some(cmd);
        self.actions.set_packet_command(Arc::as_ptr(&self.lifecycle.camp) as usize, command);
    }

    pub fn drain_camp_with_command(
        &self,
        camp: &CampReq,
        apply: impl FnOnce(CampCmd, Option<Arc<str>>),
    ) {
        let mut slot = camp.lock().unwrap();
        if let Some(cmd) = slot.take() {
            let command = self.actions.take_packet_command(Arc::as_ptr(camp) as usize);
            // Keep the lock through application so the next writer sees the updated camp timer.
            apply(cmd, command);
        }
    }

    /// Release a held-dead character back to its bind point (POST /v1/lifecycle/respawn, the HUD
    /// Respawn button). A plain flag, not `Option` — `gameplay::run_gameplay_phase` clears it itself
    /// once the retry-driven respawn completes (see module docs).
    pub fn request_respawn(&self) {
        *self.lifecycle.respawn.lock().unwrap() = true;
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use eqoxide_ipc::CampCmd;

    #[test]
    fn request_camp_sets_the_slot() {
        let cs = CommandState::default();
        assert!(cs.lifecycle.camp.lock().unwrap().is_none());
        cs.request_camp(CampCmd::Toggle);
        assert_eq!(*cs.lifecycle.camp.lock().unwrap(), Some(CampCmd::Toggle));
        cs.request_camp(CampCmd::Start);
        assert_eq!(*cs.lifecycle.camp.lock().unwrap(), Some(CampCmd::Start));
    }

    #[test]
    fn request_respawn_sets_the_flag() {
        let cs = CommandState::default();
        assert!(!*cs.lifecycle.respawn.lock().unwrap());
        cs.request_respawn();
        assert!(*cs.lifecycle.respawn.lock().unwrap());
    }
}
