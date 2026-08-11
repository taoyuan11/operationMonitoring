use anyhow::{Result, bail};
use tokio::sync::mpsc;

use super::DropOldestSender;

pub(super) struct AudioRuntime;

pub(super) struct CapturedAudioFrame {
    pub(super) bytes: Vec<u8>,
}

impl AudioRuntime {
    pub(super) fn set_enabled(&self, _enabled: bool) -> bool {
        false
    }

    pub(super) fn set_default_desktop(&self, _available: bool) {}

    pub(super) fn stop(&self) {}

    pub(super) fn accepts(&self, _frame: &CapturedAudioFrame) -> bool {
        false
    }
}

pub(super) fn negotiated(_codec: Option<&str>) -> bool {
    false
}

pub(super) fn spawn(
    _system_helper: bool,
    _session_id: u32,
    _default_desktop: bool,
    _frame_tx: DropOldestSender<CapturedAudioFrame>,
    _status_tx: mpsc::Sender<String>,
) -> Result<AudioRuntime> {
    bail!("remote desktop audio is not supported on this architecture")
}
