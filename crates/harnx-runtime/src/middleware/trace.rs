/// Drop also covers engine cancellation while a host operation is awaiting I/O.
pub(super) struct RunTrace {
    middleware: String,
    finished: bool,
}

impl RunTrace {
    pub fn start(middleware: &str) -> Self {
        tracing::info!(middleware, "middleware started");
        Self {
            middleware: middleware.to_string(),
            finished: false,
        }
    }

    pub fn failure(&self, failure: &str) {
        let middleware = &self.middleware;
        tracing::warn!(%middleware, failure, "middleware failed");
    }

    pub fn finish(&mut self) {
        self.finished = true;
        tracing::info!(middleware = %self.middleware, "middleware finished");
    }
}

impl Drop for RunTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.failure("interrupted before middleware finished");
            self.finish();
        }
    }
}
