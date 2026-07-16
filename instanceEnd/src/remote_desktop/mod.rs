#![cfg_attr(not(windows), allow(dead_code))]

use std::{collections::HashMap, time::Duration};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{activity::ActivityTracker, config::AgentConfig, models::AgentInbound};

#[cfg(windows)]
mod windows;

pub const CAPABILITY: &str = "remote_desktop_v1";
pub const FRAME_HEADER_LEN: usize = 32;
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_CONTROL_BYTES: usize = 16 * 1024;
pub(super) const DATA_CHANNEL_JOIN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct DesktopOptions {
    pub pipe: String,
    pub max_width: u32,
    pub max_height: u32,
    pub min_fps: u8,
    pub max_fps: u8,
    pub jpeg_quality: u8,
    pub system_helper: bool,
}

#[derive(Debug, Clone)]
pub struct DesktopOpenRequest {
    pub session_id: String,
    pub stream_token: String,
    pub max_width: u32,
    pub max_height: u32,
    pub min_fps: u8,
    pub max_fps: u8,
    pub jpeg_quality: u8,
}

pub struct DesktopManager {
    config: AgentConfig,
    activity: ActivityTracker,
    outbound: mpsc::UnboundedSender<AgentInbound>,
    sessions: HashMap<String, ActiveDesktop>,
}

struct ActiveDesktop {
    task: JoinHandle<()>,
    close: Option<oneshot::Sender<String>>,
}

impl DesktopManager {
    pub fn new(
        config: AgentConfig,
        activity: ActivityTracker,
        outbound: mpsc::UnboundedSender<AgentInbound>,
    ) -> Self {
        Self {
            config,
            activity,
            outbound,
            sessions: HashMap::new(),
        }
    }

    pub fn open(&mut self, request: DesktopOpenRequest) {
        self.reap_finished();
        if !cfg!(windows) {
            self.closed(request.session_id, "unsupported_platform");
            return;
        }
        if self.sessions.contains_key(&request.session_id) || !self.sessions.is_empty() {
            self.closed(request.session_id, "desktop_busy");
            return;
        }
        let Some(activity) = self.activity.try_enter() else {
            self.closed(request.session_id, "agent_draining");
            return;
        };

        let session_id = request.session_id.clone();
        let task_session_id = session_id.clone();
        let outbound = self.outbound.clone();
        let config = self.config.clone();
        let (close_tx, close_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _activity = activity;
            let reason = match run_session(config, request, outbound.clone(), close_rx).await {
                Ok(reason) => reason,
                Err(error) => {
                    crate::logging::error(format_args!(
                        "remote desktop session {task_session_id} failed: {error:#}"
                    ));
                    error_reason(&error)
                }
            };
            let _ = outbound.send(AgentInbound::DesktopClosed {
                session_id: task_session_id,
                reason,
            });
        });
        self.sessions.insert(
            session_id,
            ActiveDesktop {
                task,
                close: Some(close_tx),
            },
        );
    }

    pub fn close(&mut self, session_id: &str, reason: &str) {
        if let Some(active) = self.sessions.get_mut(session_id) {
            if let Some(close) = active.close.take() {
                let _ = close.send(reason.to_string());
            }
        }
    }

    pub async fn close_all(&mut self, reason: &str) {
        let sessions = self.request_close_all(reason);
        for (session_id, task) in sessions {
            if let Err(error) = task.await {
                crate::logging::error(format_args!(
                    "remote desktop session {session_id} cleanup task failed: {error}"
                ));
            }
        }
    }

    fn request_close_all(&mut self, reason: &str) -> Vec<(String, JoinHandle<()>)> {
        self.sessions
            .drain()
            .map(|(session_id, mut active)| {
                if let Some(close) = active.close.take() {
                    let _ = close.send(reason.to_string());
                }
                (session_id, active.task)
            })
            .collect()
    }

    fn reap_finished(&mut self) {
        self.sessions.retain(|_, active| !active.task.is_finished());
    }

    fn closed(&self, session_id: String, reason: &str) {
        let _ = self.outbound.send(AgentInbound::DesktopClosed {
            session_id,
            reason: reason.to_string(),
        });
    }
}

impl Drop for DesktopManager {
    fn drop(&mut self) {
        // JoinHandle::drop detaches the task. Sending first lets it complete the helper's
        // release-all/ACK handshake even when an unexpected caller drops the manager.
        drop(self.request_close_all("agent_shutdown"));
    }
}

