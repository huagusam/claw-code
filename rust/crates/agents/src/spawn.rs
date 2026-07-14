use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;

use runtime::ConversationRuntime;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::persist::{
    mark_agent_running, persist_agent_terminal_state, unix_now, DEFAULT_AGENT_MAX_ITERATIONS,
};
use crate::runtime::{build_agent_runtime, ProviderRuntimeClient, SubagentToolExecutor};
use crate::types::{AgentJob, AgentStatus};

/// Handle to a spawned sub-agent. Lets the caller observe completion
/// or abort the task. Returned from [`spawn_agent_task`].
pub struct AgentHandle {
    agent_id: String,
    handle: JoinHandle<()>,
}

impl AgentHandle {
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
    pub fn abort(&self) {
        self.handle.abort();
    }
    pub async fn await_finished(self) -> Result<(), tokio::task::JoinError> {
        self.handle.await.map(|_| ())
    }

    /// Build a no-op handle for tests that exercise the spawn plumbing
    /// but do not need to drive a real agent task. The handle's
    /// underlying task is a `ready(())` future, so `is_finished` will
    /// report `true` as soon as the shared runtime polls it.
    #[cfg(feature = "test-utils")]
    pub fn noop(agent_id: impl Into<String>) -> Self {
        let handle = shared_runtime().spawn(std::future::ready(()));
        Self {
            agent_id: agent_id.into(),
            handle,
        }
    }
}

fn shared_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Runtime::new().expect("failed to create shared tokio runtime for agents")
    })
}

/// Spawn an agent task on a **dedicated OS thread** so that the
/// `ProviderRuntimeClient::block_on()` call inside `run_agent_job`
/// does not panic with "Cannot start a runtime from within a runtime".
///
/// The previous approach used `shared_runtime().spawn(async …)`, which
/// ran the agent job on the shared runtime's thread pool.  Because
/// `ProviderRuntimeClient` owns its own `tokio::runtime::Runtime` and
/// calls `block_on` on it, the nested runtime context triggered the
/// panic.  Spawning on a plain thread avoids the nesting entirely —
/// the agent job is the sole owner of its `ProviderRuntimeClient`
/// runtime and can safely `block_on` it.
pub fn spawn_agent_task(job: AgentJob) -> Result<AgentHandle, String> {
    let agent_id = job.manifest.agent_id.clone();
    let agent_id_for_task = agent_id.clone();
    let manifest_for_persist = job.manifest.clone();

    // Spawn on a dedicated OS thread so ProviderRuntimeClient::block_on
    // does not collide with an existing tokio runtime context.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let shared = shared_runtime();
    let thread_handle = std::thread::spawn(move || {
        let job_for_catch = AssertUnwindSafe(job);
        let result = std::panic::catch_unwind(move || {
            run_agent_job_sync(&job_for_catch)
        });
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if let Err(e) = persist_agent_terminal_state(
                    &manifest_for_persist,
                    AgentStatus::Failed,
                    None,
                    Some(error),
                ) {
                    eprintln!(
                        "[agent] failed to persist Failed state for {agent_id_for_task}: {e}"
                    );
                }
            }
            Err(panic_payload) => {
                let panic_msg = panic_message(&panic_payload);
                if let Err(e) = persist_agent_terminal_state(
                    &manifest_for_persist,
                    AgentStatus::Failed,
                    None,
                    Some(format!("panic: {panic_msg}")),
                ) {
                    eprintln!(
                        "[agent] failed to persist panic state for {agent_id_for_task}: {e}"
                    );
                }
            }
        }
        let _ = tx.send(());
    });

    // Bridge the std::thread::JoinHandle to a tokio JoinHandle so
    // callers can still use is_finished / abort / await_finished.
    let handle = shared.spawn(async move {
        let _ = rx.recv();
        drop(thread_handle);
    });
    Ok(AgentHandle { agent_id, handle })
}

/// Synchronous entry point for the agent job.  Runs on its own OS
/// thread so `ProviderRuntimeClient::block_on` is safe.
fn run_agent_job_sync(job: &AgentJob) -> Result<(), String> {
    // Promote Created -> Running before doing any work. Best-effort;
    // a failure to write the manifest is not fatal.
    let _ = mark_agent_running(&job.manifest, unix_now());
    let mut runtime: ConversationRuntime<ProviderRuntimeClient, SubagentToolExecutor> =
        build_agent_runtime(job)?.with_max_iterations(DEFAULT_AGENT_MAX_ITERATIONS);
    let summary = runtime
        .run_turn(job.prompt.clone(), None)
        .map_err(|error| error.to_string())?;
    let final_text = final_assistant_text(&summary);
    persist_agent_terminal_state(
        &job.manifest,
        AgentStatus::Completed,
        Some(final_text.as_str()),
        None,
    )
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        String::from("unknown panic payload")
    }
}

fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    runtime::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}
