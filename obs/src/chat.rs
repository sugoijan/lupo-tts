// SPDX-License-Identifier: Apache-2.0

//! Module-level Twitch chat supervision: one EventSub listener per OBS process,
//! restarted when the account or channel changes, plus the device-code login
//! flow, all driven over the dock protocol.

use std::sync::{Arc, Mutex};

use lupo_configuration::SettingsService;
use lupo_runtime::pipeline::MessagePipeline;
use lupo_runtime::twitch::{
    DeviceLogin, TwitchConnectorConfig, TwitchConnectorContext, TwitchEvent, TwitchStatus,
    clear_stored_credentials, load_auth_metadata, load_channel_override, normalize_channel_login,
    spawn_listener, store_channel_override,
};
use lupo_runtime::ui_protocol::{ChatView, LoginState, chat_connection};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

struct ListenerTasks {
    connector: JoinHandle<()>,
    forwarder: JoinHandle<()>,
}

pub struct ChatSupervisor {
    handle: Handle,
    settings: SettingsService,
    pipeline: Arc<MessagePipeline>,
    /// The dock-facing state; every change wakes the dock sessions watching it.
    view: watch::Sender<ChatView>,
    tasks: Mutex<Option<ListenerTasks>>,
    /// The in-flight device-code login, aborted on logout so a pending
    /// approval cannot re-link the account afterwards.
    login_task: Mutex<Option<JoinHandle<()>>>,
}

impl ChatSupervisor {
    pub fn new(handle: Handle, settings: SettingsService, pipeline: Arc<MessagePipeline>) -> Self {
        // Account and channel are cached here; this supervisor is the only
        // writer of both, so the cache stays true after construction.
        let initial = ChatView {
            account: load_auth_metadata(&settings)
                .ok()
                .flatten()
                .map(|state| state.login_state.login),
            channel: load_channel_override(&settings),
            connection: chat_connection(&TwitchStatus::Idle),
            login: LoginState::Idle,
        };
        let (view, _) = watch::channel(initial);
        Self {
            handle,
            settings,
            pipeline,
            view,
            tasks: Mutex::new(None),
            login_task: Mutex::new(None),
        }
    }

    /// Current dock-facing state.
    pub fn view(&self) -> ChatView {
        self.view.borrow().clone()
    }

    /// Receiver that yields whenever the state changes.
    pub fn subscribe(&self) -> watch::Receiver<ChatView> {
        self.view.subscribe()
    }

    fn update(&self, apply: impl FnOnce(&mut ChatView)) {
        self.view.send_if_modified(|view| {
            let before = view.clone();
            apply(view);
            *view != before
        });
    }

    fn set_status(&self, status: TwitchStatus) {
        self.update(|view| view.connection = chat_connection(&status));
    }

    fn set_login(&self, login: LoginState) {
        self.update(|view| view.login = login);
    }

    /// Persist a new channel and restart the listener if it changed
    /// (`None` after normalization = the linked account's own channel).
    pub fn set_channel_login(self: &Arc<Self>, channel: &str) {
        let normalized = match normalize_channel_login(channel) {
            Ok(normalized) => normalized,
            Err(err) => {
                warn!(channel, %err, "ignoring invalid twitch channel");
                return;
            }
        };
        if self.view.borrow().channel == normalized {
            return;
        }
        match store_channel_override(&self.settings, normalized.as_deref()) {
            Ok(()) => {
                info!(
                    channel = normalized.as_deref().unwrap_or(""),
                    "twitch channel updated; restarting listener"
                );
                self.update(|view| view.channel = normalized.clone());
                self.restart_listener();
            }
            Err(err) => warn!(%err, "failed to persist twitch channel"),
        }
    }

    /// Start (or restart) the EventSub listener with the stored credentials.
    /// If no account is linked the listener reports `CredentialsMissing` and exits.
    pub fn restart_listener(self: &Arc<Self>) {
        self.stop_listener();
        let (tx, rx) = async_channel::unbounded::<TwitchEvent>();
        let context = TwitchConnectorContext {
            settings: self.settings.clone(),
            pipeline: self.pipeline.clone(),
        };
        let config = TwitchConnectorConfig {
            channel_login: self.view.borrow().channel.clone(),
        };
        let connector = spawn_listener(self.handle.clone(), context, tx, config);

        let this = self.clone();
        let forwarder = self.handle.spawn(async move {
            while let Ok(event) = rx.recv().await {
                match event {
                    TwitchEvent::Status(status) => {
                        info!(?status, "twitch status");
                        this.set_status(status);
                    }
                    TwitchEvent::Message(message) => {
                        // debug: one line per chat message would flood the OBS
                        // log on a busy stream.
                        debug!(preview = message.preview, "twitch chat message");
                    }
                }
            }
        });
        *self.tasks.lock().unwrap_or_else(|e| e.into_inner()) = Some(ListenerTasks {
            connector,
            forwarder,
        });
    }

    pub fn stop_listener(&self) {
        if let Some(tasks) = self.tasks.lock().unwrap_or_else(|e| e.into_inner()).take() {
            tasks.connector.abort();
            tasks.forwarder.abort();
            self.set_status(TwitchStatus::Idle);
        }
    }

    /// Kick off the device-code flow. Returns immediately; progress is
    /// published through the watched [`ChatView`].
    pub fn begin_login(self: &Arc<Self>, open_browser: bool) {
        if matches!(
            self.view.borrow().login,
            LoginState::Starting | LoginState::Pending { .. }
        ) {
            return;
        }
        self.set_login(LoginState::Starting);
        let this = self.clone();
        let task = self.handle.spawn(async move {
            let login = match DeviceLogin::begin().await {
                Ok(login) => login,
                Err(err) => {
                    warn!(%err, "twitch device login could not start");
                    this.set_login(LoginState::Failed {
                        message: err.to_string(),
                    });
                    return;
                }
            };
            let auth = &login.authorization;
            info!(
                user_code = auth.user_code,
                verification_uri = auth.verification_uri,
                "twitch device login started"
            );
            this.set_login(LoginState::Pending {
                user_code: auth.user_code.clone(),
                verification_uri: auth.verification_uri.clone(),
            });
            if open_browser {
                let url = auth
                    .verification_uri_complete
                    .clone()
                    .unwrap_or_else(|| auth.verification_uri.clone());
                if let Err(err) = webbrowser::open(&url) {
                    warn!(%err, url, "failed to open the browser for Twitch login");
                }
            }
            match login.complete(&this.settings).await {
                Ok(outcome) => {
                    info!(login = outcome.login_state.login, "twitch account linked");
                    this.update(|view| {
                        view.account = Some(outcome.login_state.login.clone());
                        view.login = LoginState::Idle;
                    });
                    this.restart_listener();
                }
                Err(err) => {
                    warn!(%err, "twitch device login failed");
                    this.set_login(LoginState::Failed {
                        message: err.to_string(),
                    });
                }
            }
        });
        *self.login_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
    }

    /// Forget the linked account and stop reading chat. Aborts a pending
    /// device-code login first: an approval landing after this point must not
    /// silently re-link the account.
    pub fn logout(&self) {
        if let Some(task) = self
            .login_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
        self.stop_listener();
        match clear_stored_credentials(&self.settings) {
            Ok(()) => info!("twitch credentials cleared"),
            Err(err) => warn!(%err, "failed to clear twitch credentials"),
        }
        self.update(|view| {
            view.account = None;
            view.connection = chat_connection(&TwitchStatus::CredentialsMissing);
            view.login = LoginState::Idle;
        });
    }
}
