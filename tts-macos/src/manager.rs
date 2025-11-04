// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use thiserror::Error;
use tracing::warn;

use crate::protocol::{PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope};

pub const HELPER_PATH_ENV: &str = "LUPO_TTS_MACOS_HELPER_PATH";
pub const HELPER_BINARY_NAME: &str = "lupo-tts-macos-helper";

#[derive(Clone, Debug)]
pub struct SystemSpeechProcessConfig {
    pub helper_path: Option<PathBuf>,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for SystemSpeechProcessConfig {
    fn default() -> Self {
        Self {
            helper_path: None,
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(45),
            shutdown_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Error)]
pub enum SystemSpeechProcessError {
    #[error("helper binary not found; attempted paths: {attempted:?}")]
    HelperNotFound { attempted: Vec<PathBuf> },
    #[error("failed to spawn helper at {path}: {source}")]
    SpawnFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("IPC protocol error: {message}")]
    IpcProtocolError { message: String },
    #[error("helper exited unexpectedly (status: {status:?}) while {operation}")]
    HelperExited {
        status: Option<i32>,
        operation: &'static str,
    },
    #[error("timed out during {operation} after {timeout:?}")]
    Timeout {
        operation: &'static str,
        timeout: Duration,
    },
    #[error("synthesis failed ({code}): {message}")]
    SynthesisFailed { code: String, message: String },
    #[error("manager initialization failed: {message}")]
    ManagerInitialization { message: String },
    #[error("manager command channel is closed")]
    ManagerClosed,
}

pub type Result<T> = std::result::Result<T, SystemSpeechProcessError>;

#[derive(Clone)]
pub struct SystemSpeechProcessManager {
    inner: Arc<Inner>,
}

struct Inner {
    tx: Sender<WorkerCommand>,
    join_handle: Mutex<Option<JoinHandle<()>>>,
    config: SystemSpeechProcessConfig,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerCommand::ShutdownManager);
        if let Ok(mut handle) = self.join_handle.lock()
            && let Some(join) = handle.take()
        {
            let _ = join.join();
        }
    }
}

impl SystemSpeechProcessManager {
    pub fn new(config: SystemSpeechProcessConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<WorkerCommand>();
        let worker_config = config.clone();

        let join_handle = thread::Builder::new()
            .name("lupo-tts-macos-manager".to_string())
            .spawn(move || {
                let mut worker = Worker::new(worker_config);
                worker.run(rx);
            })
            .map_err(|err| SystemSpeechProcessError::ManagerInitialization {
                message: err.to_string(),
            })?;

        Ok(Self {
            inner: Arc::new(Inner {
                tx,
                join_handle: Mutex::new(Some(join_handle)),
                config,
            }),
        })
    }

    pub fn synthesize_to_wav(&self, text: &str) -> Result<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        self.inner
            .tx
            .send(WorkerCommand::Synthesize {
                text: text.to_string(),
                respond_to: tx,
            })
            .map_err(|_| SystemSpeechProcessError::ManagerClosed)?;
        rx.recv()
            .map_err(|_| SystemSpeechProcessError::ManagerClosed)?
    }

    pub fn restart(&self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.inner
            .tx
            .send(WorkerCommand::Restart { respond_to: tx })
            .map_err(|_| SystemSpeechProcessError::ManagerClosed)?;
        rx.recv()
            .map_err(|_| SystemSpeechProcessError::ManagerClosed)?
    }

    pub fn stop(&self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.inner
            .tx
            .send(WorkerCommand::Stop { respond_to: tx })
            .map_err(|_| SystemSpeechProcessError::ManagerClosed)?;
        rx.recv()
            .map_err(|_| SystemSpeechProcessError::ManagerClosed)?
    }

    pub fn resolve_helper_path(&self) -> Result<PathBuf> {
        resolve_helper_path(&self.inner.config)
    }
}

