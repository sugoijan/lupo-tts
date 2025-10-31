// SPDX-License-Identifier: Apache-2.0

use crate::db;
use crate::logging;
use crate::ui::*;
use lupo_models::messages;
use lupo_models::prelude::*;
use sea_orm::{ActiveValue, DatabaseConnection, EntityTrait, QueryOrder};
use slint::{ComponentHandle, VecModel};
use std::rc::Rc;
use std::sync::Arc;
use tracing::*;

pub struct State {
    pub main_window: MainWindow,
    _audio_player: Arc<lupo_audio::Audio>,
    _dirs: directories::ProjectDirs,
    _log_guard: logging::LogGuard,
    _db: DatabaseConnection,
    _runtime: tokio::runtime::Runtime,
}

impl State {
    pub fn new() -> Self {
        let dirs = directories::ProjectDirs::from("dev", "sugoijan", "LUPO TTS").unwrap();
        let log_guard = logging::setup_logging(dirs.log_dir()).unwrap();

        let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");

        let db = runtime
            .block_on(async { db::setup_db(dirs.data_local_dir()).await })
            .expect("failed to initialize database");

        let audio_player = Arc::new(
            lupo_audio::Audio::start_default().expect("failed to initialize audio output"),
        );

        let initial_messages = runtime
            .block_on(async { load_messages(&db).await })
            .expect("failed to load queued messages");

        debug!("build state");

        let main_window = MainWindow::new().unwrap();
        let weak_window = main_window.as_weak();

        let initial_items = initial_messages
            .into_iter()
            .map(MessageItem::from)
            .collect::<Vec<_>>();
        let initial_model = Rc::new(VecModel::from(initial_items));
        main_window.set_todo_model(initial_model.into());

        let runtime_handle = runtime.handle().clone();

        main_window.on_todo_added({
            let db = db.clone();
            let weak_window = weak_window.clone();
            let handle = runtime_handle.clone();
            move |text| {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    return;
                }

                let db = db.clone();
                let weak_window = weak_window.clone();
                let handle = handle.clone();
                handle.spawn(async move {
                    if let Err(err) = queue_message(db.clone(), trimmed).await {
                        error!(?err, "failed to queue message");
                        return;
                    }
                    if let Err(err) = refresh_model(db, weak_window).await {
                        error!(?err, "failed to refresh queue after enqueue");
                    }
                });
            }
        });

        main_window.on_play_now({
            let db = db.clone();
            let audio = audio_player.clone();
            let weak_window = weak_window.clone();
            let handle = runtime_handle.clone();
            move |text| {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    return;
                }

                let db = db.clone();
                let audio = audio.clone();
                let weak_window = weak_window.clone();
                let handle = handle.clone();
                handle.spawn(async move {
                    if let Err(err) = queue_message(db.clone(), trimmed.clone()).await {
                        error!(?err, "failed to queue message for immediate playback");
                        return;
                    }
                    match play_next_message(db.clone(), audio.clone()).await {
                        Ok(Some(_)) => {
                            if let Err(err) = refresh_model(db, weak_window).await {
                                error!(?err, "failed to refresh queue after playback");
                            }
                        }
                        Ok(None) => {
                            warn!("queue unexpectedly empty after queuing '{}'", trimmed);
                            if let Err(err) = refresh_model(db, weak_window).await {
                                error!(?err, "failed to refresh queue after empty playback");
                            }
                        }
                        Err(err) => error!(?err, "failed to play message immediately"),
                    }
                });
            }
        });

        main_window.on_remove_done({
            let db = db.clone();
            let audio = audio_player.clone();
            let weak_window = weak_window.clone();
            let handle = runtime_handle.clone();
            move || {
                let db = db.clone();
                let audio = audio.clone();
                let weak_window = weak_window.clone();
                let handle = handle.clone();
                handle.spawn(async move {
                    match play_next_message(db.clone(), audio.clone()).await {
                        Ok(Some(_)) => {
                            if let Err(err) = refresh_model(db, weak_window).await {
                                error!(?err, "failed to refresh queue after playback");
                            }
                        }
                        Ok(None) => info!("no queued messages to play"),
                        Err(err) => error!(?err, "failed to play next queued message"),
                    }
                });
            }
        });

        main_window
            .window()
            .on_close_requested(|| slint::CloseRequestResponse::HideWindow);

        Self {
            main_window,
            _audio_player: audio_player,
            _dirs: dirs,
            _log_guard: log_guard,
            _db: db,
            _runtime: runtime,
        }
    }
}

#[derive(Clone, Debug)]
struct MessageRecord {
    id: i32,
    title: String,
}

impl From<MessageRecord> for MessageItem {
    fn from(record: MessageRecord) -> Self {
        Self {
            id: record.id,
            title: record.title.into(),
            state: ProcessingState::Added,
        }
    }
}

async fn load_messages(db: &DatabaseConnection) -> Result<Vec<MessageRecord>, crate::error::Error> {
    let rows = Messages::find()
        .order_by_desc(messages::Column::Id)
        .all(db)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| MessageRecord {
            id: row.id,
            title: row.content,
        })
        .collect())
}

async fn queue_message(db: DatabaseConnection, text: String) -> Result<(), crate::error::Error> {
    let new_message = messages::ActiveModel {
        sender: ActiveValue::Set("[local]".into()),
        content: ActiveValue::Set(text),
        ..Default::default()
    };

    Messages::insert(new_message).exec(&db).await?;
    Ok(())
}

async fn play_next_message(
    db: DatabaseConnection,
    audio: Arc<lupo_audio::Audio>,
) -> Result<Option<MessageRecord>, crate::error::Error> {
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

    Ok(Some(MessageRecord {
        id: message.id,
        title: message.content,
    }))
}

async fn refresh_model(
    db: DatabaseConnection,
    weak_window: slint::Weak<MainWindow>,
) -> Result<(), crate::error::Error> {
    let records = load_messages(&db).await?;

    if let Err(err) = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak_window.upgrade() {
            let items = records
                .into_iter()
                .map(MessageItem::from)
                .collect::<Vec<_>>();
            let model = Rc::new(VecModel::from(items));
            window.set_todo_model(model.into());
        }
    }) {
        warn!(?err, "failed to push queue update to UI");
    }

    Ok(())
}