#[cfg(windows)]
async fn run_session(
    config: AgentConfig,
    request: DesktopOpenRequest,
    outbound: mpsc::UnboundedSender<AgentInbound>,
    close: oneshot::Receiver<String>,
) -> Result<String> {
    windows::run_session(config, request, outbound, close).await
}

#[cfg(not(windows))]
async fn run_session(
    _config: AgentConfig,
    _request: DesktopOpenRequest,
    _outbound: mpsc::UnboundedSender<AgentInbound>,
    _close: oneshot::Receiver<String>,
) -> Result<String> {
    bail!("unsupported_platform")
}

pub fn run_helper(options: DesktopOptions) -> Result<()> {
    #[cfg(windows)]
    {
        return tokio::runtime::Runtime::new()?.block_on(windows::run_helper(options));
    }
    #[cfg(not(windows))]
    {
        let _ = options;
        bail!("desktop-helper is only available on Windows")
    }
}

fn error_reason(error: &anyhow::Error) -> String {
    let value = error.to_string();
    const KNOWN: [&str; 8] = [
        "no_active_session",
        "multiple_active_sessions",
        "desktop_locked",
        "secure_desktop",
        "unsupported_platform",
        "agent_draining",
        "data_channel_timeout",
        "frame_too_large",
    ];
    KNOWN
        .into_iter()
        .find(|reason| value.contains(reason))
        .unwrap_or("agent_error")
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub sequence: u64,
    pub captured_at_ms: u64,
    pub width: u32,
    pub height: u32,
}

impl FrameHeader {
    pub fn encode(self) -> [u8; FRAME_HEADER_LEN] {
        let mut output = [0_u8; FRAME_HEADER_LEN];
        output[0..4].copy_from_slice(b"OMRD");
        output[4] = 1;
        output[5] = 1;
        output[8..16].copy_from_slice(&self.sequence.to_be_bytes());
        output[16..24].copy_from_slice(&self.captured_at_ms.to_be_bytes());
        output[24..28].copy_from_slice(&self.width.to_be_bytes());
        output[28..32].copy_from_slice(&self.height.to_be_bytes());
        output
    }