enum WorkerCommand {
    Synthesize {
        text: String,
        respond_to: Sender<Result<Vec<u8>>>,
    },
    Restart {
        respond_to: Sender<Result<()>>,
    },
    Stop {
        respond_to: Sender<Result<()>>,
    },
    ShutdownManager,
}

struct Worker {
    config: SystemSpeechProcessConfig,
    process: Option<HelperProcess>,
    next_request_id: u64,
}

impl Worker {
    fn new(config: SystemSpeechProcessConfig) -> Self {
        Self {
            config,
            process: None,
            next_request_id: 1,
        }
    }

    fn run(&mut self, rx: Receiver<WorkerCommand>) {
        while let Ok(command) = rx.recv() {
            match command {
                WorkerCommand::Synthesize { text, respond_to } => {
                    let result = self.synthesize(text);
                    let _ = respond_to.send(result);
                }
                WorkerCommand::Restart { respond_to } => {
                    let result = self.restart();
                    let _ = respond_to.send(result);
                }
                WorkerCommand::Stop { respond_to } => {
                    let result = self.stop();
                    let _ = respond_to.send(result);
                }
                WorkerCommand::ShutdownManager => {
                    let _ = self.stop_process(false);
                    return;
                }
            }
        }

        let _ = self.stop_process(false);
    }

    fn synthesize(&mut self, text: String) -> Result<Vec<u8>> {
        self.ensure_process_started()?;

        let request_id = self.take_request_id();
        let result = {
            let process = self.process.as_mut().expect("process should be started");
            process.send_request(&RequestEnvelope::Synthesize { request_id, text })?;
            let response = process.wait_for_response(
                Some(request_id),
                self.config.request_timeout,
                "synthesis response",
            )?;

            match response {
                ResponseEnvelope::SynthesizeOk {
                    request_id: response_id,
                    wav_base64,
                } => {
                    if response_id != request_id {
                        Err(SystemSpeechProcessError::IpcProtocolError {
                            message: format!(
                                "mismatched synthesis response id: expected {request_id}, got {response_id}"
                            ),
                        })
                    } else {
                        BASE64_STANDARD.decode(wav_base64).map_err(|err| {
                            SystemSpeechProcessError::IpcProtocolError {
                                message: format!("invalid base64 payload from helper: {err}"),
                            }
                        })
                    }
                }
                ResponseEnvelope::Error {
                    request_id: response_id,
                    code,
                    message,
                } => {
                    if response_id != Some(request_id) {
                        Err(SystemSpeechProcessError::IpcProtocolError {
                            message: format!(
                                "helper returned synthesis error for wrong request id: expected {request_id}, got {response_id:?}"
                            ),
                        })
                    } else {
                        Err(SystemSpeechProcessError::SynthesisFailed { code, message })
                    }
                }
                other => Err(SystemSpeechProcessError::IpcProtocolError {
                    message: format!("unexpected response during synthesis: {other:?}"),
                }),
            }
        };

        if let Err(err) = &result
            && is_fatal_transport_error(err)
        {
            self.discard_broken_process();
        }

        result
    }

    fn restart(&mut self) -> Result<()> {
        self.stop_process(false)?;
        self.ensure_process_started()
    }

    fn stop(&mut self) -> Result<()> {
        self.stop_process(true)
    }

    fn ensure_process_started(&mut self) -> Result<()> {
        if self.process.is_some() {
            return Ok(());
        }

        let mut process = HelperProcess::spawn(&self.config)?;
        process.send_request(&RequestEnvelope::Hello {
            protocol_version: PROTOCOL_VERSION,
        })?;

        let hello = process.wait_for_response(None, self.config.startup_timeout, "startup")?;
        match hello {
            ResponseEnvelope::HelloOk { protocol_version }
                if protocol_version == PROTOCOL_VERSION =>
            {
                self.process = Some(process);
                Ok(())
            }
            ResponseEnvelope::HelloOk { protocol_version } => {
                process.force_terminate();
                Err(SystemSpeechProcessError::IpcProtocolError {
                    message: format!(
                        "helper protocol version mismatch: expected {PROTOCOL_VERSION}, got {protocol_version}"
                    ),
                })
            }
            ResponseEnvelope::Error { code, message, .. } => {
                process.force_terminate();
                Err(SystemSpeechProcessError::IpcProtocolError {
                    message: format!("helper rejected startup handshake ({code}): {message}"),
                })
            }
            other => {
                process.force_terminate();
                Err(SystemSpeechProcessError::IpcProtocolError {
                    message: format!("unexpected helper startup response: {other:?}"),
                })
            }
        }
    }

