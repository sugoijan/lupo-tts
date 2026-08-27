// SPDX-License-Identifier: Apache-2.0

//! The LUPO runtime hosted inside OBS: one tokio runtime, one plugin-owned
//! database, one [`MessagePipeline`] per module, shared by every `lupo_tts`
//! source instance through [`ObsSink`], and the dock server every UI talks to.

use std::ffi::CStr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lupo_configuration::SettingsService;
use lupo_obs_sys as sys;
use lupo_persistence::DatabaseHandle;
use lupo_runtime::pipeline::{MessagePipeline, PipelineEvent, PipelineOptions};
use tokio::runtime::Runtime;
use tracing::{info, warn};

use crate::chat::ChatSupervisor;
use crate::sink::ObsSink;

pub struct Engine {
    /// Owned here so unload can shut it down; everyone else gets a `Handle`.
    runtime: Runtime,
    pipeline: Arc<MessagePipeline>,
    pub sink: Arc<ObsSink>,
    chat: Arc<ChatSupervisor>,
    /// URL of the dock page, if the server started.
    pub dock_url: Option<String>,
    /// `Some(reason)` when the dock could not be added natively.
    pub dock_manual_reason: Option<String>,
    settings: SettingsService,
    #[allow(dead_code)]
    db: DatabaseHandle,
    events_task: tokio::task::JoinHandle<()>,
}

static ENGINE: OnceLock<Mutex<Option<Arc<Engine>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<Engine>>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// The running engine, if `obs_module_post_load` managed to start it.
pub fn get() -> Option<Arc<Engine>> {
    slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine already running")]
    AlreadyRunning,
    #[error("libobs did not provide a module config path")]
    NoConfigPath,
    #[error("failed to create tokio runtime: {0}")]
    Runtime(std::io::Error),
    #[error("failed to open database at {path}: {source}")]
    Database {
        path: PathBuf,
        source: lupo_persistence::Error,
    },
}

/// Resolve paths from the current module and start the engine.
pub fn start_for_module() -> Result<(), EngineError> {
    // SAFETY: obs_current_module is our own export; libobs owns the returned
    // strings (config path is bmalloc'd and must be bfree'd).
    let (config_dir, helper_path, assets_dir) = unsafe {
        let module = crate::obs_current_module();
        let raw = sys::obs_module_get_config_path(module, c"".as_ptr());
        if raw.is_null() {
            return Err(EngineError::NoConfigPath);
        }
        let config_dir = PathBuf::from(CStr::from_ptr(raw).to_string_lossy().into_owned());
        sys::bfree(raw.cast());

        let binary = sys::obs_get_module_binary_path(module);
        let helper_path = if binary.is_null() {
            None
        } else {
            PathBuf::from(CStr::from_ptr(binary).to_string_lossy().into_owned())
                .parent()
                .map(|dir| dir.join(helper_binary_name()))
        };

        let data = sys::obs_get_module_data_path(module);
        let assets_dir = if data.is_null() {
            config_dir.join("dock-ui")
        } else {
            PathBuf::from(CStr::from_ptr(data).to_string_lossy().into_owned()).join("dock-ui")
        };
        (config_dir, helper_path, assets_dir)
    };
    start(config_dir, helper_path, assets_dir)
}

#[cfg(target_os = "macos")]
fn helper_binary_name() -> &'static str {
    lupo_tts_macos::HELPER_BINARY_NAME
}

#[cfg(not(target_os = "macos"))]
fn helper_binary_name() -> &'static str {
    ""
}

pub fn start(
    config_dir: PathBuf,
    helper_path: Option<PathBuf>,
    assets_dir: PathBuf,
) -> Result<(), EngineError> {
    let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return Err(EngineError::AlreadyRunning);
    }

    lupo_runtime::install_crypto_provider();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("lupo-obs-tokio")
        .enable_all()
        .build()
        .map_err(EngineError::Runtime)?;
    let handle = runtime.handle().clone();

    let db = runtime
        .block_on(lupo_persistence::open_database_at_path(&config_dir))
        .map_err(|source| EngineError::Database {
            path: config_dir.clone(),
            source,
        })?;
    let settings = SettingsService::new(db.clone());
    let sink = Arc::new(ObsSink::default());

    let options = build_options(helper_path.as_deref());
    let pipeline = MessagePipeline::start_with_options(
        handle.clone(),
        db.clone(),
        sink.clone(),
        settings.clone(),
        options,
    );

    let events_task = runtime.spawn(log_pipeline_events(pipeline.subscribe()));
    let chat = Arc::new(ChatSupervisor::new(
        handle.clone(),
        settings.clone(),
        pipeline.clone(),
    ));
    chat.restart_listener();

    // The dock must be torn down at OBS_FRONTEND_EVENT_EXIT, not at module
    // unload (CEF is gone by then).
    // SAFETY: the callback is a plain function pointer; no private data.
    unsafe { sys::obs_frontend_add_event_callback(Some(on_frontend_event), std::ptr::null_mut()) };
    let (dock_url, dock_manual_reason) = match crate::dock::start(
        &handle,
        pipeline.clone(),
        chat.clone(),
        settings.clone(),
        &config_dir,
        assets_dir,
    ) {
        Ok(server) => (Some(server.url), server.manual_reason),
        Err(err) => {
            warn!(%err, "dock server not started");
            (None, Some(err.to_string()))
        }
    };

    info!(
        config_dir = %config_dir.display(),
        helper = helper_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        "engine started"
    );
    *guard = Some(Arc::new(Engine {
        runtime,
        pipeline,
        sink,
        chat,
        dock_url,
        dock_manual_reason,
        settings,
        db,
        events_task,
    }));
    Ok(())
}

