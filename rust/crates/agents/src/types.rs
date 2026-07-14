use std::collections::BTreeSet;

use runtime::LaneEvent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "subagentType")]
    pub subagent_type: Option<String>,
    pub model: Option<String>,
    pub status: AgentStatus,
    #[serde(rename = "outputFile")]
    pub output_file: String,
    #[serde(rename = "manifestFile")]
    pub manifest_file: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(rename = "completedAt", skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    #[serde(rename = "laneEvents", default, skip_serializing_if = "Vec::is_empty")]
    pub lane_events: Vec<LaneEvent>,
    #[serde(rename = "currentBlocker", skip_serializing_if = "Option::is_none")]
    pub current_blocker: Option<runtime::LaneEventBlocker>,
    #[serde(rename = "derivedState")]
    pub derived_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Created,
    Running,
    Completed,
    Failed,
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

impl AgentStatus {
    /// String form used in human-readable terminal-state output sections.
    /// Created/Running collapse to "running" because the caller should
    /// never pass them to a terminal-state writer (the writer's debug
    /// assertion enforces that).
    pub fn as_terminal_str(self) -> &'static str {
        match self {
            Self::Created | Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentJob {
    pub manifest: AgentOutput,
    pub prompt: String,
    pub system_prompt: Vec<String>,
    pub allowed_tools: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentInput {
    pub description: String,
    pub prompt: String,
    pub subagent_type: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    /// Optional explicit system prompt (e.g. an `@agent` file's contents).
    /// When present, `execute_agent_with_spawn` uses it instead of deriving
    /// the prompt solely from `subagent_type` (which would drop the agent's
    /// own persona).
    #[serde(default)]
    pub system_prompt: Option<Vec<String>>,
    /// Optional allowed-tool allowlist. When present, overrides the tools
    /// inferred from `subagent_type`.
    #[serde(default)]
    pub allowed_tools: Option<BTreeSet<String>>,
}

#[derive(Debug, Deserialize)]
pub struct AgentGetInput {
    #[serde(rename = "agentId")]
    pub agent_id: String,
}

/// Lightweight status response (does NOT include full output content).
/// Caller should use Read tool on `output_file` if needed.
#[derive(Debug, Serialize)]
pub struct AgentGetOutput {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(rename = "completedAt", skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    #[serde(rename = "outputFile")]
    pub output_file: String,
    #[serde(rename = "manifestFile")]
    pub manifest_file: String,
}