    fn stop_process(&mut self, strict: bool) -> Result<()> {
        let Some(mut process) = self.process.take() else {
            return Ok(());
        };

        let request_id = self.take_request_id();
        let mut first_error = None;

        if let Err(err) = process.send_request(&RequestEnvelope::Shutdown { request_id }) {
            first_error = Some(err);
        } else {
            match process.wait_for_response(
                Some(request_id),
                self.config.shutdown_timeout,
                "shutdown",
            ) {
                Ok(ResponseEnvelope::ShutdownOk {
                    request_id: response_id,
                }) if response_id == request_id => {}
                Ok(ResponseEnvelope::Error { code, message, .. }) => {
                    first_error = Some(SystemSpeechProcessError::IpcProtocolError {
                        message: format!("helper rejected shutdown request ({code}): {message}"),
                    });
                }
                Ok(other) => {
                    first_error = Some(SystemSpeechProcessError::IpcProtocolError {
                        message: format!("unexpected helper shutdown response: {other:?}"),
                    });
                }
                Err(err) => {
                    first_error = Some(err);
                }
            }
        }

        if let Err(err) = process.wait_for_exit(self.config.shutdown_timeout)
            && first_error.is_none()
        {
            first_error = Some(err);
        }
        process.join_io_threads();

        if strict && let Some(err) = first_error {
            return Err(err);
        }

        Ok(())
    }

    fn discard_broken_process(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.force_terminate();
        }
    }

    fn take_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }
}

struct HelperProcess {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<HelperEvent>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl HelperProcess {
    fn spawn(config: &SystemSpeechProcessConfig) -> Result<Self> {
        let helper_path = resolve_helper_path(config)?;
        let mut child = Command::new(&helper_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| SystemSpeechProcessError::SpawnFailed {
                path: helper_path,
                source,
            })?;

        let stdin =
            child
                .stdin
                .take()
                .ok_or_else(|| SystemSpeechProcessError::IpcProtocolError {
                    message: "helper stdin pipe was not available".to_string(),
                })?;
        let stdout =
            child
                .stdout
                .take()
                .ok_or_else(|| SystemSpeechProcessError::IpcProtocolError {
                    message: "helper stdout pipe was not available".to_string(),
                })?;
        let stderr =
            child
                .stderr
                .take()
                .ok_or_else(|| SystemSpeechProcessError::IpcProtocolError {
                    message: "helper stderr pipe was not available".to_string(),
                })?;

        let (events_tx, events_rx) = mpsc::channel();

        let stdout_thread = match thread::Builder::new()
            .name("lupo-tts-macos-helper-stdout".to_string())
            .spawn(move || read_helper_stdout(stdout, events_tx))
        {
            Ok(handle) => handle,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SystemSpeechProcessError::ManagerInitialization {
                    message: format!("failed to spawn helper stdout reader: {err}"),
                });
            }
        };

