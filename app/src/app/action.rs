// SPDX-License-Identifier: Apache-2.0

use lupo_runtime::bridge::RuntimeEvent;

use super::state::Tab;

pub const MENU_ITEM_OPEN_SETTINGS: &str = "lupo-open-settings";
pub const MENU_ITEM_TOGGLE_HELP_MODE: &str = "lupo-toggle-help-mode";
pub const MENU_ITEM_OPEN_SHORTCUTS: &str = "lupo-open-shortcuts";
pub const MENU_ITEM_OPEN_MANUAL: &str = "lupo-open-manual";

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutAction {
    OpenSettings,
    TogglePlayback,
    SkipCurrent,
    VolumeUp,
    VolumeDown,
    ToggleHelpMode,
    ExitHelpMode,
    Quit,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum AppAction {
    Event(UiEvent),
    TabSelected(Tab),
    ChatLogScrollPositionChanged { at_bottom: bool },
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
    ReconnectTwitch,
    RevokeTwitchCredentials,
    TwitchChannelLoginDraftChanged(String),
    SaveTwitchChannelLogin,
    ClearTwitchChannelLogin,
    ReconnectYouTube,
    YouTubeLiveChatIdDraftChanged(String),
    SaveYouTubeLiveChatId,
    ClearYouTubeLiveChatId,
    YouTubeApiKeyDraftChanged(String),
    SaveYouTubeApiKey,
    RevokeYouTubeApiKey,
    OpenAiApiKeyDraftChanged(String),
    SaveOpenAiApiKey,
    RevokeOpenAiApiKey,
    EnableAdvanced,
    CancelAdvancedOpen,
    AdvancedResetKeySelected(String),
    ResetSelectedAdvancedKey,
    ResetAllAdvancedSettings,
    ExportAdvancedSnapshot,
    AdvancedImportSnapshotDraftChanged(String),
    PreviewAdvancedImportSnapshot,
    ApplyAdvancedImportSnapshot,
    ChatRowSelected(u64),
    ReplayMessage(u64),
    SkipMessage(u64),
    OpenShortcutCatalog,
    CloseShortcutCatalog,
    ManualNotImplemented,
    SetStatusMessage(String),
    ChatterSearchDraftChanged(String),
    ChatterRoleFilterChanged(String),
    ChatterOverrideFilterChanged(String),
    ChatterSelected(String),
    ChatterVoiceDraftChanged(String),
    ChatterDisplayNameDraftChanged(String),
    ChatterNotesDraftChanged(String),
    SaveSelectedChatterOverride,
    ResetSelectedChatterOverride,
    RequestQuit,
    Shortcut(ShortcutAction),
}
