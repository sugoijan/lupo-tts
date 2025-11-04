// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use tokio::runtime::Runtime;

use super::logging::LogGuard;
use super::pipeline::MessagePipeline;
use lupo_configuration::SettingsService;

#[derive(Clone)]
pub struct AppServices {
    inner: Arc<AppServicesInner>,
}

struct AppServicesInner {
    runtime: Arc<Runtime>,
    settings: SettingsService,
    pipeline: Arc<MessagePipeline>,
    _log_guard: LogGuard,
}

impl AppServices {
    pub fn new(
        runtime: Arc<Runtime>,
        settings: SettingsService,
        pipeline: Arc<MessagePipeline>,
        log_guard: LogGuard,
    ) -> Self {
        Self {
            inner: Arc::new(AppServicesInner {
                runtime,
                settings,
                pipeline,
                _log_guard: log_guard,
            }),
        }
    }

    pub fn runtime(&self) -> Arc<Runtime> {
        self.inner.runtime.clone()
    }

    pub fn settings(&self) -> SettingsService {
        self.inner.settings.clone()
    }

    pub fn pipeline(&self) -> Arc<MessagePipeline> {
        self.inner.pipeline.clone()
    }
}
