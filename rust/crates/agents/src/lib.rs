//! Sub-agent subsystem.
//!
//! ## Lifecycle state machine
//!
//! Every sub-agent moves through a strict terminal state machine. The
//! `Created -> Running` transition is written by the caller (via
//! [`write_agent_manifest`]) before [`spawn_agent_task`] is invoked;
//! the agents crate writes the `Running` transition again on spawn
//! (defense in depth) and owns the `Running -> {Completed, Failed}`
//! transition.
//!
//! ```text
//!      caller
//!        │
//!        ▼
//!  ┌──────────────┐
//!  │   Created    │  created_at = now
//!  │   (written   │  status: Created
//!  │   by caller) │  started_at: None
//!  └──────┬───────┘
//!         │ spawn_agent_task()
//!         ▼
//!  ┌──────────────┐
//!  │   Running    │  status: Running, started_at = now
//!  │              │  mark_agent_running() called by spawn
//!  └──────┬───────┘
//!         │ job completes (success or failure)
//!         │ panic in run_agent_job() is caught and writes Failed
//!         ▼
//!  ┌──────────────┐
//!  │  Completed   │  status: Completed, completed_at = max(now, created_at)
//!  │              │  error: None
//!  └──────────────┘
//!  ┌──────────────┐
//!  │    Failed    │  status: Failed, completed_at = max(now, created_at)
//!  │              │  error: Some(msg)
//!  └──────────────┘
//! ```
//!
//! Invariants enforced by [`persist_agent_terminal_state`]:
//! - `status != AgentStatus::Running` (terminal-state only)
//! - `error.is_some() <=> status == AgentStatus::Failed`
//! - `completed_at >= created_at` (clock-jump floor)
//!
//! Manifest writes are crash-safe: every write goes through
//! `manifest.json.tmp` + `fsync` + atomic `rename`.

pub mod discovery;
mod normalize;
mod persist;
mod runtime;
mod spawn;
pub mod types;

pub use self::discovery::{
    definition_source_id, definition_source_json, render_agents_report,
    render_agents_report_json, AgentDiscovery, AgentSummary, DefinitionScope, DefinitionSource,
};
pub use self::normalize::{allowed_tools_for_subagent, normalize_subagent_type, SubagentKind};
pub use self::persist::{
    agent_store_dir, append_agent_output, derive_agent_state, extract_commit_sha,
    format_agent_terminal_output, make_agent_id, mark_agent_running,
    persist_agent_terminal_state, slugify_agent_name, unix_now, write_agent_manifest,
};
pub use self::runtime::{
    build_agent_runtime, build_agent_system_prompt, register_tool_executor, resolve_agent_model,
    ProviderRuntimeClient, SubagentToolExecutor,
};
pub use self::spawn::{spawn_agent_task, AgentHandle};
pub use self::types::{AgentGetInput, AgentGetOutput, AgentInput, AgentJob, AgentOutput, AgentStatus};
