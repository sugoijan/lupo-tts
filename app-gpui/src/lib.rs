// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    App, Application, Bounds, ClickEvent, Context, Entity, FontWeight, Global, KeyBinding, Render,
    SharedString, Subscription, Window, WindowBounds, WindowOptions, actions, div, px, rgb, size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{InputEvent, InputState, TextInput};
use gpui_component::scroll::{ScrollbarAxis, ScrollbarShow};
use gpui_component::{Disableable, Root, StyledExt, Theme};
use lupo_models::messages;
use lupo_models::prelude::*;
use sea_orm::{ActiveValue, DatabaseConnection, EntityTrait, QueryOrder};
use tokio::runtime::Runtime;

mod db;
mod error;
mod logging;

use error::Error;

struct AppGlobals {
    runtime: Arc<Runtime>,
    db: DatabaseConnection,
    audio: Arc<lupo_audio::Audio>,
    _log_guard: logging::LogGuard,
}

impl Global for AppGlobals {}

actions!(app, [QuitApp, CloseActiveWindow]);

#[derive(Clone, Debug)]
struct MessageRow {
    id: i32,
    content: String,
}

struct QueueView {
    input: Entity<InputState>,
    messages: Vec<MessageRow>,
    status: Option<SharedString>,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl QueueView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Manual message")
                .clean_on_escape()
        });

        let mut subscriptions = Vec::new();

        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, editor: &Entity<InputState>, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { secondary: false }) {
                    let value = editor.read(cx).value();
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        editor.update(cx, |state, cx| state.set_value("", window, cx));
                        this.enqueue_message(trimmed.to_string(), false, window, cx);
                    }
                }
            },
        );
        subscriptions.push(subscription);

        let mut view = Self {
            input,
            messages: Vec::new(),
            status: None,
            busy: false,
            _subscriptions: subscriptions,
        };

        view.refresh(window, cx);

        view
    }

    fn on_add_clicked(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value();
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.clear_input(window, cx);
        self.enqueue_message(trimmed, false, window, cx);
    }

    fn on_play_now_clicked(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value();
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.clear_input(window, cx);
        self.enqueue_message(trimmed, true, window, cx);
    }

    fn on_play_next_clicked(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.play_next(window, cx);
    }

    fn clear_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
    }

    fn enqueue_message(
        &mut self,
        message: String,
        play_now: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let globals = cx.global::<AppGlobals>();
        let runtime = globals.runtime.clone();
        let db = globals.db.clone();
        let audio = globals.audio.clone();
        let action_label = if play_now {
            "Sending message for immediate playback…"
        } else {
            "Queueing message…"
        };

        self.busy = true;
        self.status = Some(action_label.into());
        cx.notify();

        cx.spawn_in(window, async move |view, app| {
            let task = async move {
                queue_message(db.clone(), message.clone()).await?;
                if play_now {
                    play_next_message(db.clone(), audio.clone()).await?;
                }
                load_messages(db.clone()).await
            };

            let outcome = runtime
                .spawn(task)
                .await
                .map_err(join_error)
                .and_then(|res| res);

            if let Err(err) = app.update(|_window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| {
                        view.busy = false;
                        match outcome {
                            Ok(messages) => {
                                view.messages = messages;
                                view.status = if view.messages.is_empty() {
                                    Some("Queue is empty.".into())
                                } else {
                                    None
                                };
                            }
                            Err(err) => {
                                view.status = Some(format!("Error: {err}").into());
                            }
                        }
                        cx.notify();
                    });
                }
            }) {
                eprintln!("failed to update queue after enqueue: {err}");
            }
        })
        .detach();
    }

    fn play_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let globals = cx.global::<AppGlobals>();
        let runtime = globals.runtime.clone();
        let db = globals.db.clone();
        let audio = globals.audio.clone();

        self.busy = true;
        self.status = Some("Playing next message…".into());
        cx.notify();

        cx.spawn_in(window, async move |view, app| {
            let task = async move {
                let played = play_next_message(db.clone(), audio.clone()).await?;
                let messages = load_messages(db.clone()).await?;
                Ok((played, messages))
            };

            let outcome = runtime
                .spawn(task)
                .await
                .map_err(join_error)
                .and_then(|res| res);

            if let Err(err) = app.update(|_window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| {
                        view.busy = false;
                        match outcome {
                            Ok((played, messages)) => {
                                view.messages = messages;
                                view.status = played
                                    .map(|row| format!("Played: {}", row.content).into())
                                    .or_else(|| {
                                        if view.messages.is_empty() {
                                            Some("Queue is empty.".into())
                                        } else {
                                            None
                                        }
                                    });
                            }
                            Err(err) => {
                                view.status = Some(format!("Error: {err}").into());
                            }
                        }
                        cx.notify();
                    });
                }
            }) {
                eprintln!("failed to update queue after play-next: {err}");
            }
        })
        .detach();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let globals = cx.global::<AppGlobals>();
        let runtime = globals.runtime.clone();
        let db = globals.db.clone();

        self.busy = true;
        cx.notify();

        cx.spawn_in(window, async move |view, app| {
            let outcome = runtime
                .spawn(load_messages(db.clone()))
                .await
                .map_err(join_error)
                .and_then(|res| res);

            if let Err(err) = app.update(|_window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| {
                        view.busy = false;
                        match outcome {
                            Ok(messages) => {
                                view.messages = messages;
                                view.status = if view.messages.is_empty() {
                                    Some("Queue is empty.".into())
                                } else {
                                    None
                                };
                            }
                            Err(err) => {
                                view.status = Some(format!("Error: {err}").into());
                            }
                        }
                        cx.notify();
                    });
                }
            }) {
                eprintln!("failed to refresh queue: {err}");
            }
        })
        .detach();
    }
}

