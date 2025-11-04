// SPDX-License-Identifier: Apache-2.0

use iced::widget::{container, text};
use iced::{Element, Length};

use crate::app::message::AppMessage;

pub fn view() -> Element<'static, AppMessage> {
    container(text("Chatter directory not yet implemented."))
        .width(Length::Fill)
        .padding(16)
        .into()
}
