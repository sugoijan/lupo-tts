// SPDX-License-Identifier: Apache-2.0

use crate::Error;
use lupo_models::messages;
use lupo_models::messages::Model as Message;
use lupo_models::prelude::*;
use sea_orm::*;
use std::env;
use tauri::Emitter;
use tracing::*;

#[tauri::command]
pub async fn play(
    audio: tauri::State<'_, lupo_audio::Audio>,
    app_handle: tauri::AppHandle,
    db: tauri::State<'_, sea_orm::DatabaseConnection>,
) -> Result<(), Error> {
    info!("play");

    let message = if let Some(msg) = messages::Entity::find()
        .order_by_desc(<messages::Entity as sea_orm::EntityTrait>::Column::Id)
        .one(&*db)
        .await?
    {
        msg
    } else {
        debug!("no message to play");
        return Ok(());
    };

    debug!("message to speak: {}", message.content);

    // call speech API to get audio bytes
    let api_key = env::var("OPENAI_API_KEY")?;
    let openai = lupo_tts_openai::OpenAi::new(api_key);

    // play audio
    let audio_bytes = openai.text_to_speech(message.content).await?;
    audio.play_wav(audio_bytes).await?;

    // delete the message we just played
    messages::Entity::delete_by_id(message.id)
        .exec(&*db)
        .await?;

    // notify frontend of the update
    app_handle.emit("message-queue-updated", ())?;

    Ok(())
}

#[tauri::command]
pub async fn list_messages(
    db: tauri::State<'_, sea_orm::DatabaseConnection>,
) -> Result<Vec<Message>, Error> {
    info!("list_messages");
    Ok(Messages::find().all(&*db).await?)
}

#[tauri::command]
pub async fn queue_message(
    db: tauri::State<'_, sea_orm::DatabaseConnection>,
    app_handle: tauri::AppHandle,
    message: &str,
) -> Result<bool, Error> {
    info!("queue_message");

    let new_message = messages::ActiveModel {
        sender: ActiveValue::Set("[local]".into()),
        content: ActiveValue::Set(message.into()),
        ..Default::default()
    };
    let res = Messages::insert(new_message).exec(&*db).await?;
    debug!("insertion response: {:?}", res);
    app_handle.emit("message-queue-updated", ())?;

    Ok(true)
}
