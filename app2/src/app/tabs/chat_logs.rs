// SPDX-License-Identifier: Apache-2.0

use iced::widget::{
    Column, Container, button, column, container, row, scrollable, text, text_input,
};
use iced::{Alignment, Element, Length};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::app::message::{AppMessage, UiAction};
use crate::app::state::{AppState, MessageEntry, MessageStatus};
use crate::runtime::twitch::TwitchStatus;

pub fn view(state: &AppState) -> Element<'_, AppMessage> {
    let status_strip = render_status_strip(state);

    let list_content: Element<'_, _> = if state.messages.is_empty() {
        container(text("No queued messages yet."))
            .width(Length::Fill)
            .padding(16)
            .into()
    } else {
        let mut messages = Column::new().spacing(8);
        for entry in &state.messages {
            messages = messages.push(render_message_entry(entry, state.active_playing_id));
        }
        container(messages).width(Length::Fill).padding(8).into()
    };

    let message_scroll = scrollable(list_content)
        .height(Length::FillPortion(4))
        .on_scroll(|viewport| AppMessage::MessageListScrolled(viewport.relative_offset().y));

    let input_is_empty = state.input_value.trim().is_empty();

    let add_to_queue = if state.busy || input_is_empty {
        button(text("Add to queue"))
    } else {
        button(text("Add to queue")).on_press(AppMessage::Trigger(UiAction::QueueMessage))
    };

    let play_now = if state.busy || input_is_empty {
        button(text("Play now"))
    } else {
        button(text("Play now")).on_press(AppMessage::Trigger(UiAction::PlayNow))
    };

    let toggle_label = if state.playback_enabled {
        "Stop"
    } else {
        "Play"
    };
    let toggle_playback = if state.busy {
        button(text(toggle_label))
    } else {
        button(text(toggle_label)).on_press(AppMessage::Trigger(UiAction::TogglePlayback))
    };

    let clear_queue = if state.busy || state.messages.is_empty() {
        button(text("Clear queue"))
    } else {
        button(text("Clear queue")).on_press(AppMessage::Trigger(UiAction::ClearQueue))
    };

    let input_row = row![
        text_input("Manual message", &state.input_value)
            .on_input(AppMessage::InputChanged)
            .on_submit(AppMessage::Trigger(UiAction::QueueMessage))
            .padding(10)
            .width(Length::FillPortion(3)),
        add_to_queue,
        play_now,
        toggle_playback,
        clear_queue,
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut content = column![status_strip, message_scroll, input_row]
        .spacing(12)
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill);

    if let Some(status) = &state.status_message {
        content = content.push(text(status));
    }

    if !state.sticky_to_latest && state.hidden_message_count > 0 {
        content = content.push(text(format!(
            "Hidden messages: {}",
            state.hidden_message_count
        )));
    }

    if state.show_stop_confirmation {
        content = content.push(stop_confirmation(state));
    }

    if state.show_quit_confirmation {
        content = content.push(quit_confirmation());
    }

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_status_strip(state: &AppState) -> Element<'_, AppMessage> {
    let (twitch_label, _twitch_color) = match &state.twitch_status {
        TwitchStatus::Idle | TwitchStatus::Connecting => {
            ("Twitch: connecting…".to_string(), 0xfacc15)
        }
        TwitchStatus::Connected { channel } => (format!("Twitch: connected ({channel})"), 0x22c55e),
        TwitchStatus::CredentialsMissing => ("Twitch: credentials missing".to_string(), 0xf97316),
        TwitchStatus::Error(message) => (format!("Twitch: {message}"), 0xf87171),
        TwitchStatus::Disconnected => ("Twitch: disconnected".to_string(), 0xfacc15),
    };

    let queued = state
        .messages
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                MessageStatus::Arrived
                    | MessageStatus::Transforming
                    | MessageStatus::Transformed
                    | MessageStatus::Synthesizing
                    | MessageStatus::Synthesized
                    | MessageStatus::Queued
            )
        })
        .count();
    let playing = state
        .messages
        .iter()
        .filter(|entry| matches!(entry.status, MessageStatus::Playing))
        .count();

    row![
        text(twitch_label),
        text(format!(
            "Queued: {queued} • Playing: {playing} • Total: {}",
            state.messages.len()
        )),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

fn render_message_entry<'a>(
    entry: &'a MessageEntry,
    active_playing_id: Option<u64>,
) -> Container<'a, AppMessage> {
    let status_text = entry.status_badge();
    let is_active_playback = active_playing_id == Some(entry.id);
    let timestamp = entry
        .received_at
        .format(&Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc().to_string());

    let mut header = row![
        text(format!("{} • {}", entry.provider.label(), entry.sender)),
        text(status_text),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    if is_active_playback {
        header = header.push(text("(playing)"));
    }

    container(
        column![
            header,
            text(entry.display_text.clone()),
            row![text(format!("#{}", entry.id)), text(timestamp)].spacing(8)
        ]
        .spacing(6),
    )
    .width(Length::Fill)
    .padding(10)
}

fn stop_confirmation(state: &AppState) -> Container<'_, AppMessage> {
    let stop_now = if state.busy {
        button(text("Stop now"))
    } else {
        button(text("Stop now")).on_press(AppMessage::Trigger(UiAction::StopNow))
    };

    container(
        column![
            text("Stop playback?"),
            row![
                button(text("Continue playback")).on_press(AppMessage::StopContinue),
                stop_now,
                button(text("Cancel")).on_press(AppMessage::StopCancel),
            ]
            .spacing(8)
        ]
        .spacing(8),
    )
    .padding(10)
}

fn quit_confirmation<'a>() -> Container<'a, AppMessage> {
    container(
        column![
            text("Messages are still queued or playing. Quit anyway?"),
            row![
                button(text("Quit anyway")).on_press(AppMessage::QuitAnyway),
                button(text("Stop before quitting")).on_press(AppMessage::QuitStopBefore),
                button(text("Don't quit")).on_press(AppMessage::QuitCancel),
            ]
            .spacing(8)
        ]
        .spacing(8),
    )
    .padding(10)
}
