//! A long-lived MCP server in a child process, driven with the rmcp client (track T4).
//! Findings, a working snippet, deadlines and the restart strategy: `docs/spikes/s1-rmcp.md`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use uals_core::backend::{BackendError, McpToolCaller, ToolOutput};

/// How to start an MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Short name for logs and errors: "semble", "claude-context".
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Literal environment variables.
    pub env: BTreeMap<String, String>,
    /// Names of variables copied from the current environment when set.
    pub pass_env: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// A running MCP server child process.
pub struct McpChild {
    spec: ChildSpec,
}

impl McpChild {
    /// Starts the server and completes the MCP handshake.
    pub async fn spawn(spec: ChildSpec) -> Result<Self, BackendError> {
        let _ = spec;
        todo!("track T4")
    }
}

#[async_trait]
impl McpToolCaller for McpChild {
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolOutput, BackendError> {
        let _ = (&self.spec, name, arguments);
        todo!("track T4")
    }
}
