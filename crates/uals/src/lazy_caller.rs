//! An MCP server that starts on its first call.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use tracing::{debug, warn};
use uals_core::backend::{BackendError, McpToolCaller, ToolOutput};

/// Starts the server. Called on the first call, and again after a failed start.
pub type Starter = Arc<dyn Fn() -> BoxFuture<'static, Result<Arc<dyn McpToolCaller>, BackendError>> + Send + Sync>;

/// A start in progress or finished, shared by every call that waits for it.
type Start = Shared<BoxFuture<'static, Result<Arc<dyn McpToolCaller>, BackendError>>>;

/// How long a failed start is reported before the next call tries again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Forwards tool calls to a server that is started on the first call.
///
/// The start runs on its own task, and concurrent first calls share it. A caller whose deadline
/// expires during the start does not cancel it, so a slow start still finishes for the next call.
/// A failed start is returned to every call for `retry_after`, then tried again. The server stops
/// when the `LazyCaller` is dropped.
pub struct LazyCaller {
    /// The server's name, for logs.
    name: String,
    starter: Starter,
    retry_after: Duration,
    /// The latest start and when it began; `None` until the first call.
    start: Mutex<Option<(Start, Instant)>>,
}

impl LazyCaller {
    /// A caller for the server `name` (used in logs) that retries a failed start after `RETRY_AFTER`.
    pub fn new(name: impl Into<String>, starter: Starter) -> Self {
        Self::with_retry_after(name, starter, RETRY_AFTER)
    }

    /// Like `new`, with the pause before a failed start is tried again set to `retry_after`.
    pub fn with_retry_after(name: impl Into<String>, starter: Starter, retry_after: Duration) -> Self {
        Self { name: name.into(), starter, retry_after, start: Mutex::new(None) }
    }

    /// The current start, or a new one when there is none yet or the last one failed long enough ago.
    fn current(&self) -> Start {
        let mut slot = self.start.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((start, started_at)) = slot.as_ref() {
            let failed = matches!(start.peek(), Some(Err(_)));
            if !failed || started_at.elapsed() < self.retry_after {
                return start.clone();
            }
            debug!(server = %self.name, "starting again after a failed start");
        }
        let start = self.spawn_start();
        *slot = Some((start.clone(), Instant::now()));
        start
    }

    /// Runs the starter on its own task and returns a future that every waiting call can share.
    fn spawn_start(&self) -> Start {
        let task = tokio::spawn((self.starter)());
        let name = self.name.clone();
        async move {
            let started = match task.await {
                Ok(started) => started,
                Err(error) => Err(BackendError::Failed(format!("the start task of {name} ended: {error}"))),
            };
            match &started {
                Ok(_) => debug!(server = %name, "started"),
                Err(error) => warn!(server = %name, %error, "start failed"),
            }
            started
        }
        .boxed()
        .shared()
    }
}

#[async_trait]
impl McpToolCaller for LazyCaller {
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolOutput, BackendError> {
        let server = self.current().await?;
        server.call_tool(name, arguments).await
    }
}
