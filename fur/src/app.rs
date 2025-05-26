// SPDX-License-Identifier: Apache-2.0

use leptos::task::spawn_local;
use leptos::{ev::SubmitEvent, prelude::*};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

#[derive(Serialize, Deserialize)]
struct SpeakArgs<'a> {
    msg: &'a str,
}

#[component]
pub fn App() -> impl IntoView {
    let (msg, set_msg) = signal(String::new());
    let (speak_msg, set_speak_msg) = signal(String::new());

    let update_msg = move |ev| {
        let v = event_target_value(&ev);
        set_msg.set(v);
    };

    let speak = move |ev: SubmitEvent| {
        ev.prevent_default();
        spawn_local(async move {
            let msg = msg.get_untracked();
            if msg.is_empty() {
                return;
            }

            let args = serde_wasm_bindgen::to_value(&SpeakArgs { msg: &msg }).unwrap();
            // Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
            let new_msg = invoke("speak", args).await.as_string().unwrap();
            set_speak_msg.set(new_msg);
        });
    };

    view! {
        <main class="container">
            <h1>"Welcome to LUPO"</h1>
            <form class="row" on:submit=speak>
                <input id="speak-input" placeholder="Enter a message..." on:input=update_msg />
                <button type="submit">"Speak"</button>
            </form>
            <p>{ move || speak_msg.get() }</p>
        </main>
    }
}
