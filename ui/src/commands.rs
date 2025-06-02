// SPDX-License-Identifier: Apache-2.0

use lupo_models::messages::Model as Message;
use serde_wasm_bindgen::from_value;
use tracing::*;
use wasm_bindgen::prelude::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("error when converting value")]
    Binding(#[from] serde_wasm_bindgen::Error),
}

#[wasm_bindgen]
extern "C" {
    // invoke without arguments
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str) -> JsValue;

    // invoke with arguments
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    async fn invoke_with(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn list_messages() -> Result<Vec<Message>, Error> {
    trace!("list_messages");
    let messages_js = invoke("list_messages").await;
    let messages: Vec<Message> = from_value(messages_js)?;
    Ok(messages)
}

pub async fn play() -> Result<(), Error> {
    trace!("play");
    invoke("play").await;
    Ok(())
}

pub async fn queue_message(message: &str) -> Result<bool, Error> {
    let args = {
        #[derive(serde::Serialize)]
        struct QueueMessageArgs<'a> {
            message: &'a str,
        }
        let queue_message_args = QueueMessageArgs { message };
        serde_wasm_bindgen::to_value(&queue_message_args).unwrap()
    };
    let queued = invoke_with("queue_message", args).await.as_bool().unwrap();
    Ok(queued)
}
