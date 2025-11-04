// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::pipeline::PipelineEvent;
use super::twitch::TwitchEvent;

#[derive(Clone, Default)]
pub struct RuntimeBridge {
    queue: Arc<Mutex<VecDeque<RuntimeEvent>>>,
}

impl RuntimeBridge {
    pub fn push(&self, event: RuntimeEvent) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(event);
        }
    }

    pub fn drain(&self) -> Vec<RuntimeEvent> {
        let mut events = Vec::new();
        if let Ok(mut queue) = self.queue.lock() {
            while let Some(event) = queue.pop_front() {
                events.push(event);
            }
        }
        events
    }
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum RuntimeEvent {
    Pipeline(PipelineEvent),
    Twitch(TwitchEvent),
    ActionResult(ActionResult),
    Issue(RuntimeIssue),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeIssue {
    PipelineLagged { skipped: u64 },
    PipelineChannelClosed,
    TwitchChannelClosed,
}

impl RuntimeIssue {
    pub fn user_message(&self) -> String {
        match self {
            RuntimeIssue::PipelineLagged { skipped } => {
                format!("Pipeline event stream lagged; skipped {skipped} events.")
            }
            RuntimeIssue::PipelineChannelClosed => "Pipeline event stream closed.".to_string(),
            RuntimeIssue::TwitchChannelClosed => "Twitch event stream closed.".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeErrorKind {
    SubmitManual,
    EnsurePlayback,
    SetPlaybackEnabled,
    StopNow,
    ClearQueue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeError {
    kind: RuntimeErrorKind,
    detail: String,
}

impl RuntimeError {
    pub fn new(kind: RuntimeErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    pub fn user_message(&self) -> String {
        match self.kind {
            RuntimeErrorKind::SubmitManual => {
                format!("Failed to queue message: {}", self.detail)
            }
            RuntimeErrorKind::EnsurePlayback => {
                format!("Failed to nudge playback: {}", self.detail)
            }
            RuntimeErrorKind::SetPlaybackEnabled => {
                format!("Failed to update playback state: {}", self.detail)
            }
            RuntimeErrorKind::StopNow => {
                format!("Failed to stop playback: {}", self.detail)
            }
            RuntimeErrorKind::ClearQueue => {
                format!("Failed to clear queue: {}", self.detail)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum ActionResult {
    SubmitManual {
        play_now: bool,
        submit_result: Result<(), RuntimeError>,
        ensure_result: Option<Result<(), RuntimeError>>,
    },
    PlaybackEnabled {
        enabled: bool,
        result: Result<(), RuntimeError>,
    },
    StopNow {
        should_quit: bool,
        window_id: Option<iced::window::Id>,
        result: Result<(), RuntimeError>,
    },
    ClearQueue {
        result: Result<(), RuntimeError>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_push_and_drain_round_trip() {
        let bridge = RuntimeBridge::default();
        bridge.push(RuntimeEvent::ActionResult(ActionResult::ClearQueue {
            result: Ok(()),
        }));

        let events = bridge.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            RuntimeEvent::ActionResult(ActionResult::ClearQueue { result: Ok(()) })
        ));
        assert!(bridge.drain().is_empty());
    }

    #[test]
    fn runtime_issue_has_user_message() {
        let issue = RuntimeIssue::PipelineLagged { skipped: 3 };
        assert!(issue.user_message().contains("skipped 3"));
    }
}
