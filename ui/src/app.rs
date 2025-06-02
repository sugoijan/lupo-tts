// SPDX-License-Identifier: Apache-2.0

use crate::commands;
use leptos::ev::{MouseEvent, SubmitEvent};
use leptos::prelude::*;
use leptos::task::spawn_local;
use lupo_models::messages::Model as Message;
use std::rc::Rc;
use tracing::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    // callback is a JS function that receives an Event object;
    //   `event.payload` is whatever we emitted from Rust.
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = listen)]
    async fn listen(event_name: &str, handler: &js_sys::Function) -> JsValue;
}

#[component]
pub fn App() -> impl IntoView {
    let (msg, set_msg) = signal(String::new());

    let update_msg = move |ev| {
        let v = event_target_value(&ev);
        trace!("update message: {:?}", v);
        set_msg.set(v);
    };

    let queue_message = move |ev: SubmitEvent| {
        ev.prevent_default();
        spawn_local(async move {
            let msg = msg.get_untracked();
            trace!("queue message: {:?}", msg);
            if msg.is_empty() {
                return;
            }

            set_msg.set("".to_string());

            let res = commands::queue_message(&msg).await;
            debug!("message queued? {:?}", res);
            // set_speak_msg.set(new_msg);
        });
    };

    let play_next = move |ev: MouseEvent| {
        ev.prevent_default();
        spawn_local(async move {
            if let Err(err) = commands::play().await {
                error!("could not play: {:?}", err);
            }
        });
    };

    let messages = RwSignal::new(Vec::<Message>::new());

    let refresh_messages: Rc<dyn Fn()> = {
        let messages = messages.clone();
        Rc::new(move || {
            let messages = messages.clone();
            spawn_local(async move {
                if let Ok(msgs) = commands::list_messages().await {
                    messages.set(msgs);
                }
            });
        })
    };

    // call once to load the initial list
    refresh_messages();

    // create listener to reload on every update
    let handler = Closure::wrap(Box::new(move |_: JsValue| {
        // Whenever "message-queue-updated" fires:
        refresh_messages();
    }) as Box<dyn FnMut(JsValue)>);

    spawn_local(async move {
        let _unlisten = listen("message-queue-updated", handler.as_ref().unchecked_ref()).await;
        handler.forget();
    });

    view! {
        <main class="container">
            <ul>
                <For
                    each=move || messages.get()
                    key=|msg| msg.clone()
                    children=move |msg| {
                        view! { <li><span>{msg.sender}: </span><span>{msg.content}</span></li> }
                    }
                />
            </ul>
            <form class="row" on:submit=queue_message>
                <input id="speak-input" autocomplete="off" placeholder="Enter a message..." on:input=update_msg />
                <button type="submit">"Add to queue"</button>
            </form>
            <button type="submit" on:click=play_next>"Play next"</button>
        </main>
    }
}