#[cfg(target_os = "macos")]
fn build_options(helper_path: Option<&std::path::Path>) -> PipelineOptions {
    PipelineOptions {
        system_speech: Some(lupo_tts_macos::SystemSpeechProcessConfig {
            helper_path: helper_path.map(|p| p.to_path_buf()),
            ..Default::default()
        }),
        // The dock's volume slider is the plugin's only gain control; restore
        // it across OBS restarts.
        restore_output_volume: true,
    }
}

#[cfg(not(target_os = "macos"))]
fn build_options(_helper_path: Option<&std::path::Path>) -> PipelineOptions {
    PipelineOptions {
        restore_output_volume: true,
        ..Default::default()
    }
}

unsafe extern "C" fn on_frontend_event(
    event: sys::obs_frontend_event,
    _data: *mut std::ffi::c_void,
) {
    crate::ffi_guard("frontend_event", (), || {
        if event == sys::OBS_FRONTEND_EVENT_EXIT {
            info!("frontend exit; removing the native dock");
            crate::dock::remove_native_dock();
        }
    })
}

/// Mirror pipeline state changes into the OBS log. Per-message noise stays at
/// `debug` (filtered out of the OBS log): a busy chat would otherwise write
/// several formatted lines per message while OBS is encoding.
async fn log_pipeline_events(mut events: tokio::sync::broadcast::Receiver<PipelineEvent>) {
    use tracing::debug;
    loop {
        match events.recv().await {
            Ok(PipelineEvent::SnapshotUpdated(snapshot)) => {
                debug!(
                    message_id = snapshot.header.id,
                    state = ?snapshot.state_kind(),
                    "message state"
                );
            }
            Ok(PipelineEvent::PlaybackStateChanged(state)) => {
                debug!(enabled = state.enabled, active = ?state.active_message_id, "playback state");
            }
            Ok(PipelineEvent::VolumeChanged(volume)) => {
                info!(volume, "output volume changed");
            }
            Ok(PipelineEvent::ProviderStatusesChanged(statuses)) => {
                for status in statuses {
                    info!(
                        provider = status.provider_id,
                        state = ?status.state,
                        detail = status.detail.as_deref().unwrap_or(""),
                        "tts provider status"
                    );
                }
            }
            Ok(PipelineEvent::Control(control)) => info!(?control, "pipeline control event"),
            Ok(PipelineEvent::Cleared) => info!("queue cleared"),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "pipeline event stream lagged")
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Stop the engine: drop the pipeline, then shut the runtime down with a
/// timeout. Tasks parked on channels are dropped at once; blocking tasks get
/// up to the timeout. Never blocks OBS's exit indefinitely.
pub fn shutdown() {
    let Some(engine) = slot().lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let Ok(engine) = Arc::try_unwrap(engine) else {
        warn!("engine still referenced at unload; leaking it");
        return;
    };
    let Engine {
        runtime,
        pipeline,
        sink,
        chat,
        dock_url: _,
        dock_manual_reason: _,
        settings,
        db,
        events_task,
    } = engine;
    // SAFETY: matches the registration in `start`.
    unsafe {
        sys::obs_frontend_remove_event_callback(Some(on_frontend_event), std::ptr::null_mut())
    };
    // Normally already done on OBS_FRONTEND_EVENT_EXIT; harmless otherwise.
    crate::dock::remove_native_dock();
    chat.stop_listener();
    drop(chat);
    events_task.abort();
    drop(pipeline);
    drop(sink);
    drop(settings);
    runtime.shutdown_timeout(Duration::from_secs(3));
    drop(db);
}