        let stderr_thread = match thread::Builder::new()
            .name("lupo-tts-macos-helper-stderr".to_string())
            .spawn(move || read_helper_stderr(stderr))
        {
            Ok(handle) => handle,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                return Err(SystemSpeechProcessError::ManagerInitialization {
                    message: format!("failed to spawn helper stderr reader: {err}"),
                });
            }
        };

        Ok(Self {
            child,
            stdin,
            events: events_rx,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
        })
    }

    fn send_request(&mut self, request: &RequestEnvelope) -> Result<()> {
        let payload = serde_json::to_vec(request).map_err(|err| {
            SystemSpeechProcessError::IpcProtocolError {
                message: format!("failed to encode helper request: {err}"),
            }
        })?;

        self.stdin.write_all(&payload).map_err(|err| {
            SystemSpeechProcessError::IpcProtocolError {
                message: format!("failed to write helper request payload: {err}"),
            }
        })?;
        self.stdin
            .write_all(b"\n")
            .map_err(|err| SystemSpeechProcessError::IpcProtocolError {
                message: format!("failed to terminate helper request frame: {err}"),
            })?;
        self.stdin
            .flush()
            .map_err(|err| SystemSpeechProcessError::IpcProtocolError {
                message: format!("failed to flush helper request: {err}"),
            })?;

        Ok(())
    }

    fn wait_for_response(
        &mut self,
        expected_request_id: Option<u64>,
        timeout: Duration,
        operation: &'static str,
    ) -> Result<ResponseEnvelope> {
        let response = match self.events.recv_timeout(timeout) {
            Ok(HelperEvent::Response(response)) => response,
            Ok(HelperEvent::ReadError(message)) => {
                return Err(SystemSpeechProcessError::IpcProtocolError { message });
            }
            Ok(HelperEvent::Eof) => {
                return Err(SystemSpeechProcessError::HelperExited {
                    status: self
                        .child
                        .try_wait()
                        .ok()
                        .flatten()
                        .and_then(|status| status.code()),
                    operation,
                });
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(SystemSpeechProcessError::Timeout { timeout, operation });
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(SystemSpeechProcessError::HelperExited {
                    status: self
                        .child
                        .try_wait()
                        .ok()
                        .flatten()
                        .and_then(|status| status.code()),
                    operation,
                });
            }
        };

        if let Some(expected) = expected_request_id
            && !response_matches_request_id(&response, expected)
        {
            return Err(SystemSpeechProcessError::IpcProtocolError {
                message: format!(
                    "response request id mismatch for {operation}: expected {expected}, got {response:?}"
                ),
            });
        }

        Ok(response)
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;

        loop {
            match self.child.try_wait() {
                Ok(Some(_status)) => {
                    return Ok(());
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return Err(SystemSpeechProcessError::Timeout {
                            operation: "process exit",
                            timeout,
                        });
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => {
                    return Err(SystemSpeechProcessError::IpcProtocolError {
                        message: format!("failed while waiting for helper exit: {err}"),
                    });
                }
            }
        }
    }

    fn force_terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join_io_threads();
    }

    fn join_io_threads(&mut self) {
        if let Some(stdout_thread) = self.stdout_thread.take() {
            let _ = stdout_thread.join();
        }
        if let Some(stderr_thread) = self.stderr_thread.take() {
            let _ = stderr_thread.join();
        }
    }
}

enum HelperEvent {
    Response(ResponseEnvelope),
    ReadError(String),
    Eof,
}

fn read_helper_stdout(stdout: ChildStdout, tx: Sender<HelperEvent>) {
    let mut reader = BufReader::new(stdout);

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = tx.send(HelperEvent::Eof);
                return;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<ResponseEnvelope>(trimmed) {
                    Ok(response) => {
                        if tx.send(HelperEvent::Response(response)).is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(HelperEvent::ReadError(format!(
                            "failed to decode helper response: {err}; payload={trimmed}"
                        )));
                        return;
                    }
                }
            }
            Err(err) => {
                let _ = tx.send(HelperEvent::ReadError(format!(
                    "failed to read helper stdout: {err}"
                )));
                return;
            }
        }
    }
}

fn read_helper_stderr(stderr: ChildStderr) {
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        match line {
            Ok(content) => {
                warn!(target: "lupo_tts_macos_helper", "{content}");
            }
            Err(err) => {
                warn!(target: "lupo_tts_macos_helper", "failed to read helper stderr: {err}");
                return;
            }
        }
    }
}

