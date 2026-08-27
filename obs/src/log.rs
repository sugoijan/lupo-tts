// SPDX-License-Identifier: Apache-2.0

//! Route `tracing` events into OBS's `blog()` so plugin logs land in the OBS
//! log file and the Help → Log Files viewer.

use std::ffi::CString;
use std::fmt::Write as _;
use std::sync::Once;

use lupo_obs_sys as sys;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

const PREFIX: &std::ffi::CStr = c"[lupo] %s";

/// Install the global `tracing` subscriber. Idempotent: OBS never loads a module
/// twice in one process, but tests and reloads should not panic either.
pub fn install() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Without a filter every TRACE/DEBUG event from the dependency tree
        // (hyper, tungstenite, redb, …) would be formatted and shipped over
        // FFI just for OBS to discard it; keep DEBUG only for our own crates.
        let filter = filter_fn(|metadata| {
            *metadata.level() <= Level::INFO || metadata.target().starts_with("lupo")
        });
        let _ = tracing_subscriber::registry()
            .with(ObsLayer.with_filter(filter))
            .try_init();
    });
}

/// Map a `tracing` level onto OBS's `LOG_*` levels.
pub fn level_to_blog(level: &Level) -> i32 {
    match *level {
        Level::ERROR => sys::LOG_ERROR as i32,
        Level::WARN => sys::LOG_WARNING as i32,
        Level::INFO => sys::LOG_INFO as i32,
        Level::DEBUG | Level::TRACE => sys::LOG_DEBUG as i32,
    }
}

/// Write one line straight to `blog`, bypassing `tracing`. Interior NULs are
/// stripped rather than failing, since this is the path of last resort.
pub fn blog_raw(level: u32, message: &str) {
    let c_message = match CString::new(message) {
        Ok(c_message) => c_message,
        Err(_) => {
            let cleaned: String = message.chars().filter(|c| *c != '\0').collect();
            let Ok(c_message) = CString::new(cleaned) else {
                return;
            };
            c_message
        }
    };
    // SAFETY: `blog` is variadic `(int, const char *fmt, ...)`; we pass a fixed
    // "%s" format and one NUL-terminated string that outlives the call.
    unsafe { sys::blog(level as i32, PREFIX.as_ptr(), c_message.as_ptr()) };
}

struct ObsLayer;

impl<S: Subscriber> Layer<S> for ObsLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut body = String::new();
        let mut visitor = LineVisitor {
            line: &mut body,
            has_message: false,
            log_target: None,
        };
        event.record(&mut visitor);
        // Records bridged from the `log` crate carry their real target in a
        // `log.target` field; prefer it over the generic "log" target.
        let target = visitor.log_target.take();
        let line = format!("{}: {body}", target.as_deref().unwrap_or(metadata.target()));
        blog_raw(level_to_blog(metadata.level()) as u32, &line);
    }
}

/// Formats an event as `message key=value key=value`.
struct LineVisitor<'a> {
    line: &'a mut String,
    has_message: bool,
    /// `log.target` from `log`-bridged records.
    log_target: Option<String>,
}

impl Visit for LineVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name().starts_with("log.") {
            // log.module_path / log.file / log.line: noise for OBS logs.
            return;
        }
        if field.name() == "message" {
            if self.has_message {
                let _ = write!(self.line, " {value:?}");
            } else {
                // The message is recorded first by tracing's macros; keep it unquoted.
                let _ = write!(self.line, "{value:?}");
                self.has_message = true;
            }
        } else {
            let _ = write!(self.line, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "log.target" {
            self.log_target = Some(value.to_string());
            return;
        }
        if field.name().starts_with("log.") {
            return;
        }
        if field.name() == "message" {
            let _ = write!(self.line, "{value}");
            self.has_message = true;
        } else {
            let _ = write!(self.line, " {}={}", field.name(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_onto_obs_log_levels() {
        assert_eq!(level_to_blog(&Level::ERROR), 100);
        assert_eq!(level_to_blog(&Level::WARN), 200);
        assert_eq!(level_to_blog(&Level::INFO), 300);
        assert_eq!(level_to_blog(&Level::DEBUG), 400);
        assert_eq!(level_to_blog(&Level::TRACE), 400);
    }
}