    pub fn decode(value: &[u8]) -> Result<Self> {
        if value.len() < FRAME_HEADER_LEN || &value[0..4] != b"OMRD" {
            bail!("invalid remote desktop frame header")
        }
        if value[4] != 1 || value[5] != 1 || value[6] != 0 || value[7] != 0 {
            bail!("unsupported remote desktop frame version or codec")
        }
        Ok(Self {
            sequence: u64::from_be_bytes(value[8..16].try_into().unwrap()),
            captured_at_ms: u64::from_be_bytes(value[16..24].try_into().unwrap()),
            width: u32::from_be_bytes(value[24..28].try_into().unwrap()),
            height: u32::from_be_bytes(value[28..32].try_into().unwrap()),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DesktopControl {
    PointerMove {
        x: f64,
        y: f64,
    },
    PointerButton {
        x: f64,
        y: f64,
        button: u8,
        down: bool,
    },
    Wheel {
        x: f64,
        y: f64,
        delta_x: i32,
        delta_y: i32,
    },
    Key {
        code: String,
        down: bool,
        #[serde(default)]
        repeat: bool,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    ReleaseAll,
    SecureAttention,
    Feedback {
        sequence: u64,
        fps: f64,
        decode_ms: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveSettings {
    pub fps: u8,
    pub jpeg_quality: u8,
    last_sequence: Option<u64>,
}

impl AdaptiveSettings {
    pub fn initial(min_fps: u8, max_fps: u8, jpeg_quality: u8) -> Self {
        let min_fps = min_fps.clamp(1, 12);
        Self {
            fps: max_fps.clamp(min_fps, 12),
            jpeg_quality: jpeg_quality.clamp(50, 75),
            // Browser feedback starts at sequence zero before the first frame. Treat that as
            // already observed so a static/not-yet-captured desktop cannot lower quality.
            last_sequence: Some(0),
        }
    }

    pub fn update(
        &mut self,
        min_fps: u8,
        max_fps: u8,
        sequence: u64,
        rendered_fps: f64,
        decode_ms: f64,
    ) {
        let previous_sequence = self.last_sequence.replace(sequence).unwrap_or(sequence);
        let produced_frames = sequence.saturating_sub(previous_sequence);
        if produced_frames < u64::from(self.fps) {
            return;
        }
        let min_fps = min_fps.clamp(1, 12);
        let max_fps = max_fps.clamp(min_fps, 12);
        if rendered_fps + 1.0 < f64::from(self.fps) || decode_ms > 60.0 {
            self.fps = self.fps.saturating_sub(1).max(min_fps);
            self.jpeg_quality = self.jpeg_quality.saturating_sub(5).max(50);
        } else if rendered_fps >= f64::from(self.fps) - 0.25 && decode_ms < 30.0 {
            self.fps = self.fps.saturating_add(1).min(max_fps);
            self.jpeg_quality = self.jpeg_quality.saturating_add(2).min(75);
        }
    }
}

pub(super) fn scaled_dimensions(
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    let scale = (max_width as f64 / width.max(1) as f64)
        .min(max_height as f64 / height.max(1) as f64)
        .min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

pub(super) fn absolute_pointer_coordinate(value: f64) -> i32 {
    (value.clamp(0.0, 1.0) * 65535.0).round() as i32
}

pub(super) fn dom_code_to_vk(code: &str) -> Option<u16> {
    if let Some(letter) = code.strip_prefix("Key").filter(|value| value.len() == 1) {
        return Some(letter.as_bytes()[0].to_ascii_uppercase() as u16);
    }
    if let Some(digit) = code.strip_prefix("Digit").filter(|value| value.len() == 1) {
        return Some(digit.as_bytes()[0] as u16);
    }
    if let Some(number) = code
        .strip_prefix('F')
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (1..=24).contains(value))
    {
        return Some(0x6f + number);
    }
    Some(match code {
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Enter" | "NumpadEnter" => 0x0d,
        "ShiftLeft" => 0xa0,
        "ShiftRight" => 0xa1,
        "ControlLeft" => 0xa2,
        "ControlRight" => 0xa3,
        "AltLeft" => 0xa4,
        "AltRight" => 0xa5,
        "CapsLock" => 0x14,
        "Escape" => 0x1b,
        "Space" => 0x20,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "End" => 0x23,
        "Home" => 0x24,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "Insert" => 0x2d,
        "Delete" => 0x2e,
        "MetaLeft" => 0x5b,
        "MetaRight" => 0x5c,
        "ContextMenu" => 0x5d,
        "Semicolon" => 0xba,
        "Equal" => 0xbb,
        "Comma" => 0xbc,
        "Minus" => 0xbd,
        "Period" => 0xbe,
        "Slash" => 0xbf,
        "Backquote" => 0xc0,
        "BracketLeft" => 0xdb,
        "Backslash" => 0xdc,
        "BracketRight" => 0xdd,
        "Quote" => 0xde,
        _ => return None,
    })
}

pub(super) fn dom_code_uses_extended_key(code: &str) -> bool {
    matches!(
        code,
        "NumpadEnter"
            | "ControlRight"
            | "AltRight"
            | "PageUp"
            | "PageDown"
            | "End"
            | "Home"
            | "ArrowLeft"
            | "ArrowUp"
            | "ArrowRight"
            | "ArrowDown"
            | "Insert"
            | "Delete"
            | "MetaLeft"
            | "MetaRight"
            | "ContextMenu"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager(activity: ActivityTracker) -> DesktopManager {
        let (outbound, _) = mpsc::unbounded_channel();
        DesktopManager::new(
            AgentConfig {
                server: "http://127.0.0.1:13500".to_string(),
                identity_file: None,
                report_interval: 5,
                state_dir: None,
                log_file: None,
                log_max_bytes: 1024,
                log_history: 1,
                update_dir: None,
            },
            activity,
            outbound,
        )
    }

    #[tokio::test]
    async fn dropping_manager_requests_graceful_agent_shutdown() {
        let mut manager = test_manager(ActivityTracker::default());
        let (close_tx, close_rx) = oneshot::channel();
        let (observed_tx, observed_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let reason = close_rx.await.unwrap();
            let _ = observed_tx.send(reason);
        });
        manager.sessions.insert(
            "desktop-1".to_string(),
            ActiveDesktop {
                task,
                close: Some(close_tx),
            },
        );

        drop(manager);

        assert_eq!(observed_rx.await.unwrap(), "agent_shutdown");
    }

    #[tokio::test]
    async fn close_all_waits_for_session_cleanup_and_activity_guard() {
        let activity = ActivityTracker::default();
        let guard = activity.try_enter().unwrap();
        let mut manager = test_manager(activity.clone());
        let (close_tx, close_rx) = oneshot::channel();
        let (observed_tx, observed_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let reason = close_rx.await.unwrap();
            let _ = observed_tx.send(reason);
            let _ = finish_rx.await;
            drop(guard);
        });
        manager.sessions.insert(
            "desktop-1".to_string(),
            ActiveDesktop {
                task,
                close: Some(close_tx),
            },
        );

        let cleanup = tokio::spawn(async move {
            manager.close_all("agent_disconnected").await;
        });
        assert_eq!(observed_rx.await.unwrap(), "agent_disconnected");
        assert_eq!(activity.active_count(), 1);
        assert!(!cleanup.is_finished());

        let _ = finish_tx.send(());
        cleanup.await.unwrap();
        assert_eq!(activity.active_count(), 0);
    }

    #[test]
    fn omrd_header_is_fixed_big_endian_and_round_trips() {
        let header = FrameHeader {
            sequence: 0x0102_0304_0506_0708,
            captured_at_ms: 1_725_000_000_123,
            width: 1920,
            height: 1080,
        };
        let encoded = header.encode();
        assert_eq!(encoded.len(), 32);
        assert_eq!(&encoded[..8], b"OMRD\x01\x01\0\0");
        assert_eq!(FrameHeader::decode(&encoded).unwrap(), header);
    }

    #[test]
    fn rejects_unknown_frame_version_codec_and_flags() {
        for index in [4, 5, 6, 7] {
            let mut encoded = FrameHeader {
                sequence: 1,
                captured_at_ms: 2,
                width: 3,
                height: 4,
            }
            .encode();
            encoded[index] = 99;
            assert!(FrameHeader::decode(&encoded).is_err());
        }
    }

    #[test]
    fn adaptive_settings_stay_within_v1_limits() {
        let mut settings = AdaptiveSettings::initial(8, 12, 70);
        settings.update(8, 12, 0, 0.0, 500.0);
        assert_eq!((settings.fps, settings.jpeg_quality), (12, 70));
        for sequence in (12..=240).step_by(12) {
            settings.update(8, 12, sequence, 2.0, 100.0);
        }
        assert_eq!((settings.fps, settings.jpeg_quality), (8, 50));
        for sequence in (252..=480).step_by(12) {
            settings.update(8, 12, sequence, 12.0, 5.0);
        }
        assert_eq!((settings.fps, settings.jpeg_quality), (12, 75));

        settings.update(8, 12, 481, 0.0, 500.0);
        assert_eq!((settings.fps, settings.jpeg_quality), (12, 75));

        for sequence in 482..490 {
            settings.update(8, 12, sequence, 1.0, 500.0);
        }
        assert_eq!((settings.fps, settings.jpeg_quality), (12, 75));
    }

    #[test]
    fn desktop_controls_match_browser_wire_shape() {
        assert_eq!(
            serde_json::from_str::<DesktopControl>(r#"{"type":"pointer_move","x":0.25,"y":0.75}"#,)
                .unwrap(),
            DesktopControl::PointerMove { x: 0.25, y: 0.75 }
        );
        assert_eq!(
            serde_json::from_str::<DesktopControl>(r#"{"type":"secure_attention"}"#).unwrap(),
            DesktopControl::SecureAttention
        );
    }

    #[test]
    fn scales_desktop_without_upscaling_or_distorting() {
        assert_eq!(scaled_dimensions(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(scaled_dimensions(1080, 1920, 1920, 1080), (608, 1080));
        assert_eq!(scaled_dimensions(1280, 720, 1920, 1080), (1280, 720));
    }

    #[test]
    fn normalized_pointer_coordinates_clamp_to_send_input_range() {
        assert_eq!(absolute_pointer_coordinate(-1.0), 0);
        assert_eq!(absolute_pointer_coordinate(0.5), 32768);
        assert_eq!(absolute_pointer_coordinate(2.0), 65535);
    }

    #[test]
    fn maps_dom_codes_without_accepting_unknown_or_secure_attention_sequences() {
        assert_eq!(dom_code_to_vk("KeyA"), Some(0x41));
        assert_eq!(dom_code_to_vk("Digit9"), Some(0x39));
        assert_eq!(dom_code_to_vk("F12"), Some(0x7b));
        assert_eq!(dom_code_to_vk("ShiftLeft"), Some(0xa0));
        assert_eq!(dom_code_to_vk("ShiftRight"), Some(0xa1));
        assert_eq!(dom_code_to_vk("ControlLeft"), Some(0xa2));
        assert_eq!(dom_code_to_vk("ControlRight"), Some(0xa3));
        assert_eq!(dom_code_to_vk("AltLeft"), Some(0xa4));
        assert_eq!(dom_code_to_vk("AltRight"), Some(0xa5));
        assert!(dom_code_uses_extended_key("ControlRight"));
        assert!(dom_code_uses_extended_key("ArrowDown"));
        assert!(!dom_code_uses_extended_key("KeyA"));
        assert_eq!(dom_code_to_vk("Unknown"), None);
    }
}