fn is_fatal_transport_error(error: &SystemSpeechProcessError) -> bool {
    matches!(
        error,
        SystemSpeechProcessError::IpcProtocolError { .. }
            | SystemSpeechProcessError::HelperExited { .. }
            | SystemSpeechProcessError::Timeout { .. }
            | SystemSpeechProcessError::ManagerClosed
    )
}

fn response_matches_request_id(response: &ResponseEnvelope, expected: u64) -> bool {
    match response {
        ResponseEnvelope::SynthesizeOk { request_id, .. } => *request_id == expected,
        ResponseEnvelope::ShutdownOk { request_id } => *request_id == expected,
        ResponseEnvelope::Error { request_id, .. } => request_id.is_some_and(|id| id == expected),
        ResponseEnvelope::HelloOk { .. } => false,
    }
}

fn resolve_helper_path(config: &SystemSpeechProcessConfig) -> Result<PathBuf> {
    let current_exe =
        std::env::current_exe().map_err(|err| SystemSpeechProcessError::IpcProtocolError {
            message: format!("failed to resolve current executable path: {err}"),
        })?;

    resolve_helper_path_with_current_exe(config, &current_exe)
}

fn resolve_helper_path_with_current_exe(
    config: &SystemSpeechProcessConfig,
    current_exe: &Path,
) -> Result<PathBuf> {
    if let Some(path) = config.helper_path.clone() {
        return resolve_explicit_helper_path(path);
    }

    if let Some(path) = std::env::var_os(HELPER_PATH_ENV).map(PathBuf::from) {
        return resolve_explicit_helper_path(path);
    }

    let mut attempted = Vec::new();

    if let Some(path) = bundle_helper_path_for_executable(current_exe) {
        attempted.push(path.clone());
        if path.is_file() {
            return Ok(path);
        }
    }

    if let Some(path) = sibling_helper_path_for_executable(current_exe) {
        attempted.push(path.clone());
        if path.is_file() {
            return Ok(path);
        }
    }

    Err(SystemSpeechProcessError::HelperNotFound { attempted })
}

fn resolve_explicit_helper_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(SystemSpeechProcessError::HelperNotFound {
            attempted: vec![path],
        })
    }
}

fn bundle_helper_path_for_executable(current_exe: &Path) -> Option<PathBuf> {
    let mut cursor = current_exe;
    while let Some(parent) = cursor.parent() {
        if parent.extension() == Some(OsStr::new("app")) {
            return Some(parent.join("Contents/Helpers").join(HELPER_BINARY_NAME));
        }
        cursor = parent;
    }
    None
}

