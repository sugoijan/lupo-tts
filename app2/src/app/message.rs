// SPDX-License-Identifier: Apache-2.0

use iced::window;

use crate::runtime::bridge::RuntimeEvent;

use super::state::Tab;

#[derive(Clone, Debug)]
pub enum UiAction {
    QueueMessage,
    PlayNow,
    TogglePlayback,
    ClearQueue,
    StopNow,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum UiEvent {
    PollRuntime,
    Runtime(RuntimeEvent),
    WindowCloseRequested(window::Id),
    ResolveWindowClose(Option<window::Id>),
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum AppMessage {
    Event(UiEvent),
    TabSelected(Tab),
    InputChanged(String),
    Trigger(UiAction),
    StopContinue,
    StopCancel,
    QuitAnyway,
    QuitStopBefore,
    QuitCancel,
    ReloadSettings,
    SettingDraftChanged { key: String, draft: String },
    SaveSetting(String),
    ResetSetting(String),
    DiscardSetting(String),
    EnableAdvanced,
    MessageListScrolled(f32),
    RequestQuit,
}
