// SPDX-License-Identifier: Apache-2.0

use iced::widget::{button, column, container, row, text, text_input};
use iced::{Alignment, Element, Length};
use lupo_configuration::{SettingSource, SettingValueKind};

use crate::app::message::AppMessage;
use crate::app::state::{AppState, SettingDisplay};

pub fn view(state: &AppState) -> Element<'_, AppMessage> {
    let mut root = column![
        row![
            text("Workspace Settings").size(24),
            button(text("Reload")).on_press(AppMessage::ReloadSettings),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        text(format!("Orphan setting entries: {}", state.orphan_count)),
    ]
    .spacing(12)
    .padding(16)
    .width(Length::Fill);

    if let Some(status) = &state.settings_status_message {
        root = root.push(text(status));
    }

    if let Some(err) = &state.settings_error {
        root = root.push(text(err));
        return container(root).width(Length::Fill).into();
    }

    if state.settings_entries.is_empty() {
        root = root.push(text("No settings available."));
        return container(root).width(Length::Fill).into();
    }

    for entry in &state.settings_entries {
        root = root.push(render_setting_entry(state, entry));
    }

    container(root).width(Length::Fill).into()
}

pub fn render_setting_entry<'a>(
    state: &'a AppState,
    entry: &'a SettingDisplay,
) -> Element<'a, AppMessage> {
    let key = entry.key.clone();
    let source_label = match entry.source {
        SettingSource::Default => "default".to_string(),
        SettingSource::Persisted { ref scope, .. } => format!("persisted ({scope})"),
    };

    let editor = state.settings_editors.get(&key);
    let draft = editor
        .map(|editor| editor.draft_value.as_str())
        .unwrap_or(entry.value_json.as_str());
    let dirty = editor.map(|editor| editor.dirty).unwrap_or(false);
    let validation_error = editor.and_then(|editor| editor.validation_error.clone());

    let save_button = if dirty {
        button(text("Save")).on_press(AppMessage::SaveSetting(key.clone()))
    } else {
        button(text("Save"))
    };

    let discard_button = if dirty {
        button(text("Discard")).on_press(AppMessage::DiscardSetting(key.clone()))
    } else {
        button(text("Discard"))
    };

    let card = column![
        row![text(entry.title.clone()), text(source_label),]
            .spacing(8)
            .align_y(Alignment::Center),
        text(format!("{} • {}", entry.key, entry.domain.as_str())),
        text(entry.description.clone()),
        text_input("Enter JSON value", draft)
            .on_input({
                let key = key.clone();
                move |draft| AppMessage::SettingDraftChanged {
                    key: key.clone(),
                    draft,
                }
            })
            .padding(8)
            .width(Length::Fill),
        row![
            save_button,
            button(text("Reset")).on_press(AppMessage::ResetSetting(key.clone())),
            discard_button,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        text(format!(
            "Expected type: {}",
            setting_value_kind_label(entry.value_kind)
        )),
    ]
    .spacing(8)
    .padding(10)
    .width(Length::Fill);

    if let Some(err) = validation_error {
        container(card.push(text(err))).width(Length::Fill).into()
    } else {
        container(card).width(Length::Fill).into()
    }
}

fn setting_value_kind_label(kind: SettingValueKind) -> String {
    match kind {
        SettingValueKind::Boolean => "boolean".to_string(),
        SettingValueKind::Integer => "integer".to_string(),
        SettingValueKind::Number => "number".to_string(),
        SettingValueKind::String => "string".to_string(),
        SettingValueKind::Object => "object".to_string(),
        SettingValueKind::Array => "array".to_string(),
        SettingValueKind::Enum { variants } => format!("enum ({})", variants.join(", ")),
    }
}