fn sibling_helper_path_for_executable(current_exe: &Path) -> Option<PathBuf> {
    current_exe.parent().map(|dir| dir.join(HELPER_BINARY_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::MutexGuard;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct ScopedHelperEnv {
        previous: Option<OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl ScopedHelperEnv {
        fn set(path: Option<&Path>) -> Self {
            let lock = ENV_LOCK.lock().expect("env lock should acquire");
            let previous = std::env::var_os(HELPER_PATH_ENV);
            unsafe {
                if let Some(path) = path {
                    std::env::set_var(HELPER_PATH_ENV, path);
                } else {
                    std::env::remove_var(HELPER_PATH_ENV);
                }
            }
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for ScopedHelperEnv {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = self.previous.as_ref() {
                    std::env::set_var(HELPER_PATH_ENV, value);
                } else {
                    std::env::remove_var(HELPER_PATH_ENV);
                }
            }
        }
    }

    #[test]
    fn bundle_helper_path_is_preferred_over_sibling_when_available() {
        let _env = ScopedHelperEnv::set(None);
        let temp = tempfile::tempdir().expect("tempdir should create");
        let app_root = temp.path().join("Lupo.app");
        let macos_dir = app_root.join("Contents/MacOS");
        let helpers_dir = app_root.join("Contents/Helpers");
        fs::create_dir_all(&macos_dir).expect("macos dir should create");
        fs::create_dir_all(&helpers_dir).expect("helpers dir should create");

        let executable = macos_dir.join("lupo");
        fs::write(&executable, b"binary").expect("executable placeholder should write");

        let bundle_helper = helpers_dir.join(HELPER_BINARY_NAME);
        fs::write(&bundle_helper, b"helper").expect("bundle helper should write");

        let sibling_helper = macos_dir.join(HELPER_BINARY_NAME);
        fs::write(&sibling_helper, b"sibling-helper").expect("sibling helper should write");

        let config = SystemSpeechProcessConfig::default();
        let resolved = resolve_helper_path_with_current_exe(&config, &executable)
            .expect("helper path should resolve");
        assert_eq!(resolved, bundle_helper);
    }

    #[test]
    fn sibling_helper_is_used_when_not_running_from_bundle() {
        let _env = ScopedHelperEnv::set(None);
        let temp = tempfile::tempdir().expect("tempdir should create");
        let bin_dir = temp.path().join("target/debug");
        fs::create_dir_all(&bin_dir).expect("bin dir should create");

        let executable = bin_dir.join("lupo");
        fs::write(&executable, b"binary").expect("executable placeholder should write");

        let sibling_helper = bin_dir.join(HELPER_BINARY_NAME);
        fs::write(&sibling_helper, b"helper").expect("sibling helper should write");

        let config = SystemSpeechProcessConfig::default();
        let resolved = resolve_helper_path_with_current_exe(&config, &executable)
            .expect("helper path should resolve");
        assert_eq!(resolved, sibling_helper);
    }

    #[test]
    fn env_override_is_strict_and_takes_priority() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let env_helper = temp.path().join("env-helper");
        fs::write(&env_helper, b"helper").expect("env helper should write");

        let executable = temp.path().join("lupo");
        fs::write(&executable, b"binary").expect("executable placeholder should write");

        let _env = ScopedHelperEnv::set(Some(&env_helper));

        let config = SystemSpeechProcessConfig::default();
        let resolved = resolve_helper_path_with_current_exe(&config, &executable)
            .expect("env helper should resolve");
        assert_eq!(resolved, env_helper);
    }

    #[test]
    fn manager_stop_is_noop_before_first_start() {
        let _env = ScopedHelperEnv::set(None);
        let manager = SystemSpeechProcessManager::new(SystemSpeechProcessConfig::default())
            .expect("manager should initialize");
        manager.stop().expect("stop should be no-op");
    }

    #[test]
    fn manager_reports_missing_explicit_helper_path() {
        let _env = ScopedHelperEnv::set(None);
        let temp = tempfile::tempdir().expect("tempdir should create");
        let missing_helper = temp.path().join("missing-helper");
        let manager = SystemSpeechProcessManager::new(SystemSpeechProcessConfig {
            helper_path: Some(missing_helper.clone()),
            ..SystemSpeechProcessConfig::default()
        })
        .expect("manager should initialize");

        let err = manager
            .synthesize_to_wav("hello")
            .expect_err("synthesis should fail");
        match err {
            SystemSpeechProcessError::HelperNotFound { attempted } => {
                assert_eq!(attempted, vec![missing_helper]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn manager_detects_protocol_mismatch_from_non_helper_binary() {
        let _env = ScopedHelperEnv::set(None);
        let helper_path = PathBuf::from("/bin/cat");
        if !helper_path.exists() {
            return;
        }

        let manager = SystemSpeechProcessManager::new(SystemSpeechProcessConfig {
            helper_path: Some(helper_path),
            ..SystemSpeechProcessConfig::default()
        })
        .expect("manager should initialize");

        let err = manager
            .synthesize_to_wav("hello")
            .expect_err("synthesis should fail");
        assert!(matches!(
            err,
            SystemSpeechProcessError::IpcProtocolError { .. }
        ));
    }
}
