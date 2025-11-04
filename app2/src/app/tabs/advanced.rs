// SPDX-License-Identifier: Apache-2.0

use iced::widget::{button, column, container, text};
use iced::{Element, Length};
use lupo_configuration::SettingDomain;

use crate::app::message::AppMessage;
use crate::app::state::AppState;

use super::settings::render_setting_entry;

pub fn view(state: &AppState) -> Element<'_, AppMessage> {
    if !state.show_advanced {
        return container(
            column![
                text("Advanced settings expose experimental and high-risk options."),
                button(text("Enable advanced settings")).on_press(AppMessage::EnableAdvanced),
            ]
            .spacing(8)
            .padding(16),
        )
        .width(Length::Fill)
        .into();
    }

    let mut root = column![text("Advanced Settings").size(24)]
        .spacing(8)
        .padding(16)
        .width(Length::Fill);

    let mut count = 0usize;
    for entry in &state.settings_entries {
        if entry.domain == SettingDomain::Advanced {
            root = root.push(render_setting_entry(state, entry));
            count += 1;
        }
    }

    if count == 0 {
        root = root.push(text("No advanced settings configured."));
    }

    container(root).width(Length::Fill).into()
}