impl Render for QueueView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input_value = self.input.read(cx).value();
        let trimmed_empty = input_value.trim().is_empty();
        let disable_queue = self.busy || trimmed_empty;
        let disable_play_next = self.busy || self.messages.is_empty();

        let list = if self.messages.is_empty() {
            div()
                .flex()
                .items_center()
                .justify_center()
                .py_10()
                .text_color(rgb(0x94a3b8))
                .child("No queued messages yet.")
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .children(self.messages.iter().map(|row| {
                    div()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(0x1f2937))
                        .bg(rgb(0x111827))
                        .px_3()
                        .py_2()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xf8fafc))
                                .child(row.content.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x64748b))
                                .child(format!("#{}", row.id)),
                        )
                        .into_any_element()
                }))
                .into_any_element()
        };

        let status = self.status.clone().map(|msg| {
            div()
                .px_4()
                .pb_4()
                .text_sm()
                .text_color(rgb(0x93c5fd))
                .child(msg)
                .into_any_element()
        });

        div()
            .size_full()
            .bg(rgb(0x0f172a))
            .text_color(rgb(0xf8fafc))
            .child(
                div()
                    .px_4()
                    .pt_5()
                    .pb_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Message Queue"),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_sm()
                            .text_color(rgb(0x9ca3af))
                            .child(
                                "Add chat messages, stream synthesized speech through OpenAI, and play them immediately or later.",
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .px_4()
                    .pb_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x1f2937))
                            .bg(rgb(0x111827))
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child("Queued messages"),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .pb_3()
                                    .scrollable(ScrollbarAxis::Vertical)
                                    .max_h(px(260.0))
                                    .child(list),
                            ),
                    )
                    .child(
                        div()
                            .mt_4()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(
                                TextInput::new(&self.input)
                                    .cleanable()
                                    .flex_1(),
                            )
                            .child(
                                Button::new("queue-message")
                                    .primary()
                                    .label("Add to queue")
                                    .loading(self.busy)
                                    .disabled(disable_queue)
                                    .on_click(cx.listener(Self::on_add_clicked)),
                            )
                            .child(
                                Button::new("play-now")
                                    .info()
                                    .label("Play now")
                                    .loading(self.busy)
                                    .disabled(disable_queue)
                                    .on_click(cx.listener(Self::on_play_now_clicked)),
                            )
                            .child(
                                Button::new("play-next")
                                    .ghost()
                                    .label("Play next")
                                    .loading(self.busy)
                                    .disabled(disable_play_next)
                                    .on_click(cx.listener(Self::on_play_next_clicked)),
                            ),
                    ),
            )
            .when_some(status, |view, status| view.child(status))
    }
}

fn join_error(err: tokio::task::JoinError) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::Other, err))
}

async fn load_messages(db: DatabaseConnection) -> Result<Vec<MessageRow>, Error> {
    let rows = Messages::find()
        .order_by_desc(messages::Column::Id)
        .all(&db)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| MessageRow {
            id: row.id,
            content: row.content,
        })
        .collect())
}

async fn queue_message(db: DatabaseConnection, content: String) -> Result<(), Error> {
    let new_message = messages::ActiveModel {
        sender: ActiveValue::Set("[local]".into()),
        content: ActiveValue::Set(content),
        ..Default::default()
    };
    Messages::insert(new_message).exec(&db).await?;
    Ok(())
}

async fn play_next_message(
    db: DatabaseConnection,
    audio: Arc<lupo_audio::Audio>,
) -> Result<Option<MessageRow>, Error> {
    let message = Messages::find()
        .order_by_desc(messages::Column::Id)
        .one(&db)
        .await?;

    let Some(message) = message else {
        return Ok(None);
    };

    let api_key = std::env::var("OPENAI_API_KEY")?;
    let openai = lupo_tts_openai::OpenAi::new(api_key);
    let audio_bytes = openai.text_to_speech(message.content.clone()).await?;
    audio.play_wav(audio_bytes).await?;

    messages::Entity::delete_by_id(message.id).exec(&db).await?;

    Ok(Some(MessageRow {
        id: message.id,
        content: message.content,
    }))
}

pub fn run() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        Theme::global_mut(cx).scrollbar_show = ScrollbarShow::Hover;

        let dirs =
            directories::ProjectDirs::from("dev", "sugoijan", "LUPO TTS").expect("missing dirs");
        let log_guard = logging::setup_logging(dirs.log_dir()).expect("failed to set up logging");

        let runtime = Arc::new(Runtime::new().expect("failed to create tokio runtime"));
        let db = runtime
            .block_on(async { db::setup_db(dirs.data_local_dir()).await })
            .expect("failed to initialize database");
        let audio = Arc::new(
            lupo_audio::Audio::start_default().expect("failed to initialize audio output"),
        );

        cx.set_global(AppGlobals {
            runtime,
            db,
            audio,
            _log_guard: log_guard,
        });

        let bounds = Bounds::centered(None, size(px(720.0), px(560.0)), cx);

        cx.bind_keys([
            KeyBinding::new("cmd-q", QuitApp, None),
            KeyBinding::new("cmd-w", CloseActiveWindow, None),
        ]);

        cx.on_action(|_: &QuitApp, cx| cx.quit());
        cx.on_action(|_: &CloseActiveWindow, cx| {
            let window = cx
                .active_window()
                .or_else(|| cx.window_stack().and_then(|stack| stack.into_iter().next()))
                .or_else(|| cx.windows().into_iter().next());
            if let Some(window) = window {
                let _ = window.update(cx, |_, window, _| window.remove_window());
            }
        });

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| QueueView::new(window, cx));
                cx.new(|cx| Root::new(view.into(), window, cx))
            },
        )
        .expect("failed to open window");

        cx.activate(true);
    });
}
