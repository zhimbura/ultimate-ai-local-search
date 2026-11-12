//! `LazyCaller`: one start for concurrent first calls, starts that outlive impatient callers,
//! and failed starts that are retried after a pause.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use uals::lazy_caller::{LazyCaller, Starter};
use uals_core::backend::{BackendError, McpToolCaller, ToolOutput};

/// Answers every call with the tool name.
struct Echo;

#[async_trait]
impl McpToolCaller for Echo {
    async fn call_tool(&self, name: &str, _arguments: serde_json::Value) -> Result<ToolOutput, BackendError> {
        Ok(ToolOutput { texts: vec![name.to_owned()], structured: None, is_error: false })
    }
}

/// A starter that counts its starts, takes `delay`, and fails the first `failures` starts.
fn starter(starts: &Arc<AtomicUsize>, delay: Duration, failures: usize) -> Starter {
    let starts = Arc::clone(starts);
    Arc::new(move || {
        let starts = Arc::clone(&starts);
        async move {
            let attempt = starts.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            if attempt < failures {
                Err(BackendError::Unavailable(format!("start {attempt} failed")))
            } else {
                Ok(Arc::new(Echo) as Arc<dyn McpToolCaller>)
            }
        }
        .boxed()
    })
}

fn texts(result: Result<ToolOutput, BackendError>) -> Vec<String> {
    result.expect("the call succeeds").texts
}

#[tokio::test]
async fn concurrent_first_calls_share_one_start() {
    let starts = Arc::new(AtomicUsize::new(0));
    let caller = LazyCaller::new("echo", starter(&starts, Duration::from_millis(100), 0));

    let (a, b, c) = tokio::join!(
        caller.call_tool("a", json!({})),
        caller.call_tool("b", json!({})),
        caller.call_tool("c", json!({}))
    );

    assert_eq!([texts(a), texts(b), texts(c)], [["a"], ["b"], ["c"]]);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_caller_that_gives_up_does_not_cancel_the_start() {
    let starts = Arc::new(AtomicUsize::new(0));
    let caller = LazyCaller::new("echo", starter(&starts, Duration::from_millis(200), 0));

    let impatient = tokio::time::timeout(Duration::from_millis(50), caller.call_tool("early", json!({}))).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let later = tokio::time::timeout(Duration::from_millis(50), caller.call_tool("later", json!({}))).await;

    assert!(impatient.is_err(), "the first caller gave up during the start");
    assert_eq!(texts(later.expect("the started server answers at once")), ["later"]);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_start_is_reported_until_the_retry_pause_ends() {
    let starts = Arc::new(AtomicUsize::new(0));
    let caller = LazyCaller::with_retry_after("echo", starter(&starts, Duration::ZERO, 1), Duration::from_millis(200));

    let first = caller.call_tool("a", json!({})).await;
    let during_pause = caller.call_tool("b", json!({})).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after_pause = caller.call_tool("c", json!({})).await;

    let failed = Err(BackendError::Unavailable("start 0 failed".into()));
    assert_eq!((first, during_pause), (failed.clone(), failed));
    assert_eq!(texts(after_pause), ["c"]);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}
