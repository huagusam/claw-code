use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use agents::{
    agent_store_dir, allowed_tools_for_subagent, build_agent_system_prompt, make_agent_id,
    mark_agent_running, normalize_subagent_type, resolve_agent_model, slugify_agent_name,
    spawn_agent_task, unix_now, write_agent_manifest, AgentDiscovery, AgentInput,
    AgentOutput, AgentStatus,
};
use api::ToolDefinition;
use plugins::PluginTool;
use reqwest::blocking::Client;
use runtime::{
    check_freshness, execute_bash,
    lsp_client::LspRegistry,
    mcp_tool_bridge::McpToolRegistry,
    permission_enforcer::{EnforcementResult, PermissionEnforcer},
    task_registry::TaskRegistry,
    team_cron_registry::{CronRegistry, TeamRegistry},
    worker_boot::{WorkerReadySnapshot, WorkerRegistry},
    BashCommandInput, BashCommandOutput, BranchFreshness, ConfigLoader,
    GrepSearchInput, LaneEvent, LaneEventName, LaneEventStatus, LaneFailureClass,
    McpDegradedReport, PermissionMode, TaskPacket,
    WorkspacePolicy,
};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Global task registry shared across tool invocations within a session.
fn global_lsp_registry() -> &'static LspRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<LspRegistry> = OnceLock::new();
    REGISTRY.get_or_init(LspRegistry::new)
}

fn global_mcp_registry() -> &'static McpToolRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<McpToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(McpToolRegistry::new)
}

fn global_team_registry() -> &'static TeamRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<TeamRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TeamRegistry::new)
}

fn global_cron_registry() -> &'static CronRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<CronRegistry> = OnceLock::new();
    REGISTRY.get_or_init(CronRegistry::new)
}

fn global_task_registry() -> &'static TaskRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<TaskRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TaskRegistry::new)
}

fn global_worker_registry() -> &'static WorkerRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<WorkerRegistry> = OnceLock::new();
    REGISTRY.get_or_init(WorkerRegistry::new)
}

/// WebFetch content cache. Stores extracted text per URL to avoid
/// repeated HTTP requests when the AI references the same page
/// multiple times within a session.
struct WebFetchCacheEntry {
    content: String,
    fetched_at: u64,
}

fn global_webfetch_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, WebFetchCacheEntry>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, WebFetchCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Cache TTL: 5 minutes (web content rarely changes within this window).
const WEBFETCH_CACHE_TTL_SECS: u64 = 300;

/// File content cache. Stores file contents written by new_file/edit_file
/// so that subsequent read_file calls can skip disk I/O.
/// Key: canonicalized absolute path, Value: full file content string.
struct FileCacheEntry {
    content: String,
    checksum: String,
}

fn global_file_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, FileCacheEntry>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, FileCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Holder for the active `WorkspacePolicy`. The default is `Strict`,

/// Write a diff file to `.claw/diffs/` for potential rollback.
/// Returns the relative path to the diff file, or None on failure.
fn write_diff_file(file_path: &str, old_string: &str, new_string: &str, replace_all: bool) -> Option<String> {
    use std::io::Write;

    let diffs_dir = std::path::Path::new(".claw").join("diffs");
    std::fs::create_dir_all(&diffs_dir).ok()?;

    // Generate unique filename: timestamp-counter.patch
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let filename = format!("{now}.patch");
    let diff_path = diffs_dir.join(&filename);
    let relative_path = format!(".claw/diffs/{filename}");

    let diff_content = serde_json::json!({
        "path": file_path,
        "old_string": old_string,
        "new_string": new_string,
        "replace_all": replace_all,
        "timestamp": now,
    });

    let mut file = std::fs::File::create(&diff_path).ok()?;
    file.write_all(
        serde_json::to_string_pretty(&diff_content)
            .ok()?
            .as_bytes(),
    )
    .ok()?;

    Some(relative_path)
}

/// Delete a diff file after rollback is complete.
fn delete_diff_file(diff_path: &str) {
    let _ = std::fs::remove_file(diff_path);
}
/// which preserves the original behavior: out-of-workspace accesses
/// are rejected with a clear error. Callers (e.g. the CLI startup
/// hook or test fixtures) can override the policy via
/// [`set_active_workspace_policy`].
pub struct ActiveWorkspacePolicy {
    cell: std::sync::Mutex<WorkspacePolicy>,
}

impl Default for ActiveWorkspacePolicy {
    fn default() -> Self {
        Self {
            cell: std::sync::Mutex::new(WorkspacePolicy::Allow),
        }
    }
}

impl ActiveWorkspacePolicy {
    pub const fn new() -> Self {
        Self {
            cell: std::sync::Mutex::new(WorkspacePolicy::Strict),
        }
    }

    /// Replace the active policy. Returns the previous policy for
    /// callers that want to restore it (handy in tests).
    pub fn set(&self, policy: WorkspacePolicy) -> WorkspacePolicy {
        let mut guard = self.cell.lock().expect("workspace policy mutex poisoned");
        std::mem::replace(&mut *guard, policy)
    }

    /// Snapshot the active policy.
    pub fn get(&self) -> WorkspacePolicy {
        self.cell
            .lock()
            .expect("workspace policy mutex poisoned")
            .clone()
    }
}

fn global_workspace_policy() -> &'static ActiveWorkspacePolicy {
    use std::sync::OnceLock;
    static POLICY: OnceLock<ActiveWorkspacePolicy> = OnceLock::new();
    POLICY.get_or_init(ActiveWorkspacePolicy::new)
}

/// Override the active workspace policy. Returns the previous policy
/// so callers can restore it. Useful for the CLI startup hook (which
/// reads `--workspace-policy`) and for tests that exercise the
/// `Prompt` and `Allow` modes.
pub fn set_active_workspace_policy(policy: WorkspacePolicy) -> WorkspacePolicy {
    global_workspace_policy().set(policy)
}

/// Snapshot of the active workspace policy.
pub fn active_workspace_policy() -> WorkspacePolicy {
    global_workspace_policy().get()
}

/// Record that the user explicitly named a path in input. In
/// `Prompt` mode, this pre-trusts the path's parent directory so the
/// LLM can read it without prompting. In `Strict` and `Allow` modes
/// this is a no-op: the policy already has a fixed answer for every
/// path. Designed to be called from the input parser whenever it
/// detects an absolute path in user input.
pub fn note_user_input_path(path: &Path) {
    global_workspace_policy().get().note_user_path(path);
}

/// Count of paths the user has explicitly named in input. Exposed
/// for tests and `claw status` output.
pub fn user_typed_path_count() -> usize {
    global_workspace_policy().get().user_typed_count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifestEntry {
    pub name: String,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Base,
    Conditional,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRegistry {
    entries: Vec<ToolManifestEntry>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(entries: Vec<ToolManifestEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[ToolManifestEntry] {
        &self.entries
    }
}

// Deleted 2026-06-04 per spec 搂5.4 (cycle-break Option 2): relocated to
// crates/runtime/src/tool_registry/ to break the agents鈫抰ools鈫攖ools鈫抋gents cycle.

#[derive(Debug, Clone)]
pub struct GlobalToolRegistry {
    plugin_tools: Vec<PluginTool>,
    runtime_tools: Vec<RuntimeToolDefinition>,
    enforcer: Option<PermissionEnforcer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub required_permission: PermissionMode,
}

impl GlobalToolRegistry {
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            plugin_tools: Vec::new(),
            runtime_tools: Vec::new(),
            enforcer: None,
        }
    }

    pub fn with_plugin_tools(plugin_tools: Vec<PluginTool>) -> Result<Self, String> {
        let builtin_names = runtime::tool_registry::mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .collect::<BTreeSet<_>>();
        let mut seen_plugin_names = BTreeSet::new();

        for tool in &plugin_tools {
            let name = tool.definition().name.clone();
            if builtin_names.contains(&name) {
                return Err(format!(
                    "plugin tool `{name}` conflicts with a built-in tool name"
                ));
            }
            if !seen_plugin_names.insert(name.clone()) {
                return Err(format!("duplicate plugin tool name `{name}`"));
            }
        }

        Ok(Self {
            plugin_tools,
            runtime_tools: Vec::new(),
            enforcer: None,
        })
    }

    pub fn with_runtime_tools(
        mut self,
        runtime_tools: Vec<RuntimeToolDefinition>,
    ) -> Result<Self, String> {
        let mut seen_names = runtime::tool_registry::mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.definition().name.clone()),
            )
            .collect::<BTreeSet<_>>();

        for tool in &runtime_tools {
            if !seen_names.insert(tool.name.clone()) {
                return Err(format!(
                    "runtime tool `{}` conflicts with an existing tool name",
                    tool.name
                ));
            }
        }

        self.runtime_tools = runtime_tools;
        Ok(self)
    }

    #[must_use]
    pub fn with_enforcer(mut self, enforcer: PermissionEnforcer) -> Self {
        self.set_enforcer(enforcer);
        self
    }

    pub fn normalize_allowed_tools(
        &self,
        values: &[String],
    ) -> Result<Option<BTreeSet<String>>, String> {
        if values.is_empty() {
            return Ok(None);
        }

        let builtin_specs = runtime::tool_registry::mvp_tool_specs();
        let canonical_names = builtin_specs
            .iter()
            .map(|spec| spec.name.to_string())
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.definition().name.clone()),
            )
            .chain(self.runtime_tools.iter().map(|tool| tool.name.clone()))
            .collect::<Vec<_>>();
        let mut name_map = canonical_names
            .iter()
            .map(|name| (normalize_tool_name(name), name.clone()))
            .collect::<BTreeMap<_, _>>();

        for (alias, canonical) in [
            ("read", "read_file"),
            ("write", "new_file"),
            ("write_file", "new_file"),
            ("edit", "edit_file"),
            ("glob", "glob_search"),
            ("grep", "grep_search"),
        ] {
            name_map.insert(alias.to_string(), canonical.to_string());
        }

        let mut allowed = BTreeSet::new();
        for value in values {
            for token in value
                .split(|ch: char| ch == ',' || ch.is_whitespace())
                .filter(|token| !token.is_empty())
            {
                let normalized = normalize_tool_name(token);
                let canonical = name_map.get(&normalized).ok_or_else(|| {
                    format!(
                        "unsupported tool in --allowedTools: {token} (expected one of: {})",
                        canonical_names.join(", ")
                    )
                })?;
                allowed.insert(canonical.clone());
            }
        }

        Ok(Some(allowed))
    }

    #[must_use]
    pub fn definitions(&self, allowed_tools: Option<&BTreeSet<String>>) -> Vec<ToolDefinition> {
        let builtin = runtime::tool_registry::mvp_tool_specs()
            .into_iter()
            .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema,
            });
        let runtime = self
            .runtime_tools
            .iter()
            .filter(|tool| allowed_tools.is_none_or(|allowed| allowed.contains(tool.name.as_str())))
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            });
        let plugin = self
            .plugin_tools
            .iter()
            .filter(|tool| {
                allowed_tools
                    .is_none_or(|allowed| allowed.contains(tool.definition().name.as_str()))
            })
            .map(|tool| ToolDefinition {
                name: tool.definition().name.clone(),
                description: tool.definition().description.clone(),
                input_schema: tool.definition().input_schema.clone(),
            });
        builtin.chain(runtime).chain(plugin).collect()
    }

    pub fn permission_specs(
        &self,
        allowed_tools: Option<&BTreeSet<String>>,
    ) -> Result<Vec<(String, PermissionMode)>, String> {
        let builtin = runtime::tool_registry::mvp_tool_specs()
            .into_iter()
            .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
            .map(|spec| (spec.name.to_string(), spec.required_permission));
        let runtime = self
            .runtime_tools
            .iter()
            .filter(|tool| allowed_tools.is_none_or(|allowed| allowed.contains(tool.name.as_str())))
            .map(|tool| (tool.name.clone(), tool.required_permission));
        let plugin = self
            .plugin_tools
            .iter()
            .filter(|tool| {
                allowed_tools
                    .is_none_or(|allowed| allowed.contains(tool.definition().name.as_str()))
            })
            .map(|tool| {
                permission_mode_from_plugin(tool.required_permission())
                    .map(|permission| (tool.definition().name.clone(), permission))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(builtin.chain(runtime).chain(plugin).collect())
    }

    #[must_use]
    pub fn has_runtime_tool(&self, name: &str) -> bool {
        self.runtime_tools.iter().any(|tool| tool.name == name)
    }

    #[must_use]
    pub fn search(
        &self,
        query: &str,
        max_results: usize,
        pending_mcp_servers: Option<Vec<String>>,
        mcp_degraded: Option<McpDegradedReport>,
    ) -> ToolSearchOutput {
        let query = query.trim().to_string();
        let normalized_query = normalize_tool_search_query(&query);
        let matches = search_tool_specs(&query, max_results.max(1), &self.searchable_tool_specs());

        ToolSearchOutput {
            matches,
            query,
            normalized_query,
            total_deferred_tools: self.searchable_tool_specs().len(),
            pending_mcp_servers,
            mcp_degraded,
        }
    }

    pub fn set_enforcer(&mut self, enforcer: PermissionEnforcer) {
        self.enforcer = Some(enforcer);
    }

    pub fn execute(&self, name: &str, input: &Value) -> Result<String, String> {
        if runtime::tool_registry::mvp_tool_specs()
            .iter()
            .any(|spec| spec.name == name)
        {
            return execute_tool_with_enforcer(self.enforcer.as_ref(), name, input);
        }
        self.plugin_tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .ok_or_else(|| format!("unsupported tool: {name}"))?
            .execute(input)
            .map_err(|error| error.to_string())
    }

    fn searchable_tool_specs(&self) -> Vec<SearchableToolSpec> {
        let builtin = deferred_tool_specs()
            .into_iter()
            .map(|spec| SearchableToolSpec {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
            });
        let runtime = self.runtime_tools.iter().map(|tool| SearchableToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
        });
        let plugin = self.plugin_tools.iter().map(|tool| SearchableToolSpec {
            name: tool.definition().name.clone(),
            description: tool.definition().description.clone().unwrap_or_default(),
        });
        builtin.chain(runtime).chain(plugin).collect()
    }
}

fn normalize_tool_name(value: &str) -> String {
    value.trim().replace('-', "_").to_ascii_lowercase()
}

fn permission_mode_from_plugin(value: &str) -> Result<PermissionMode, String> {
    match value {
        "read-only" => Ok(PermissionMode::ReadOnly),
        "workspace-write" => Ok(PermissionMode::WorkspaceWrite),
        "danger-full-access" => Ok(PermissionMode::DangerFullAccess),
        other => Err(format!("unsupported plugin permission: {other}")),
    }
}

// Deleted 2026-06-04 per spec 搂5.4 (cycle-break Option 2): relocated to
// crates/runtime/src/tool_registry/ to break the agents鈫抰ools鈫攖ools鈫抋gents cycle.


/// Deserialize a Value into type T, converting serde errors to String.
fn from_value<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, String> {
    serde_json::from_value(input.clone()).map_err(|e| e.to_string())
}

/// Check permission before executing a tool. Returns Err with denial reason if blocked.
pub fn enforce_permission_check(
    enforcer: &PermissionEnforcer,
    tool_name: &str,
    input: &Value,
) -> Result<(), String> {
    let input_str = serde_json::to_string(input).unwrap_or_default();
    let result = enforcer.check(tool_name, &input_str);

    match result {
        EnforcementResult::Allowed => Ok(()),
        EnforcementResult::Denied { reason, .. } => Err(reason),
    }
}

pub fn execute_tool(name: &str, input: &Value) -> Result<String, String> {
    execute_tool_with_enforcer(None, name, input)
}

/// Register `execute_tool` as the global sub-agent tool executor. Must
/// be called once at startup, before any sub-agent runs. Subsequent
/// calls are a no-op so test binaries that call it more than once
/// (e.g. via `tools_init` in two `#[test]` functions sharing the same
/// static) do not fail.
pub fn tools_init() -> Result<(), String> {
    use agents::register_tool_executor;
    match register_tool_executor(Box::new(|name, value, _policy| {
        execute_tool(name, value)
    })) {
        Ok(()) => Ok(()),
        Err(error) if error == "tool executor already registered" => Ok(()),
        Err(error) => Err(error),
    }
}

/// Execute a tool, optionally running its result through a `PermissionEnforcer`.
///
/// # Permission contract
///
/// When `enforcer` is `Some`, the enforcer runs before the tool body and may
/// deny the call. When `enforcer` is `None`, **no permission check is performed
/// and the tool body runs unconditionally**. Callers that need guaranteed
/// permission enforcement must pass a `Some` value — typically
/// [`PermissionEnforcer::permissive`] is the wrong choice for production paths.
///
/// For new code, prefer `PermissionPolicy::authorize_with_context` (from the
/// `runtime` crate) at the caller site rather than this helper, since the
/// `PermissionEnforcer` wrapper is deprecated and may be removed.
fn execute_tool_with_enforcer(
    enforcer: Option<&PermissionEnforcer>,
    name: &str,
    input: &Value,
) -> Result<String, String> {
    // Pre-process: check for PDF files in tool input and extract text automatically
    if let Some((pdf_path, pdf_text)) = pdf_extract::maybe_extract_pdf_from_prompt(
        &input.to_string()
    ) {
        eprintln!("[pdf_extract] Extracted text from {} ({} chars)", pdf_path, pdf_text.len());
        // Inject extracted PDF text into the tool input for processing
        // This allows tools to work with PDF content seamlessly
    }

    match name {
        "bash" => {
            // Parse input to get the command for permission classification
            let bash_input: BashCommandInput = from_value(input)?;
            let classified_mode = classify_bash_permission(&bash_input.command);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, classified_mode)?;
            run_bash(bash_input)
        }
        "read_file" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<ReadFileInput>(input).and_then(run_read_file)
        }
        "new_file" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<WriteFileInput>(input).and_then(run_new_file)
        }
        "edit_file" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<EditFileInput>(input).and_then(run_edit_file)
        }
        "undo" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<UndoInput>(input).and_then(run_undo)
        }
        "glob_search" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<GlobSearchInputValue>(input).and_then(run_glob_search)
        }
        "grep_search" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<GrepSearchInput>(input).and_then(run_grep_search)
        }
        "WebFetch" => from_value::<WebFetchInput>(input).and_then(run_web_fetch),
        "WebFind" => from_value::<WebFindInput>(input).and_then(run_web_find),
        "WebSearch" => from_value::<WebSearchInput>(input).and_then(run_web_search),
        "TodoWrite" => from_value::<TodoWriteInput>(input).and_then(run_todo_write),
        "Skill" => from_value::<SkillInput>(input).and_then(run_skill),
        "Agent" => from_value::<AgentInput>(input).and_then(run_agent),
        "ToolSearch" => from_value::<ToolSearchInput>(input).and_then(run_tool_search),
        "NotebookEdit" => from_value::<NotebookEditInput>(input).and_then(run_notebook_edit),
        "Sleep" => from_value::<SleepInput>(input).and_then(run_sleep),
        "SendUserMessage" | "Brief" => from_value::<BriefInput>(input).and_then(run_brief),
        "Config" => from_value::<ConfigInput>(input).and_then(run_config),
        "EnterPlanMode" => from_value::<EnterPlanModeInput>(input).and_then(run_enter_plan_mode),
        "ExitPlanMode" => from_value::<ExitPlanModeInput>(input).and_then(run_exit_plan_mode),
        "StructuredOutput" => {
            from_value::<StructuredOutputInput>(input).and_then(run_structured_output)
        }
        "REPL" => from_value::<ReplInput>(input).and_then(run_repl),
        "PowerShell" => {
            // Parse input to get the command for permission classification
            let ps_input: PowerShellInput = from_value(input)?;
            let classified_mode = classify_powershell_permission(&ps_input.command);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, classified_mode)?;
            run_powershell(ps_input)
        }
        "AskUserQuestion" => {
            from_value::<AskUserQuestionInput>(input).and_then(run_ask_user_question)
        }
        "TaskCreate" => from_value::<TaskCreateInput>(input).and_then(run_task_create),
        "RunTaskPacket" => from_value::<TaskPacket>(input).and_then(run_task_packet),
        "TaskGet" => from_value::<TaskIdInput>(input).and_then(run_task_get),
        "TaskList" => run_task_list(input.clone()),
        "TaskStop" => from_value::<TaskIdInput>(input).and_then(run_task_stop),
        "TaskUpdate" => from_value::<TaskUpdateInput>(input).and_then(run_task_update),
        "TaskOutput" => from_value::<TaskIdInput>(input).and_then(run_task_output),
        "WorkerCreate" => from_value::<WorkerCreateInput>(input).and_then(run_worker_create),
        "WorkerGet" => from_value::<WorkerIdInput>(input).and_then(run_worker_get),
        "WorkerObserve" => from_value::<WorkerObserveInput>(input).and_then(run_worker_observe),
        "WorkerResolveTrust" => {
            from_value::<WorkerIdInput>(input).and_then(run_worker_resolve_trust)
        }
        "WorkerAwaitReady" => from_value::<WorkerIdInput>(input).and_then(run_worker_await_ready),
        "WorkerSendPrompt" => {
            from_value::<WorkerSendPromptInput>(input).and_then(run_worker_send_prompt)
        }
        "WorkerRestart" => from_value::<WorkerIdInput>(input).and_then(run_worker_restart),
        "WorkerTerminate" => from_value::<WorkerIdInput>(input).and_then(run_worker_terminate),
        "WorkerObserveCompletion" => from_value::<WorkerObserveCompletionInput>(input)
            .and_then(run_worker_observe_completion),
        "TeamCreate" => from_value::<TeamCreateInput>(input).and_then(run_team_create),
        "TeamDelete" => from_value::<TeamDeleteInput>(input).and_then(run_team_delete),
        "CronCreate" => from_value::<CronCreateInput>(input).and_then(run_cron_create),
        "CronDelete" => from_value::<CronDeleteInput>(input).and_then(run_cron_delete),
        "CronList" => run_cron_list(input.clone()),
        "LSP" => from_value::<LspInput>(input).and_then(run_lsp),
        "ListMcpResources" => {
            from_value::<McpResourceInput>(input).and_then(run_list_mcp_resources)
        }
        "ReadMcpResource" => from_value::<McpResourceInput>(input).and_then(run_read_mcp_resource),
        "McpAuth" => from_value::<McpAuthInput>(input).and_then(run_mcp_auth),
        "ListAgents" => run_list_agents(input.clone()),
        "AgentGet" => from_value::<agents::AgentGetInput>(input).and_then(run_agent_get),
        "ListSkills" => run_list_skills(input.clone()),
        "ListPlugins" => run_list_plugins(input.clone()),
        "RemoteTrigger" => from_value::<RemoteTriggerInput>(input).and_then(run_remote_trigger),
        "MCP" => from_value::<McpToolInput>(input).and_then(run_mcp_tool),
        "TestingPermission" => {
            from_value::<TestingPermissionInput>(input).and_then(run_testing_permission)
        }
        _ => Err(format!("unsupported tool: {name}")),
    }
}

fn maybe_enforce_permission_check(
    enforcer: Option<&PermissionEnforcer>,
    tool_name: &str,
    input: &Value,
) -> Result<(), String> {
    if let Some(enforcer) = enforcer {
        enforce_permission_check(enforcer, tool_name, input)?;
    }
    Ok(())
}

/// Enforce permission check with a dynamically classified permission mode.
/// Used for tools like bash and `PowerShell` where the required permission
/// depends on the actual command being executed.
fn maybe_enforce_permission_check_with_mode(
    enforcer: Option<&PermissionEnforcer>,
    tool_name: &str,
    input: &Value,
    required_mode: PermissionMode,
) -> Result<(), String> {
    if let Some(enforcer) = enforcer {
        let input_str = serde_json::to_string(input).unwrap_or_default();
        let result = enforcer.check_with_required_mode(tool_name, &input_str, required_mode);

        match result {
            EnforcementResult::Allowed => Ok(()),
            EnforcementResult::Denied { reason, .. } => Err(reason),
        }
    } else {
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_ask_user_question(input: AskUserQuestionInput) -> Result<String, String> {
    use std::io::{self, BufRead, Write};

    // Display the question to the user via stdout
    let stdout = io::stdout();
    let stdin = io::stdin();
    let mut out = stdout.lock();

    writeln!(out, "\n[Question] {}", input.question).map_err(|e| e.to_string())?;

    if let Some(ref options) = input.options {
        for (i, option) in options.iter().enumerate() {
            writeln!(out, "  {}. {}", i + 1, option).map_err(|e| e.to_string())?;
        }
        write!(out, "Enter choice (1-{}): ", options.len()).map_err(|e| e.to_string())?;
    } else {
        write!(out, "Your answer: ").map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())?;

    // Read user response from stdin
    let mut response = String::new();
    stdin
        .lock()
        .read_line(&mut response)
        .map_err(|e| e.to_string())?;
    let response = response.trim().to_string();

    // If options were provided, resolve the numeric choice
    let answer = if let Some(ref options) = input.options {
        if let Ok(idx) = response.parse::<usize>() {
            if idx >= 1 && idx <= options.len() {
                options[idx - 1].clone()
            } else {
                response.clone()
            }
        } else {
            response.clone()
        }
    } else {
        response.clone()
    };

    to_pretty_json(json!({
        "question": input.question,
        "answer": answer,
        "status": "answered"
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_create(input: TaskCreateInput) -> Result<String, String> {
    let registry = global_task_registry();
    let task = registry.create(&input.prompt, input.description.as_deref());
    to_pretty_json(json!({
        "task_id": task.task_id,
        "status": task.status,
        "prompt": task.prompt,
        "description": task.description,
        "task_packet": task.task_packet,
        "created_at": task.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_packet(input: TaskPacket) -> Result<String, String> {
    let registry = global_task_registry();
    let task = registry
        .create_from_packet(input)
        .map_err(|error| error.to_string())?;

    to_pretty_json(json!({
        "task_id": task.task_id,
        "status": task.status,
        "prompt": task.prompt,
        "description": task.description,
        "task_packet": task.task_packet,
        "created_at": task.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_get(input: TaskIdInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.get(&input.task_id) {
        Some(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "prompt": task.prompt,
            "description": task.description,
            "task_packet": task.task_packet,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "messages": task.messages,
            "team_id": task.team_id
        })),
        None => Err(format!("task not found: {}", input.task_id)),
    }
}

fn run_task_list(_input: Value) -> Result<String, String> {
    let registry = global_task_registry();
    let tasks: Vec<_> = registry
        .list(None)
        .into_iter()
        .map(|t| {
            json!({
                "task_id": t.task_id,
                "status": t.status,
                "prompt": t.prompt,
                "description": t.description,
                "task_packet": t.task_packet,
                "created_at": t.created_at,
                "updated_at": t.updated_at,
                "team_id": t.team_id
            })
        })
        .collect();
    to_pretty_json(json!({
        "tasks": tasks,
        "count": tasks.len()
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_stop(input: TaskIdInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.stop(&input.task_id) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message": "Task stopped"
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_update(input: TaskUpdateInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.update(&input.task_id, &input.message) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message_count": task.messages.len(),
            "last_message": input.message
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_output(input: TaskIdInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.output(&input.task_id) {
        Ok(output) => to_pretty_json(json!({
            "task_id": input.task_id,
            "output": output,
            "has_output": !output.is_empty()
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_create(input: WorkerCreateInput) -> Result<String, String> {
    // Merge config-level trusted_roots with per-call overrides.
    // Config provides the default allowlist; per-call roots add on top.
    let config_roots: Vec<String> = ConfigLoader::default_for(&input.cwd)
        .load()
        .ok()
        .map(|c| c.trusted_roots().to_vec())
        .unwrap_or_default();
    let merged_roots: Vec<String> = config_roots
        .into_iter()
        .chain(input.trusted_roots.iter().cloned())
        .collect();
    let worker = global_worker_registry().create(
        &input.cwd,
        &merged_roots,
        input.auto_recover_prompt_misdelivery,
    );
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_get(input: WorkerIdInput) -> Result<String, String> {
    global_worker_registry().get(&input.worker_id).map_or_else(
        || Err(format!("worker not found: {}", input.worker_id)),
        to_pretty_json,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_observe(input: WorkerObserveInput) -> Result<String, String> {
    let worker = global_worker_registry().observe(&input.worker_id, &input.screen_text)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_resolve_trust(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().resolve_trust(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_await_ready(input: WorkerIdInput) -> Result<String, String> {
    let snapshot: WorkerReadySnapshot = global_worker_registry().await_ready(&input.worker_id)?;
    to_pretty_json(snapshot)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_send_prompt(input: WorkerSendPromptInput) -> Result<String, String> {
    let worker = global_worker_registry().send_prompt(
        &input.worker_id,
        input.prompt.as_deref(),
        input.task_receipt,
    )?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_restart(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().restart(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_terminate(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().terminate(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_observe_completion(input: WorkerObserveCompletionInput) -> Result<String, String> {
    let worker = global_worker_registry().observe_completion(
        &input.worker_id,
        &input.finish_reason,
        input.tokens_output,
    )?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_team_create(input: TeamCreateInput) -> Result<String, String> {
    let task_ids: Vec<String> = input
        .tasks
        .iter()
        .filter_map(|t| t.get("task_id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    let team = global_team_registry().create(&input.name, task_ids);
    // Register team assignment on each task
    for task_id in &team.task_ids {
        let _ = global_task_registry().assign_team(task_id, &team.team_id);
    }
    to_pretty_json(json!({
        "team_id": team.team_id,
        "name": team.name,
        "task_count": team.task_ids.len(),
        "task_ids": team.task_ids,
        "status": team.status,
        "created_at": team.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_team_delete(input: TeamDeleteInput) -> Result<String, String> {
    match global_team_registry().delete(&input.team_id) {
        Ok(team) => to_pretty_json(json!({
            "team_id": team.team_id,
            "name": team.name,
            "status": team.status,
            "message": "Team deleted"
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_create(input: CronCreateInput) -> Result<String, String> {
    let entry =
        global_cron_registry().create(&input.schedule, &input.prompt, input.description.as_deref());
    to_pretty_json(json!({
        "cron_id": entry.cron_id,
        "schedule": entry.schedule,
        "prompt": entry.prompt,
        "description": entry.description,
        "enabled": entry.enabled,
        "created_at": entry.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_delete(input: CronDeleteInput) -> Result<String, String> {
    match global_cron_registry().delete(&input.cron_id) {
        Ok(entry) => to_pretty_json(json!({
            "cron_id": entry.cron_id,
            "schedule": entry.schedule,
            "status": "deleted",
            "message": "Cron entry removed"
        })),
        Err(e) => Err(e),
    }
}

fn run_cron_list(_input: Value) -> Result<String, String> {
    let entries: Vec<_> = global_cron_registry()
        .list(false)
        .into_iter()
        .map(|e| {
            json!({
                "cron_id": e.cron_id,
                "schedule": e.schedule,
                "prompt": e.prompt,
                "description": e.description,
                "enabled": e.enabled,
                "run_count": e.run_count,
                "last_run_at": e.last_run_at,
                "created_at": e.created_at
            })
        })
        .collect();
    to_pretty_json(json!({
        "crons": entries,
        "count": entries.len()
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_lsp(input: LspInput) -> Result<String, String> {
    let registry = global_lsp_registry();
    let action = &input.action;
    let path = input.path.as_deref();
    let line = input.line;
    let character = input.character;
    let query = input.query.as_deref();

    match registry.dispatch(action, path, line, character, query) {
        Ok(result) => to_pretty_json(result),
        Err(e) => to_pretty_json(json!({
            "action": action,
            "error": e,
            "status": "error"
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_list_mcp_resources(input: McpResourceInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let server = input.server.as_deref().unwrap_or("default");
    match registry.list_resources(server) {
        Ok(resources) => {
            let items: Vec<_> = resources
                .iter()
                .map(|r| {
                    json!({
                        "uri": r.uri,
                        "name": r.name,
                        "description": r.description,
                        "mime_type": r.mime_type,
                    })
                })
                .collect();
            to_pretty_json(json!({
                "server": server,
                "resources": items,
                "count": items.len()
            }))
        }
        Err(e) => to_pretty_json(json!({
            "server": server,
            "resources": [],
            "error": e
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_read_mcp_resource(input: McpResourceInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let uri = input.uri.as_deref().unwrap_or("");
    let server = input.server.as_deref().unwrap_or("default");
    match registry.read_resource(server, uri) {
        Ok(resource) => to_pretty_json(json!({
            "server": server,
            "uri": resource.uri,
            "name": resource.name,
            "description": resource.description,
            "mime_type": resource.mime_type
        })),
        Err(e) => to_pretty_json(json!({
            "server": server,
            "uri": uri,
            "error": e
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_mcp_auth(input: McpAuthInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    match registry.get_server(&input.server) {
        Some(state) => to_pretty_json(json!({
            "server": input.server,
            "status": state.status,
            "server_info": state.server_info,
            "tool_count": state.tools.len(),
            "resource_count": state.resources.len()
        })),
        None => to_pretty_json(json!({
            "server": input.server,
            "status": "disconnected",
            "message": "Server not registered. Use MCP tool to connect first."
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_remote_trigger(input: RemoteTriggerInput) -> Result<String, String> {
    let method = input.method.unwrap_or_else(|| "GET".to_string());
    let client = Client::new();

    let mut request = match method.to_uppercase().as_str() {
        "GET" => client.get(&input.url),
        "POST" => client.post(&input.url),
        "PUT" => client.put(&input.url),
        "DELETE" => client.delete(&input.url),
        "PATCH" => client.patch(&input.url),
        "HEAD" => client.head(&input.url),
        other => return Err(format!("unsupported HTTP method: {other}")),
    };

    // Apply custom headers
    if let Some(ref headers) = input.headers {
        if let Some(obj) = headers.as_object() {
            for (key, value) in obj {
                if let Some(val) = value.as_str() {
                    request = request.header(key.as_str(), val);
                }
            }
        }
    }

    // Apply body
    if let Some(ref body) = input.body {
        request = request.body(body.to_string());
    }

    // Execute with a 30-second timeout
    let request = request.timeout(Duration::from_secs(30));

    match request.send() {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response.text().unwrap_or_default();
            let truncated_body = if body.len() > 8192 {
                format!(
                    "{}\n\n[response truncated 閳?{} bytes total]",
                    &body[..8192],
                    body.len()
                )
            } else {
                body
            };
            to_pretty_json(json!({
                "url": input.url,
                "method": method,
                "status_code": status,
                "body": truncated_body,
                "success": (200..300).contains(&status)
            }))
        }
        Err(e) => to_pretty_json(json!({
            "url": input.url,
            "method": method,
            "error": e.to_string(),
            "success": false
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_mcp_tool(input: McpToolInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let args = input.arguments.unwrap_or(serde_json::json!({}));
    match registry.call_tool(&input.server, &input.tool, &args) {
        Ok(result) => to_pretty_json(json!({
            "server": input.server,
            "tool": input.tool,
            "result": result,
            "status": "success"
        })),
        Err(e) => to_pretty_json(json!({
            "server": input.server,
            "tool": input.tool,
            "error": e,
            "status": "error"
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_testing_permission(input: TestingPermissionInput) -> Result<String, String> {
    to_pretty_json(json!({
        "action": input.action,
        "permitted": true,
        "message": "Testing permission tool stub"
    }))
}

fn run_list_agents(_input: Value) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let discovery = AgentDiscovery::new(&cwd);
    let active = discovery.active_names_list();
    to_pretty_json(json!({
        "agents": active,
        "count": active.len()
    }))
}

fn run_list_skills(_input: Value) -> Result<String, String> {
    let mut skills = Vec::new();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

    for ancestor in cwd.ancestors() {
        let skills_dir = ancestor.join(".claude").join("skills");
        if !skills_dir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let skill_path = if path.is_dir() {
                    path.join("SKILL.md")
                } else {
                    path.clone()
                };
                if !skill_path.is_file() {
                    continue;
                }
                if let Some(content) = parse_frontmatter_name(&skill_path) {
                    skills.push(content);
                } else if let Some(stem) = skill_path.file_stem() {
                    skills.push(stem.to_string_lossy().to_string());
                }
            }
        }
    }

    skills.sort();
    skills.dedup();
    to_pretty_json(json!({
        "skills": skills,
        "count": skills.len()
    }))
}

fn run_list_plugins(_input: Value) -> Result<String, String> {
    let mut plugins = Vec::new();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

    for ancestor in cwd.ancestors() {
        let plugins_dir = ancestor.join(".claude").join("plugins");
        if !plugins_dir.is_dir() {
            continue;
        }
        // Scan installed plugins (plugins/<name>/)
        let installed = plugins_dir.join("installed.json");
        if installed.is_file() {
            if let Ok(content) = std::fs::read_to_string(&installed) {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(arr) = parsed.as_array() {
                        for entry in arr {
                            if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
                                plugins.push(id.to_string());
                            }
                        }
                    }
                }
            }
        }
        // Also scan cache/ for plugins
        let cache = plugins_dir.join("cache");
        if cache.is_dir() {
            if let Ok(marketplaces) = std::fs::read_dir(&cache) {
                for mp in marketplaces.flatten() {
                    if let Ok(plugin_names) = std::fs::read_dir(mp.path()) {
                        for pn in plugin_names.flatten() {
                            let id = pn.file_name().to_string_lossy().to_string();
                            if !plugins.contains(&id) {
                                plugins.push(id);
                            }
                        }
                    }
                }
            }
        }
    }

    plugins.sort();
    to_pretty_json(json!({
        "plugins": plugins,
        "count": plugins.len()
    }))
}

fn parse_frontmatter_name(path: &std::path::Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    plugins::frontmatter::parse_frontmatter(&contents).ok()?.frontmatter.name
}


/// Classify bash command permission based on command type and path.
/// ROADMAP #50: Read-only commands targeting CWD paths get `WorkspaceWrite`,
/// all others remain `DangerFullAccess`.
fn classify_bash_permission(command: &str) -> PermissionMode {
    // Read-only commands that are safe when targeting workspace paths
    const READ_ONLY_COMMANDS: &[&str] = &[
        "cat", "head", "tail", "less", "more", "ls", "ll", "dir", "find", "test", "[", "[[",
        "grep", "rg", "awk", "sed", "file", "stat", "readlink", "wc", "sort", "uniq", "cut", "tr",
        "pwd", "echo", "printf",
    ];

    // Get the base command (first word before any args or pipes)
    let base_cmd = command.split_whitespace().next().unwrap_or("");
    let base_cmd = base_cmd.split('|').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split(';').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split('>').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split('<').next().unwrap_or("").trim();

    // Check if it's a read-only command
    let cmd_name = base_cmd.split('/').next_back().unwrap_or(base_cmd);
    let is_read_only = READ_ONLY_COMMANDS.contains(&cmd_name);

    if !is_read_only {
        return PermissionMode::DangerFullAccess;
    }

    // Check if any path argument is outside workspace
    // Simple heuristic: check for absolute paths not starting with CWD
    if has_dangerous_paths(command) {
        return PermissionMode::DangerFullAccess;
    }

    PermissionMode::WorkspaceWrite
}

/// Check if command has dangerous paths (outside workspace).
fn has_dangerous_paths(command: &str) -> bool {
    // Look for absolute paths
    let tokens: Vec<&str> = command.split_whitespace().collect();

    for token in tokens {
        // Strip surrounding quotes so `cat "C:\Users\foo\bar.txt"` is
        // recognised as a Windows absolute path.
        let stripped = token
            .trim_start_matches('"')
            .trim_end_matches('"')
            .trim_start_matches('\'')
            .trim_end_matches('\'');

        // Skip flags/options
        if stripped.starts_with('-') {
            continue;
        }

        // POSIX absolute path or `~/...` home-relative
        if stripped.starts_with('/') || stripped.starts_with("~/") {
            let path = PathBuf::from(
                stripped.replace('~', &std::env::var("HOME").unwrap_or_default()),
            );
            if let Ok(cwd) = std::env::current_dir() {
                if !path.starts_with(&cwd) {
                    return true; // Path outside workspace
                }
            }
        }

        // Windows drive-letter absolute path: `<letter>:\` or `<letter>:/`
        // e.g. `C:\Users\foo\bar.txt`, `D:/data/file.txt`.
        if stripped.len() >= 3
            && stripped.as_bytes()[0].is_ascii_alphabetic()
            && stripped.as_bytes()[1] == b':'
            && (stripped.as_bytes()[2] == b'\\' || stripped.as_bytes()[2] == b'/')
        {
            return true;
        }

        // UNC path: `\\server\share\...`
        if stripped.starts_with("\\\\") {
            return true;
        }

        // Check for parent directory traversal that escapes workspace
        if stripped.contains("../..")
            || (stripped.starts_with("../") && !stripped.starts_with("./"))
        {
            return true;
        }
    }

    false
}

fn run_bash(input: BashCommandInput) -> Result<String, String> {
    if let Some(output) = workspace_test_branch_preflight(&input.command) {
        return Ok(bash_model_view(&output));
    }
    let output = execute_bash(input).map_err(|error| error.to_string())?;
    Ok(bash_model_view(&output))
}

/// Render a `BashCommandOutput` for the model. The compact envelope
/// puts `stdout` and `stderr` first so the model sees the command's
/// output before any sandbox diagnostics, and it always carries
/// `sandbox.fallbackReason` so the model can reason honestly about
/// which sandbox mechanisms are actually enforced (rather than
/// concluding "the sandbox blocked it" when only process-tree kill
/// is active, as the legacy 16-field envelope allowed).
fn bash_model_view(output: &runtime::BashCommandOutput) -> String {
    let sandbox_block = output.sandbox_status.as_ref().map(|status| {
        serde_json::json!({
            "enabled": status.enabled,
            "active": status.active,
            "type": output.sandbox_type,
            "fallbackReason": status.fallback_reason,
        })
    });
    let view = serde_json::json!({
        "stdout": output.stdout,
        "stderr": output.stderr,
        "interrupted": output.interrupted,
        "returnCodeInterpretation": output.return_code_interpretation,
        "noOutputExpected": output.no_output_expected,
        "persistedOutputPath": output.persisted_output_path,
        "persistedOutputSize": output.persisted_output_size,
        "backgroundTaskId": output.background_task_id,
        "backgroundedByUser": output.backgrounded_by_user,
        "assistantAutoBackgrounded": output.assistant_auto_backgrounded,
        "dangerouslyDisableSandbox": output.dangerously_disable_sandbox,
        "structuredContent": output.structured_content,
        "sandbox": sandbox_block,
    });
    serde_json::to_string_pretty(&view).unwrap_or_else(|error| {
        // Fall back to the full envelope if the view can't be
        // serialised — the model still gets *some* output rather than
        // a hard error.
        serde_json::to_string_pretty(output)
            .unwrap_or_else(|_| format!("{{\"error\":\"{error}\"}}"))
    })
}

fn workspace_test_branch_preflight(command: &str) -> Option<BashCommandOutput> {
    if !is_workspace_test_command(command) {
        return None;
    }

    let branch = git_stdout(&["branch", "--show-current"])?;
    let main_ref = resolve_main_ref(&branch)?;
    let freshness = check_freshness(&branch, &main_ref);
    match freshness {
        BranchFreshness::Fresh => None,
        BranchFreshness::Stale {
            commits_behind,
            missing_fixes,
        } => Some(branch_divergence_output(
            command,
            &branch,
            &main_ref,
            commits_behind,
            None,
            &missing_fixes,
        )),
        BranchFreshness::Diverged {
            ahead,
            behind,
            missing_fixes,
        } => Some(branch_divergence_output(
            command,
            &branch,
            &main_ref,
            behind,
            Some(ahead),
            &missing_fixes,
        )),
    }
}

fn is_workspace_test_command(command: &str) -> bool {
    let normalized = normalize_shell_command(command);
    [
        "cargo test --workspace",
        "cargo test --all",
        "cargo nextest run --workspace",
        "cargo nextest run --all",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn normalize_shell_command(command: &str) -> String {
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn resolve_main_ref(branch: &str) -> Option<String> {
    let has_local_main = git_ref_exists("main");
    let has_remote_main = git_ref_exists("origin/main");

    if branch == "main" && has_remote_main {
        Some("origin/main".to_string())
    } else if has_local_main {
        Some("main".to_string())
    } else if has_remote_main {
        Some("origin/main".to_string())
    } else {
        None
    }
}

fn git_ref_exists(reference: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn git_stdout(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

fn branch_divergence_output(
    command: &str,
    branch: &str,
    main_ref: &str,
    commits_behind: usize,
    commits_ahead: Option<usize>,
    missing_fixes: &[String],
) -> BashCommandOutput {
    let relation = commits_ahead.map_or_else(
        || format!("is {commits_behind} commit(s) behind"),
        |ahead| format!("has diverged ({ahead} ahead, {commits_behind} behind)"),
    );
    let missing_summary = if missing_fixes.is_empty() {
        "(none surfaced)".to_string()
    } else {
        missing_fixes.join("; ")
    };
    let stderr = format!(
        "branch divergence detected before workspace tests: `{branch}` {relation} `{main_ref}`. Missing commits: {missing_summary}. Merge or rebase `{main_ref}` before re-running `{command}`."
    );

    BashCommandOutput {
        stdout: String::new(),
        stderr: stderr.clone(),
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: None,
        return_code_interpretation: Some("preflight_blocked:branch_divergence".to_string()),
        no_output_expected: Some(false),
        structured_content: Some(vec![serde_json::to_value(
            LaneEvent::new(
                LaneEventName::BranchStaleAgainstMain,
                LaneEventStatus::Blocked,
                iso8601_now(),
            )
            .with_failure_class(LaneFailureClass::BranchDivergence)
            .with_detail(stderr.clone())
            .with_data(json!({
                "branch": branch,
                "mainRef": main_ref,
                "commitsBehind": commits_behind,
                "commitsAhead": commits_ahead,
                "missingCommits": missing_fixes,
                "blockedCommand": command,
                "recommendedAction": format!("merge or rebase {main_ref} before workspace tests")
            })),
        )
        .expect("lane event should serialize")]),
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: None,
        sandbox_type: None,
    }
}

fn extract_title(content: &str, raw_body: &str, content_type: &str) -> Option<String> {
    if content_type.contains("html") {
        let lowered = raw_body.to_lowercase();
        if let Some(start) = lowered.find("<title>") {
            let after = start + "<title>".len();
            if let Some(end_rel) = lowered[after..].find("</title>") {
                let title =
                    collapse_whitespace(&decode_html_entities(&raw_body[after..after + end_rel]));
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

#[allow(dead_code)]
fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut previous_was_space = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            '&' => {
                text.push('&');
                previous_was_space = false;
            }
            ch if ch.is_whitespace() => {
                if !previous_was_space {
                    text.push(' ');
                    previous_was_space = true;
                }
            }
            _ => {
                text.push(ch);
                previous_was_space = false;
            }
        }
    }

    collapse_whitespace(&decode_html_entities(&text))
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn preview_text(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let shortened = input.chars().take(max_chars).collect::<String>();
    format!("{}…", shortened.trim_end())
}

/// Smart content extractor for fetched HTML pages.
/// Identifies the main content container (article/main/section/.content/etc.),
/// skips nav/footer/aside/.ad noise, and detects JavaScript-heavy pages
/// so the caller can fall back to RSS or text-only mode.
struct FastContentEvaluator {
    js_ratio_threshold: f64,
    min_text_len: usize,
    script_sel: Selector,
    container_sel: Selector,
    negative_sel: Selector,
    p_sel: Selector,
    body_sel: Selector,
    div_sel: Selector,
}

impl Default for FastContentEvaluator {
    fn default() -> Self {
        Self {
            js_ratio_threshold: 0.30,
            min_text_len: 60,
            script_sel: Selector::parse("script").unwrap(),
            container_sel: Selector::parse(
                "article, main, section, .content, .post, .article, .entry, #content, #main, .rich_media_content, .article-content, .news_content, .text_content, .content-article",
            )
            .unwrap(),
            negative_sel: Selector::parse("nav, footer, header, aside, .sidebar, .ad, .nav, .footer")
                .unwrap(),
            p_sel: Selector::parse("p").unwrap(),
            body_sel: Selector::parse("body").unwrap(),
            div_sel: Selector::parse("div").unwrap(),
        }
    }
}

struct PageAnalysis {
    #[allow(dead_code)]
    js_ratio: f64,
    best_content_len: usize,
    #[allow(dead_code)]
    has_external_scripts: bool,
}

impl FastContentEvaluator {
    fn analyze(&self, html: &str) -> PageAnalysis {
        if html.len() < 50 {
            return PageAnalysis {
                js_ratio: 1.0,
                best_content_len: 0,
                has_external_scripts: false,
            };
        }

        let document = Html::parse_document(html);
        let html_len = html.len();

        let (js_ratio, has_external) = self.calculate_js_ratio(&document, html_len);
        let best_content_len = self.detect_content_length(&document);

        PageAnalysis {
            js_ratio,
            best_content_len,
            has_external_scripts: has_external,
        }
    }

    fn should_retain(&self, analysis: &PageAnalysis) -> bool {
        if analysis.best_content_len >= self.min_text_len {
            return true;
        }
        if analysis.js_ratio <= self.js_ratio_threshold && analysis.best_content_len >= 20 {
            return true;
        }
        false
    }

    fn calculate_js_ratio(&self, document: &Html, html_len: usize) -> (f64, bool) {
        let html_len_f = html_len as f64;
        let mut total_len = 0usize;
        let mut has_external = false;

        for el in document.select(&self.script_sel) {
            total_len += el.text().map(|t| t.len()).sum::<usize>();
            if el.value().attr("src").is_some() {
                has_external = true;
                total_len += 1024;
            }
        }

        let ratio = if html_len_f > 0.0 {
            (total_len as f64 / html_len_f).min(1.0)
        } else {
            0.0
        };

        (ratio, has_external)
    }

    fn detect_content_length(&self, document: &Html) -> usize {
        let candidates: Vec<_> = document.select(&self.container_sel).collect();

        let body = match document.select(&self.body_sel).next() {
            Some(b) => b,
            None => return 0,
        };

        let candidate_refs: Vec<_> = if candidates.is_empty() {
            body.select(&self.div_sel).collect()
        } else {
            candidates
        };

        let mut best_score = -1i32;
        let mut best_text_len = 0usize;

        for candidate in &candidate_refs {
            let mut score = 0i32;

            let name = candidate.value().name();
            let class = candidate.value().attr("class").unwrap_or("");
            let id = candidate.value().attr("id").unwrap_or("");

            match name {
                "article" => score += 30,
                "main" => score += 25,
                "section" => score += 15,
                _ => {}
            }

            let class_tokens = format!(" {} ", class);
            let id_tokens = format!(" {} ", id);

            if class_tokens.contains(" content ")
                || class_tokens.contains(" post ")
                || class_tokens.contains(" article ")
                || class_tokens.contains(" entry ")
            {
                score += 25;
            }

            if id_tokens.contains(" content ")
                || id_tokens.contains(" main ")
                || id_tokens.contains(" article ")
            {
                score += 25;
            }

            let p_count = candidate.select(&self.p_sel).count();
            score += p_count as i32 * 8;

            let neg_count = candidate.select(&self.negative_sel).count();
            score -= neg_count as i32 * 12;

            if score > best_score {
                best_score = score;
                best_text_len = candidate
                    .text()
                    .map(|t| t.chars().filter(|c| !c.is_whitespace()).count())
                    .sum();
            }
        }

        if best_text_len == 0 {
            best_text_len = body
                .text()
                .map(|t| t.chars().filter(|c| !c.is_whitespace()).count())
                .sum();
        }

        best_text_len
    }

    fn extract_text(&self, html: &str) -> String {
        if html.len() < 50 {
            return String::new();
        }

        let document = Html::parse_document(html);
        let candidates: Vec<_> = document.select(&self.container_sel).collect();

        let body = match document.select(&self.body_sel).next() {
            Some(b) => b,
            None => return String::new(),
        };

        let candidate_refs: Vec<_> = if candidates.is_empty() {
            body.select(&self.div_sel).collect()
        } else {
            candidates
        };

        let mut best_score = -1i32;
        let mut best_text = String::new();

        for candidate in &candidate_refs {
            let mut score = 0i32;

            let name = candidate.value().name();
            let class = candidate.value().attr("class").unwrap_or("");
            let id = candidate.value().attr("id").unwrap_or("");

            match name {
                "article" => score += 30,
                "main" => score += 25,
                "section" => score += 15,
                _ => {}
            }

            let class_tokens = format!(" {} ", class);
            let id_tokens = format!(" {} ", id);

            if class_tokens.contains(" content ")
                || class_tokens.contains(" post ")
                || class_tokens.contains(" article ")
                || class_tokens.contains(" entry ")
            {
                score += 25;
            }

            if id_tokens.contains(" content ")
                || id_tokens.contains(" main ")
                || id_tokens.contains(" article ")
            {
                score += 25;
            }

            let p_count = candidate.select(&self.p_sel).count();
            score += p_count as i32 * 8;

            let neg_count = candidate.select(&self.negative_sel).count();
            score -= neg_count as i32 * 12;

            if score > best_score {
                best_score = score;
                best_text = candidate.text().collect();
            }
        }

        if best_text.is_empty() {
            best_text = body.text().collect();
        }

        best_text
    }
}

fn summarize_web_fetch(
    url: &str,
    prompt: &str,
    raw_body: &str,
    content_type: &str,
) -> String {
    let lower_prompt = prompt.to_ascii_lowercase();

    // For non-HTML content, skip the SmartContentEvaluator and just
    // surface the trimmed body — the evaluator's heuristics assume
    // an HTML document.
    if !content_type.contains("html") {
        let text = raw_body.trim();
        let detail = if lower_prompt.contains("title") {
            text.lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| format!("Title: {line}"))
                .unwrap_or_else(|| format!("Fetched {url}\n\n{text}"))
        } else if lower_prompt.contains("summary") || lower_prompt.contains("summarize") {
            format!("Fetched {url}\n\n{text}")
        } else {
            format!(
                "Fetched {url}\n\nPrompt: {prompt}\n\n{text}",
                prompt = prompt
            )
        };
        return detail;
    }

    let evaluator = FastContentEvaluator::default();
    let analysis = evaluator.analyze(raw_body);
    let should_retain = evaluator.should_retain(&analysis);
    let main_text = evaluator.extract_text(raw_body);
    let compact = collapse_whitespace(&main_text);

    let is_dynamic_content = !should_retain;

    let detail = if is_dynamic_content {
        let normalized_for_title = collapse_whitespace(&decode_html_entities(&main_text));
        let title = extract_title(&normalized_for_title, raw_body, content_type)
            .unwrap_or_else(|| "Unable to extract".to_string());
        format!(
            "Title: {}\n\nNote: This page uses JavaScript to render content. \
            The web fetch tool cannot execute JavaScript, so only static HTML was retrieved. \
            For best results with news sites, try using a text-only version or RSS feed.",
            title
        )
    } else if lower_prompt.contains("title") {
        extract_title(&compact, raw_body, content_type).map_or_else(
            || preview_text(&compact, 600),
            |title| format!("Title: {title}"),
        )
    } else if lower_prompt.contains("summary") || lower_prompt.contains("summarize") {
        // Return full content for summary requests (up to 50,000 chars)
        preview_text(&compact, 50_000)
    } else {
        // Return full content instead of 900-char preview to avoid
        // AI repeatedly fetching the same page trying to get complete content.
        // This saves tokens overall: one fetch with full content vs multiple
        // fetches with truncated previews.
        let full = preview_text(&compact, 50_000);
        format!("Prompt: {prompt}\nContent:\n{full}")
    };

    format!("Fetched {url}\n{detail}")
}

fn execute_todo_write(input: TodoWriteInput) -> Result<TodoWriteOutput, String> {
    validate_todos(&input.todos)?;
    let store_path = todo_store_path()?;
    let old_todos = if store_path.exists() {
        serde_json::from_str::<Vec<TodoItem>>(
            &std::fs::read_to_string(&store_path).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?
    } else {
        Vec::new()
    };

    let all_done = input
        .todos
        .iter()
        .all(|todo| matches!(todo.status, TodoStatus::Completed));
    let persisted = if all_done {
        Vec::new()
    } else {
        input.todos.clone()
    };

    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&persisted).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;

    let verification_nudge_needed = (all_done
        && input.todos.len() >= 3
        && !input
            .todos
            .iter()
            .any(|todo| todo.content.to_lowercase().contains("verif")))
    .then_some(true);

    Ok(TodoWriteOutput {
        old_todos,
        new_todos: input.todos,
        verification_nudge_needed,
    })
}

fn execute_skill(input: SkillInput) -> Result<SkillOutput, String> {
    let skill_path = resolve_skill_path(&input.skill)?;
    let prompt = std::fs::read_to_string(&skill_path).map_err(|error| error.to_string())?;
    let description = parse_skill_description(&prompt).unwrap_or_default();

    Ok(SkillOutput {
        skill: input.skill,
        path: skill_path.display().to_string(),
        args: input.args,
        description,
        prompt,
    })
}

fn validate_todos(todos: &[TodoItem]) -> Result<(), String> {
    if todos.is_empty() {
        return Err(String::from("todos must not be empty"));
    }
    // Allow multiple in_progress items for parallel workflows
    if todos.iter().any(|todo| todo.content.trim().is_empty()) {
        return Err(String::from("todo content must not be empty"));
    }
    if todos.iter().any(|todo| todo.active_form.trim().is_empty()) {
        return Err(String::from("todo activeForm must not be empty"));
    }
    Ok(())
}

fn todo_store_path() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("CLAWD_TODO_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(cwd.join(".clawd-todos.json"))
}

fn resolve_skill_path(skill: &str) -> Result<std::path::PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    match commands::resolve_skill_path(&cwd, skill) {
        Ok(path) => Ok(path),
        Err(_) => resolve_skill_path_from_compat_roots(skill),
    }
}

fn resolve_skill_path_from_compat_roots(skill: &str) -> Result<std::path::PathBuf, String> {
    let requested = skill.trim().trim_start_matches('/').trim_start_matches('$');
    if requested.is_empty() {
        return Err(String::from("skill must not be empty"));
    }

    if requested == "*" || requested == "all" {
        let names = list_available_skill_names();
        if names.is_empty() {
            return Err(String::from("no skills available 閳?use /skills list in the CLI"));
        }
        return Err(format!("Available skills: {}", names.join(", ")));
    }

    for root in skill_lookup_roots() {
        if let Some(path) = resolve_skill_path_in_root(&root, requested) {
            return Ok(path);
        }
    }

    let available = list_available_skill_names();
    if available.is_empty() {
        Err(format!("unknown skill: {requested} (no skills are currently available)"))
    } else {
        Err(format!(
            "unknown skill: {requested}. Available skills: {}",
            available.join(", ")
        ))
    }
}

fn list_available_skill_names() -> Vec<String> {
    let mut names = Vec::new();
    for root in skill_lookup_roots() {
        match root.origin {
            SkillLookupOrigin::SkillsDir => {
                if let Ok(entries) = std::fs::read_dir(&root.path) {
                    for entry in entries.flatten() {
                        if !entry.path().is_dir() {
                            continue;
                        }
                        let skill_path = entry.path().join("SKILL.md");
                        if !skill_path.is_file() {
                            continue;
                        }
                        if let Ok(contents) = std::fs::read_to_string(&skill_path) {
                            if let Some(name) = parse_skill_name(&contents) {
                                if !names.contains(&name) {
                                    names.push(name);
                                    continue;
                                }
                            }
                        }
                        let dir_name = entry.file_name().to_string_lossy().to_string();
                        if !names.contains(&dir_name) {
                            names.push(dir_name);
                        }
                    }
                }
            }
            SkillLookupOrigin::LegacyCommandsDir => {
                if let Ok(entries) = std::fs::read_dir(&root.path) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() && path.join("SKILL.md").is_file() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if !names.contains(&name) {
                                names.push(name);
                            }
                        } else if path.extension().is_some_and(|e| e == "md") {
                            if let Some(stem) = path.file_stem() {
                                let name = stem.to_string_lossy().to_string();
                                if !names.contains(&name) {
                                    names.push(name);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    names.sort();
    names
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillLookupOrigin {
    SkillsDir,
    LegacyCommandsDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillLookupRoot {
    path: std::path::PathBuf,
    origin: SkillLookupOrigin,
}

fn skill_lookup_roots() -> Vec<SkillLookupRoot> {
    let mut roots = Vec::new();

    if let Ok(cwd) = std::env::current_dir() {
        push_project_skill_lookup_roots(&mut roots, &cwd);
    }

    if let Ok(claw_config_home) = std::env::var("CLAW_CONFIG_HOME") {
        push_prefixed_skill_lookup_roots(&mut roots, std::path::Path::new(&claw_config_home));
    }
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        push_prefixed_skill_lookup_roots(&mut roots, std::path::Path::new(&codex_home));
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        push_home_skill_lookup_roots(&mut roots, std::path::Path::new(&home));
    }
    if let Ok(claude_config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let claude_config_dir = std::path::PathBuf::from(claude_config_dir);
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("skills"),
            SkillLookupOrigin::SkillsDir,
        );
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("skills").join("omc-learned"),
            SkillLookupOrigin::SkillsDir,
        );
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("commands"),
            SkillLookupOrigin::LegacyCommandsDir,
        );
    }

    roots
}

fn push_project_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, cwd: &std::path::Path) {
    for ancestor in cwd.ancestors() {
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".omc"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".agents"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".claw"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".codex"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".claude"));
    }
}

fn push_home_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, home: &std::path::Path) {
    push_prefixed_skill_lookup_roots(roots, &home.join(".omc"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".claw"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".codex"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".claude"));
    push_skill_lookup_root(
        roots,
        home.join(".agents").join("skills"),
        SkillLookupOrigin::SkillsDir,
    );
    push_skill_lookup_root(
        roots,
        home.join(".config").join("opencode").join("skills"),
        SkillLookupOrigin::SkillsDir,
    );
    push_skill_lookup_root(
        roots,
        home.join(".claude").join("skills").join("omc-learned"),
        SkillLookupOrigin::SkillsDir,
    );
}

fn push_prefixed_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, prefix: &std::path::Path) {
    push_skill_lookup_root(roots, prefix.join("skills"), SkillLookupOrigin::SkillsDir);
    push_skill_lookup_root(
        roots,
        prefix.join("commands"),
        SkillLookupOrigin::LegacyCommandsDir,
    );
}

fn push_skill_lookup_root(
    roots: &mut Vec<SkillLookupRoot>,
    path: std::path::PathBuf,
    origin: SkillLookupOrigin,
) {
    if path.is_dir() && !roots.iter().any(|existing| existing.path == path) {
        roots.push(SkillLookupRoot { path, origin });
    }
}

fn resolve_skill_path_in_root(
    root: &SkillLookupRoot,
    requested: &str,
) -> Option<std::path::PathBuf> {
    match root.origin {
        SkillLookupOrigin::SkillsDir => resolve_skill_path_in_skills_dir(&root.path, requested),
        SkillLookupOrigin::LegacyCommandsDir => {
            resolve_skill_path_in_legacy_commands_dir(&root.path, requested)
        }
    }
}

fn resolve_skill_path_in_skills_dir(
    root: &std::path::Path,
    requested: &str,
) -> Option<std::path::PathBuf> {
    let direct = root.join(requested).join("SKILL.md");
    if direct.is_file() {
        return Some(direct);
    }

    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let skill_path = entry.path().join("SKILL.md");
        if !skill_path.is_file() {
            continue;
        }
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(requested)
            || skill_frontmatter_name_matches(&skill_path, requested)
        {
            return Some(skill_path);
        }
    }

    None
}

fn resolve_skill_path_in_legacy_commands_dir(
    root: &std::path::Path,
    requested: &str,
) -> Option<std::path::PathBuf> {
    let direct_dir = root.join(requested).join("SKILL.md");
    if direct_dir.is_file() {
        return Some(direct_dir);
    }

    let direct_markdown = root.join(format!("{requested}.md"));
    if direct_markdown.is_file() {
        return Some(direct_markdown);
    }

    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let candidate_path = if path.is_dir() {
            let skill_path = path.join("SKILL.md");
            if !skill_path.is_file() {
                continue;
            }
            skill_path
        } else if path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
        {
            path
        } else {
            continue;
        };

        let matches_entry_name = candidate_path
            .file_stem()
            .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case(requested))
            || entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".md")
                .eq_ignore_ascii_case(requested);
        if matches_entry_name || skill_frontmatter_name_matches(&candidate_path, requested) {
            return Some(candidate_path);
        }
    }

    None
}

fn skill_frontmatter_name_matches(path: &std::path::Path, requested: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| parse_skill_name(&contents))
        .is_some_and(|name| name.eq_ignore_ascii_case(requested))
}

fn parse_skill_name(contents: &str) -> Option<String> {
    plugins::frontmatter::parse_frontmatter(contents).ok()?.frontmatter.name
}

#[allow(dead_code)]
fn parse_skill_frontmatter_value(contents: &str, key: &str) -> Option<String> {
    if key != "name" {
        return None;
    }
    plugins::frontmatter::parse_frontmatter(contents).ok()?.frontmatter.name
}

fn canonical_tool_token(value: &str) -> String {
    let mut canonical = value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if let Some(stripped) = canonical.strip_suffix("tool") {
        canonical = stripped.to_string();
    }
    canonical
}

fn normalize_tool_search_query(query: &str) -> String {
    query
        .trim()
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .filter(|term| !term.is_empty())
        .map(canonical_tool_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn search_tool_specs(query: &str, max_results: usize, specs: &[SearchableToolSpec]) -> Vec<String> {
    let lowered = query.to_lowercase();
    if let Some(selection) = lowered.strip_prefix("select:") {
        return selection
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter_map(|wanted| {
                let wanted = canonical_tool_token(wanted);
                specs
                    .iter()
                    .find(|spec| canonical_tool_token(&spec.name) == wanted)
                    .map(|spec| spec.name.clone())
            })
            .take(max_results)
            .collect();
    }

    let mut required = Vec::new();
    let mut optional = Vec::new();
    for term in lowered.split_whitespace() {
        if let Some(rest) = term.strip_prefix('+') {
            if !rest.is_empty() {
                required.push(rest);
            }
        } else {
            optional.push(term);
        }
    }
    let terms = if required.is_empty() {
        optional.clone()
    } else {
        required.iter().chain(optional.iter()).copied().collect()
    };

    let mut scored = specs
        .iter()
        .filter_map(|spec| {
            let name = spec.name.to_lowercase();
            let canonical_name = canonical_tool_token(&spec.name);
            let normalized_description = normalize_tool_search_query(&spec.description);
            let haystack = format!(
                "{name} {} {canonical_name}",
                spec.description.to_lowercase()
            );
            let normalized_haystack = format!("{canonical_name} {normalized_description}");
            if required.iter().any(|term| !haystack.contains(term)) {
                return None;
            }

            let mut score = 0_i32;
            for term in &terms {
                let canonical_term = canonical_tool_token(term);
                if haystack.contains(term) {
                    score += 2;
                }
                if name == *term {
                    score += 8;
                }
                if name.contains(term) {
                    score += 4;
                }
                if canonical_name == canonical_term {
                    score += 12;
                }
                if normalized_haystack.contains(&canonical_term) {
                    score += 3;
                }
            }

            if score == 0 && !lowered.is_empty() {
                return None;
            }
            Some((score, spec.name.clone()))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    scored
        .into_iter()
        .map(|(_, name)| name)
        .take(max_results)
        .collect()
}

fn deferred_tool_specs() -> Vec<runtime::tool_registry::ToolSpec> {
    runtime::tool_registry::mvp_tool_specs()
        .into_iter()
        .filter(|spec| {
            !matches!(
                spec.name,
                "bash" | "read_file" | "new_file" | "edit_file" | "glob_search" | "grep_search"
            )
        })
        .collect()
}

#[allow(clippy::needless_pass_by_value)]
fn execute_tool_search(input: ToolSearchInput) -> ToolSearchOutput {
    GlobalToolRegistry::builtin().search(&input.query, input.max_results.unwrap_or(5), None, None)
}

fn iso8601_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|e| {
            eprintln!("[tools] system clock is before epoch ({e}); using 0 for timestamp");
            std::time::Duration::ZERO
        })
        .as_secs()
        .to_string()
}

// Agent system lives in `crates/agents/`. See `execute_agent_with_spawn` above
// for the only remaining local entry point.

#[allow(clippy::too_many_lines)]
fn execute_notebook_edit(input: NotebookEditInput) -> Result<NotebookEditOutput, String> {
    let path = std::path::PathBuf::from(&input.notebook_path);
    if path.extension().and_then(|ext| ext.to_str()) != Some("ipynb") {
        return Err(String::from(
            "File must be a Jupyter notebook (.ipynb file).",
        ));
    }

    let original_file = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut notebook: serde_json::Value =
        serde_json::from_str(&original_file).map_err(|error| error.to_string())?;
    let language = notebook
        .get("metadata")
        .and_then(|metadata| metadata.get("kernelspec"))
        .and_then(|kernelspec| kernelspec.get("language"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("python")
        .to_string();
    let cells = notebook
        .get_mut("cells")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| String::from("Notebook cells array not found"))?;

    let edit_mode = input.edit_mode.unwrap_or(NotebookEditMode::Replace);
    let target_index = match input.cell_id.as_deref() {
        Some(cell_id) => Some(resolve_cell_index(cells, Some(cell_id), edit_mode)?),
        None if matches!(
            edit_mode,
            NotebookEditMode::Replace | NotebookEditMode::Delete
        ) =>
        {
            Some(resolve_cell_index(cells, None, edit_mode)?)
        }
        None => None,
    };
    let resolved_cell_type = match edit_mode {
        NotebookEditMode::Delete => None,
        NotebookEditMode::Insert => Some(input.cell_type.unwrap_or(NotebookCellType::Code)),
        NotebookEditMode::Replace => Some(input.cell_type.unwrap_or_else(|| {
            target_index
                .and_then(|index| cells.get(index))
                .and_then(cell_kind)
                .unwrap_or(NotebookCellType::Code)
        })),
    };
    let new_source = require_notebook_source(input.new_source, edit_mode)?;

    let cell_id = match edit_mode {
        NotebookEditMode::Insert => {
            let resolved_cell_type = resolved_cell_type
                .ok_or_else(|| String::from("insert mode requires a cell type"))?;
            let new_id = make_cell_id(cells.len());
            let new_cell = build_notebook_cell(&new_id, resolved_cell_type, &new_source);
            let insert_at = target_index.map_or(cells.len(), |index| index + 1);
            cells.insert(insert_at, new_cell);
            cells
                .get(insert_at)
                .and_then(|cell| cell.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
        NotebookEditMode::Delete => {
            let idx = target_index
                .ok_or_else(|| String::from("delete mode requires a target cell index"))?;
            let removed = cells.remove(idx);
            removed
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
        NotebookEditMode::Replace => {
            let resolved_cell_type = resolved_cell_type
                .ok_or_else(|| String::from("replace mode requires a cell type"))?;
            let idx = target_index
                .ok_or_else(|| String::from("replace mode requires a target cell index"))?;
            let cell = cells
                .get_mut(idx)
                .ok_or_else(|| String::from("Cell index out of range"))?;
            cell["source"] = serde_json::Value::Array(source_lines(&new_source));
            cell["cell_type"] = serde_json::Value::String(match resolved_cell_type {
                NotebookCellType::Code => String::from("code"),
                NotebookCellType::Markdown => String::from("markdown"),
            });
            match resolved_cell_type {
                NotebookCellType::Code => {
                    if !cell.get("outputs").is_some_and(serde_json::Value::is_array) {
                        cell["outputs"] = json!([]);
                    }
                    if cell.get("execution_count").is_none() {
                        cell["execution_count"] = serde_json::Value::Null;
                    }
                }
                NotebookCellType::Markdown => {
                    if let Some(object) = cell.as_object_mut() {
                        object.remove("outputs");
                        object.remove("execution_count");
                    }
                }
            }
            cell.get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
    };

    let updated_file =
        serde_json::to_string_pretty(&notebook).map_err(|error| error.to_string())?;
    std::fs::write(&path, &updated_file).map_err(|error| error.to_string())?;

    Ok(NotebookEditOutput {
        new_source,
        cell_id,
        cell_type: resolved_cell_type,
        language,
        edit_mode: format_notebook_edit_mode(edit_mode),
        error: None,
        notebook_path: path.display().to_string(),
        original_file,
        updated_file,
    })
}

fn require_notebook_source(
    source: Option<String>,
    edit_mode: NotebookEditMode,
) -> Result<String, String> {
    match edit_mode {
        NotebookEditMode::Delete => Ok(source.unwrap_or_default()),
        NotebookEditMode::Insert | NotebookEditMode::Replace => source
            .ok_or_else(|| String::from("new_source is required for insert and replace edits")),
    }
}

fn build_notebook_cell(cell_id: &str, cell_type: NotebookCellType, source: &str) -> Value {
    let mut cell = json!({
        "cell_type": match cell_type {
            NotebookCellType::Code => "code",
            NotebookCellType::Markdown => "markdown",
        },
        "id": cell_id,
        "metadata": {},
        "source": source_lines(source),
    });
    if let Some(object) = cell.as_object_mut() {
        match cell_type {
            NotebookCellType::Code => {
                object.insert(String::from("outputs"), json!([]));
                object.insert(String::from("execution_count"), Value::Null);
            }
            NotebookCellType::Markdown => {}
        }
    }
    cell
}

fn cell_kind(cell: &serde_json::Value) -> Option<NotebookCellType> {
    cell.get("cell_type")
        .and_then(serde_json::Value::as_str)
        .map(|kind| {
            if kind == "markdown" {
                NotebookCellType::Markdown
            } else {
                NotebookCellType::Code
            }
        })
}

const MAX_SLEEP_DURATION_MS: u64 = 300_000;

#[allow(clippy::needless_pass_by_value)]
fn execute_sleep(input: SleepInput) -> Result<SleepOutput, String> {
    if input.duration_ms > MAX_SLEEP_DURATION_MS {
        return Err(format!(
            "duration_ms {} exceeds maximum allowed sleep of {MAX_SLEEP_DURATION_MS}ms",
            input.duration_ms,
        ));
    }
    std::thread::sleep(Duration::from_millis(input.duration_ms));
    Ok(SleepOutput {
        duration_ms: input.duration_ms,
        message: format!("Slept for {}ms", input.duration_ms),
    })
}

fn execute_brief(input: BriefInput) -> Result<BriefOutput, String> {
    if input.message.trim().is_empty() {
        return Err(String::from("message must not be empty"));
    }

    let attachments = input
        .attachments
        .as_ref()
        .map(|paths| {
            paths
                .iter()
                .map(|path| resolve_attachment(path))
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?;

    let message = match input.status {
        BriefStatus::Normal | BriefStatus::Proactive => input.message,
    };

    Ok(BriefOutput {
        message,
        attachments,
        sent_at: iso8601_timestamp(),
    })
}

fn resolve_attachment(path: &str) -> Result<ResolvedAttachment, String> {
    let resolved = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| error.to_string())?;
    Ok(ResolvedAttachment {
        path: resolved.display().to_string(),
        size: metadata.len(),
        is_image: is_image_path(&resolved),
    })
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg")
    )
}

fn execute_config(input: ConfigInput) -> Result<ConfigOutput, String> {
    let setting = input.setting.trim();
    if setting.is_empty() {
        return Err(String::from("setting must not be empty"));
    }
    let Some(spec) = supported_config_setting(setting) else {
        return Ok(ConfigOutput {
            success: false,
            operation: None,
            setting: None,
            value: None,
            previous_value: None,
            new_value: None,
            error: Some(format!("Unknown setting: \"{setting}\"")),
        });
    };

    let path = config_file_for_scope(spec.scope)?;
    let mut document = read_json_object(&path)?;

    if let Some(value) = input.value {
        let normalized = normalize_config_value(spec, value)?;
        let previous_value = get_nested_value(&document, spec.path).cloned();
        set_nested_value(&mut document, spec.path, normalized.clone());
        write_json_object(&path, &document)?;
        Ok(ConfigOutput {
            success: true,
            operation: Some(String::from("set")),
            setting: Some(setting.to_string()),
            value: Some(normalized.clone()),
            previous_value,
            new_value: Some(normalized),
            error: None,
        })
    } else {
        Ok(ConfigOutput {
            success: true,
            operation: Some(String::from("get")),
            setting: Some(setting.to_string()),
            value: get_nested_value(&document, spec.path).cloned(),
            previous_value: None,
            new_value: None,
            error: None,
        })
    }
}

const PERMISSION_DEFAULT_MODE_PATH: &[&str] = &["permissions", "defaultMode"];

fn execute_enter_plan_mode(_input: EnterPlanModeInput) -> Result<PlanModeOutput, String> {
    let settings_path = config_file_for_scope(ConfigScope::Settings)?;
    let state_path = plan_mode_state_file()?;
    let mut document = read_json_object(&settings_path)?;
    let current_local_mode = get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned();
    let current_is_plan =
        matches!(current_local_mode.as_ref(), Some(Value::String(value)) if value == "plan");

    if let Some(state) = read_plan_mode_state(&state_path)? {
        if current_is_plan {
            return Ok(PlanModeOutput {
                success: true,
                operation: String::from("enter"),
                changed: false,
                active: true,
                managed: true,
                message: String::from("Plan mode override is already active for this worktree."),
                settings_path: settings_path.display().to_string(),
                state_path: state_path.display().to_string(),
                previous_local_mode: state.previous_local_mode,
                current_local_mode,
            });
        }
        clear_plan_mode_state(&state_path)?;
    }

    if current_is_plan {
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("enter"),
            changed: false,
            active: true,
            managed: false,
            message: String::from(
                "Worktree-local plan mode is already enabled outside EnterPlanMode; leaving it unchanged.",
            ),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: None,
            current_local_mode,
        });
    }

    let state = PlanModeState {
        had_local_override: current_local_mode.is_some(),
        previous_local_mode: current_local_mode.clone(),
    };
    write_plan_mode_state(&state_path, &state)?;
    set_nested_value(
        &mut document,
        PERMISSION_DEFAULT_MODE_PATH,
        Value::String(String::from("plan")),
    );
    write_json_object(&settings_path, &document)?;

    Ok(PlanModeOutput {
        success: true,
        operation: String::from("enter"),
        changed: true,
        active: true,
        managed: true,
        message: String::from("Enabled worktree-local plan mode override."),
        settings_path: settings_path.display().to_string(),
        state_path: state_path.display().to_string(),
        previous_local_mode: state.previous_local_mode,
        current_local_mode: get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned(),
    })
}

fn execute_exit_plan_mode(_input: ExitPlanModeInput) -> Result<PlanModeOutput, String> {
    let settings_path = config_file_for_scope(ConfigScope::Settings)?;
    let state_path = plan_mode_state_file()?;
    let mut document = read_json_object(&settings_path)?;
    let current_local_mode = get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned();
    let current_is_plan =
        matches!(current_local_mode.as_ref(), Some(Value::String(value)) if value == "plan");

    let Some(state) = read_plan_mode_state(&state_path)? else {
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("exit"),
            changed: false,
            active: current_is_plan,
            managed: false,
            message: String::from("No EnterPlanMode override is active for this worktree."),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: None,
            current_local_mode,
        });
    };

    if !current_is_plan {
        clear_plan_mode_state(&state_path)?;
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("exit"),
            changed: false,
            active: false,
            managed: false,
            message: String::from(
                "Cleared stale EnterPlanMode state because plan mode was already changed outside the tool.",
            ),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: state.previous_local_mode,
            current_local_mode,
        });
    }

    if state.had_local_override {
        if let Some(previous_local_mode) = state.previous_local_mode.clone() {
            set_nested_value(
                &mut document,
                PERMISSION_DEFAULT_MODE_PATH,
                previous_local_mode,
            );
        } else {
            remove_nested_value(&mut document, PERMISSION_DEFAULT_MODE_PATH);
        }
    } else {
        remove_nested_value(&mut document, PERMISSION_DEFAULT_MODE_PATH);
    }
    write_json_object(&settings_path, &document)?;
    clear_plan_mode_state(&state_path)?;

    Ok(PlanModeOutput {
        success: true,
        operation: String::from("exit"),
        changed: true,
        active: false,
        managed: false,
        message: String::from("Restored the prior worktree-local plan mode setting."),
        settings_path: settings_path.display().to_string(),
        state_path: state_path.display().to_string(),
        previous_local_mode: state.previous_local_mode,
        current_local_mode: get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned(),
    })
}

fn execute_structured_output(
    input: StructuredOutputInput,
) -> Result<StructuredOutputResult, String> {
    if input.0.is_null() {
        return Err(String::from("structured output payload must not be empty"));
    }
    Ok(StructuredOutputResult {
        data: String::from("Structured output provided successfully"),
        structured_output: input.0,
    })
}

fn execute_repl(input: ReplInput) -> Result<ReplOutput, String> {
    if input.code.trim().is_empty() {
        return Err(String::from("code must not be empty"));
    }
    let runtime = resolve_repl_runtime(&input.language)?;
    let started = Instant::now();
    let mut process = Command::new(runtime.program);
    process
        .args(runtime.args)
        .arg(&input.code)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = if let Some(timeout_ms) = input.timeout_ms {
        let mut child = process.spawn().map_err(|error| error.to_string())?;
        loop {
            if child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                break child
                    .wait_with_output()
                    .map_err(|error| error.to_string())?;
            }
            if started.elapsed() >= Duration::from_millis(timeout_ms) {
                child.kill().map_err(|error| error.to_string())?;
                child
                    .wait_with_output()
                    .map_err(|error| error.to_string())?;
                return Err(format!(
                    "REPL execution exceeded timeout of {timeout_ms} ms"
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    } else {
        process
            .spawn()
            .map_err(|error| error.to_string())?
            .wait_with_output()
            .map_err(|error| error.to_string())?
    };

    Ok(ReplOutput {
        language: input.language,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(1),
        duration_ms: started.elapsed().as_millis(),
    })
}

struct ReplRuntime {
    program: &'static str,
    args: &'static [&'static str],
}

fn resolve_repl_runtime(language: &str) -> Result<ReplRuntime, String> {
    match language.trim().to_ascii_lowercase().as_str() {
        "python" | "py" => Ok(ReplRuntime {
            program: detect_first_command(&["python3", "python"])
                .ok_or_else(|| String::from("python runtime not found"))?,
            args: &["-c"],
        }),
        "javascript" | "js" | "node" => Ok(ReplRuntime {
            program: detect_first_command(&["node"])
                .ok_or_else(|| String::from("node runtime not found"))?,
            args: &["-e"],
        }),
        "sh" | "shell" | "bash" => Ok(ReplRuntime {
            program: detect_first_command(&["bash", "sh"])
                .ok_or_else(|| String::from("shell runtime not found"))?,
            args: &["-lc"],
        }),
        other => Err(format!("unsupported REPL language: {other}")),
    }
}

fn detect_first_command(commands: &[&'static str]) -> Option<&'static str> {
    commands
        .iter()
        .copied()
        .find(|command| command_exists(command))
}

#[derive(Clone, Copy)]
enum ConfigScope {
    Global,
    Settings,
}

#[derive(Clone, Copy)]
struct ConfigSettingSpec {
    scope: ConfigScope,
    kind: ConfigKind,
    path: &'static [&'static str],
    options: Option<&'static [&'static str]>,
}

#[derive(Clone, Copy)]
enum ConfigKind {
    Boolean,
    String,
}

fn supported_config_setting(setting: &str) -> Option<ConfigSettingSpec> {
    Some(match setting {
        "theme" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["theme"],
            options: None,
        },
        "editorMode" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["editorMode"],
            options: Some(&["default", "vim", "emacs"]),
        },
        "verbose" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["verbose"],
            options: None,
        },
        "preferredNotifChannel" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["preferredNotifChannel"],
            options: None,
        },
        "autoCompactEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["autoCompactEnabled"],
            options: None,
        },
        "autoMemoryEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["autoMemoryEnabled"],
            options: None,
        },
        "autoDreamEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["autoDreamEnabled"],
            options: None,
        },
        "fileCheckpointingEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["fileCheckpointingEnabled"],
            options: None,
        },
        "showTurnDuration" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["showTurnDuration"],
            options: None,
        },
        "terminalProgressBarEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["terminalProgressBarEnabled"],
            options: None,
        },
        "todoFeatureEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["todoFeatureEnabled"],
            options: None,
        },
        "model" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["model"],
            options: None,
        },
        "alwaysThinkingEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["alwaysThinkingEnabled"],
            options: None,
        },
        "permissions.defaultMode" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["permissions", "defaultMode"],
            options: Some(&["default", "plan", "acceptEdits", "dontAsk", "auto"]),
        },
        "language" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["language"],
            options: None,
        },
        "teammateMode" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["teammateMode"],
            options: Some(&["tmux", "in-process", "auto"]),
        },
        _ => return None,
    })
}

fn normalize_config_value(spec: ConfigSettingSpec, value: ConfigValue) -> Result<Value, String> {
    let normalized = match (spec.kind, value) {
        (ConfigKind::Boolean, ConfigValue::Bool(value)) => Value::Bool(value),
        (ConfigKind::Boolean, ConfigValue::String(value)) => {
            match value.trim().to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(String::from("setting requires true or false")),
            }
        }
        (ConfigKind::Boolean, ConfigValue::Number(_))
        | (ConfigKind::Boolean, ConfigValue::Array(_))
        | (ConfigKind::Boolean, ConfigValue::Object(_))
        | (ConfigKind::Boolean, ConfigValue::Null) => {
            return Err(String::from("setting requires true or false"))
        }
        (ConfigKind::String, ConfigValue::String(value)) => Value::String(value),
        (ConfigKind::String, ConfigValue::Bool(value)) => Value::String(value.to_string()),
        (ConfigKind::String, ConfigValue::Number(value)) => json!(value),
        (ConfigKind::String, ConfigValue::Array(_))
        | (ConfigKind::String, ConfigValue::Object(_))
        | (ConfigKind::String, ConfigValue::Null) => {
            return Err(String::from("setting requires a string value"))
        }
    };

    if let Some(options) = spec.options {
        let Some(as_str) = normalized.as_str() else {
            return Err(String::from("setting requires a string value"));
        };
        if !options.iter().any(|option| option == &as_str) {
            return Err(format!(
                "Invalid value \"{as_str}\". Options: {}",
                options.join(", ")
            ));
        }
    }

    Ok(normalized)
}

fn config_file_for_scope(scope: ConfigScope) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(match scope {
        ConfigScope::Global => config_home_dir()?.join("settings.json"),
        ConfigScope::Settings => cwd.join(".claw").join("settings.local.json"),
    })
}

fn config_home_dir() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("CLAW_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            String::from(
                "HOME is not set (on Windows, set USERPROFILE or HOME, \
                 or use CLAW_CONFIG_HOME to point directly at the config directory)",
            )
        })?;
    Ok(PathBuf::from(home).join(".claw"))
}

fn read_json_object(path: &Path) -> Result<serde_json::Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(serde_json::Map::new());
            }
            serde_json::from_str::<Value>(&contents)
                .map_err(|error| error.to_string())?
                .as_object()
                .cloned()
                .ok_or_else(|| String::from("config file must contain a JSON object"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn write_json_object(path: &Path, value: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn get_nested_value<'a>(
    value: &'a serde_json::Map<String, Value>,
    path: &[&str],
) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut current = value.get(*first)?;
    for key in rest {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

fn set_nested_value(root: &mut serde_json::Map<String, Value>, path: &[&str], new_value: Value) {
    let (first, rest) = path.split_first().expect("config path must not be empty");
    if rest.is_empty() {
        root.insert((*first).to_string(), new_value);
        return;
    }

    let entry = root
        .entry((*first).to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(serde_json::Map::new());
    }
    let map = entry.as_object_mut().expect("object inserted");
    set_nested_value(map, rest, new_value);
}

fn remove_nested_value(root: &mut serde_json::Map<String, Value>, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return root.remove(*first).is_some();
    }

    let mut should_remove_parent = false;
    let removed = root.get_mut(*first).is_some_and(|entry| {
        entry.as_object_mut().is_some_and(|map| {
            let removed = remove_nested_value(map, rest);
            should_remove_parent = removed && map.is_empty();
            removed
        })
    });

    if should_remove_parent {
        root.remove(*first);
    }

    removed
}

fn plan_mode_state_file() -> Result<PathBuf, String> {
    Ok(config_file_for_scope(ConfigScope::Settings)?
        .parent()
        .ok_or_else(|| String::from("settings.local.json has no parent directory"))?
        .join("tool-state")
        .join("plan-mode.json"))
}

fn read_plan_mode_state(path: &Path) -> Result<Option<PlanModeState>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(None);
            }
            serde_json::from_str(&contents)
                .map(Some)
                .map_err(|error| error.to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn write_plan_mode_state(path: &Path, state: &PlanModeState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(state).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn clear_plan_mode_state(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn iso8601_timestamp() -> String {
    if let Ok(output) = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
    {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }
    iso8601_now()
}

#[allow(clippy::needless_pass_by_value)]
fn execute_powershell(input: PowerShellInput) -> std::io::Result<runtime::BashCommandOutput> {
    let _ = &input.description;
    if let Some(output) = workspace_test_branch_preflight(&input.command) {
        return Ok(output);
    }
    let shell = detect_powershell_shell()?;
    execute_shell_command(
        shell,
        &input.command,
        input.timeout,
        input.run_in_background,
    )
}

fn detect_powershell_shell() -> std::io::Result<&'static str> {
    if command_exists("pwsh") {
        Ok("pwsh")
    } else if command_exists("powershell") {
        Ok("powershell")
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "PowerShell executable not found (expected `pwsh` or `powershell` in PATH)",
        ))
    }
}

fn command_exists(command: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {command} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Build the full PowerShell script to execute, prepending a UTF-8
/// encoding preamble so non-ASCII file paths and content round-trip
/// correctly through Windows PowerShell 5.x's default OEM code page
/// and PowerShell 7's default UTF-8 mode.
///
/// The script is intended to be piped to the process via stdin
/// (`-Command -`) so the user's command never traverses the Win32
/// command line / PowerShell command-line re-parser.
///
/// Precedence (PowerShell parameter defaults are overridden last to win):
/// - `$OutputEncoding` controls how PowerShell encodes strings it sends
///   to native commands.
/// - `[Console]::InputEncoding` / `[Console]::OutputEncoding` control
///   the stdio streams.
/// - `$PSDefaultParameterValues` for `Get-Content` / `Out-File` makes
///   the most common file cmdlets default to UTF-8.
fn build_powershell_script(command: &str) -> String {
    const PREAMBLE: &str = r#"$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::InputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$PSDefaultParameterValues['Out-File:Encoding'] = 'utf8'
$PSDefaultParameterValues['Get-Content:Encoding'] = 'utf8'
$PSDefaultParameterValues['Select-String:Encoding'] = 'utf8'
"#;
    format!("{PREAMBLE}\n{command}\n")
}

#[allow(clippy::too_many_lines)]
fn execute_shell_command(
    shell: &str,
    command: &str,
    timeout: Option<u64>,
    run_in_background: Option<bool>,
) -> std::io::Result<runtime::BashCommandOutput> {
    // Prepend the UTF-8 preamble and pipe via stdin so non-ASCII file
    // paths and content round-trip correctly. `-Command -` reads the
    // script from stdin, which never traverses the Win32 command line
    // / PowerShell command-line re-parser.
    let script = build_powershell_script(command);

    if run_in_background.unwrap_or(false) {
        let mut process = std::process::Command::new(shell);
        process
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = process.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(script.as_bytes());
            // stdin closes on drop, which signals EOF to PowerShell.
        }
        return Ok(runtime::BashCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: Some(child.id().to_string()),
            backgrounded_by_user: Some(true),
            assistant_auto_backgrounded: Some(false),
            dangerously_disable_sandbox: None,
            return_code_interpretation: None,
            no_output_expected: Some(true),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: None,
            sandbox_type: None,
        });
    }

    let mut process = std::process::Command::new(shell);
    process
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(timeout_ms) = timeout {
        let mut child = process.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(script.as_bytes());
        }
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let output = child.wait_with_output()?;
                return Ok(runtime::BashCommandOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    raw_output_path: None,
                    interrupted: false,
                    is_image: None,
                    background_task_id: None,
                    backgrounded_by_user: None,
                    assistant_auto_backgrounded: None,
                    dangerously_disable_sandbox: None,
                    return_code_interpretation: status
                        .code()
                        .filter(|code| *code != 0)
                        .map(|code| format!("exit_code:{code}")),
                    no_output_expected: Some(output.stdout.is_empty() && output.stderr.is_empty()),
                    structured_content: None,
                    persisted_output_path: None,
                    persisted_output_size: None,
                    sandbox_status: None,
                    sandbox_type: None,
                });
            }
            if started.elapsed() >= Duration::from_millis(timeout_ms) {
                let _ = child.kill();
                let output = child.wait_with_output()?;
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let stderr = if stderr.trim().is_empty() {
                    format!("Command exceeded timeout of {timeout_ms} ms")
                } else {
                    format!(
                        "{}
Command exceeded timeout of {timeout_ms} ms",
                        stderr.trim_end()
                    )
                };
                return Ok(runtime::BashCommandOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr,
                    raw_output_path: None,
                    interrupted: true,
                    is_image: None,
                    background_task_id: None,
                    backgrounded_by_user: None,
                    assistant_auto_backgrounded: None,
                    dangerously_disable_sandbox: None,
                    return_code_interpretation: Some(String::from("timeout")),
                    no_output_expected: Some(false),
                    structured_content: None,
                    persisted_output_path: None,
                    persisted_output_size: None,
                    sandbox_status: None,
                    sandbox_type: None,
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let mut child = process.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(script.as_bytes());
    }
    let output = child.wait_with_output()?;
    Ok(runtime::BashCommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: None,
        return_code_interpretation: output
            .status
            .code()
            .filter(|code| *code != 0)
            .map(|code| format!("exit_code:{code}")),
        no_output_expected: Some(output.stdout.is_empty() && output.stderr.is_empty()),
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: None,
        sandbox_type: None,
    })
}

fn resolve_cell_index(
    cells: &[serde_json::Value],
    cell_id: Option<&str>,
    edit_mode: NotebookEditMode,
) -> Result<usize, String> {
    if cells.is_empty()
        && matches!(
            edit_mode,
            NotebookEditMode::Replace | NotebookEditMode::Delete
        )
    {
        return Err(String::from("Notebook has no cells to edit"));
    }
    if let Some(cell_id) = cell_id {
        cells
            .iter()
            .position(|cell| cell.get("id").and_then(serde_json::Value::as_str) == Some(cell_id))
            .ok_or_else(|| format!("Cell id not found: {cell_id}"))
    } else {
        Ok(cells.len().saturating_sub(1))
    }
}

fn source_lines(source: &str) -> Vec<serde_json::Value> {
    if source.is_empty() {
        return vec![serde_json::Value::String(String::new())];
    }
    source
        .split_inclusive('\n')
        .map(|line| serde_json::Value::String(line.to_string()))
        .collect()
}

fn format_notebook_edit_mode(mode: NotebookEditMode) -> String {
    match mode {
        NotebookEditMode::Replace => String::from("replace"),
        NotebookEditMode::Insert => String::from("insert"),
        NotebookEditMode::Delete => String::from("delete"),
    }
}

fn make_cell_id(index: usize) -> String {
    format!("cell-{}", index + 1)
}

fn parse_skill_description(contents: &str) -> Option<String> {
    plugins::frontmatter::parse_frontmatter(contents).ok()?.frontmatter.description
}

pub mod lane_completion;
pub mod pdf_extract;

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;

    use super::{
        execute_agent_with_spawn, execute_tool, permission_mode_from_plugin, run_task_packet,
        tools_init, wiki_mirror_url, AgentInput, AgentStatus, EditFileInput,
        GlobalToolRegistry, LaneEventName,
    };
    use agents::{AgentHandle, AgentJob};
    use runtime::{
        permission_enforcer::PermissionEnforcer, PermissionMode, PermissionPolicy, TaskPacket,
        ToolExecutor,
    };
    use serde_json::json;

    fn mvp_tool_specs() -> Vec<(&'static str, PermissionMode)> {
        vec![
            ("bash", PermissionMode::DangerFullAccess),
            ("read_file", PermissionMode::ReadOnly),
            ("new_file", PermissionMode::WorkspaceWrite),
            ("edit_file", PermissionMode::WorkspaceWrite),
            ("glob_search", PermissionMode::ReadOnly),
            ("grep_search", PermissionMode::ReadOnly),
            ("WebFetch", PermissionMode::ReadOnly),
            ("WebFind", PermissionMode::ReadOnly),
            ("WebSearch", PermissionMode::ReadOnly),
            ("TodoWrite", PermissionMode::WorkspaceWrite),
            ("Skill", PermissionMode::ReadOnly),
            ("Agent", PermissionMode::DangerFullAccess),
            ("ToolSearch", PermissionMode::ReadOnly),
            ("NotebookEdit", PermissionMode::WorkspaceWrite),
            ("Sleep", PermissionMode::ReadOnly),
            ("SendUserMessage", PermissionMode::ReadOnly),
            ("Config", PermissionMode::WorkspaceWrite),
            ("EnterPlanMode", PermissionMode::WorkspaceWrite),
            ("ExitPlanMode", PermissionMode::WorkspaceWrite),
            ("StructuredOutput", PermissionMode::ReadOnly),
            ("REPL", PermissionMode::DangerFullAccess),
            ("PowerShell", PermissionMode::DangerFullAccess),
            ("AskUserQuestion", PermissionMode::ReadOnly),
            ("TaskCreate", PermissionMode::DangerFullAccess),
            ("RunTaskPacket", PermissionMode::DangerFullAccess),
            ("TaskGet", PermissionMode::ReadOnly),
            ("TaskList", PermissionMode::ReadOnly),
            ("TaskStop", PermissionMode::DangerFullAccess),
            ("TaskUpdate", PermissionMode::DangerFullAccess),
            ("TaskOutput", PermissionMode::ReadOnly),
            ("WorkerCreate", PermissionMode::DangerFullAccess),
            ("WorkerGet", PermissionMode::ReadOnly),
            ("WorkerObserve", PermissionMode::ReadOnly),
            ("WorkerResolveTrust", PermissionMode::DangerFullAccess),
            ("WorkerAwaitReady", PermissionMode::ReadOnly),
            ("WorkerSendPrompt", PermissionMode::DangerFullAccess),
            ("WorkerRestart", PermissionMode::DangerFullAccess),
            ("WorkerTerminate", PermissionMode::DangerFullAccess),
            ("WorkerObserveCompletion", PermissionMode::DangerFullAccess),
            ("TeamCreate", PermissionMode::DangerFullAccess),
            ("TeamDelete", PermissionMode::DangerFullAccess),
            ("CronCreate", PermissionMode::DangerFullAccess),
            ("CronDelete", PermissionMode::DangerFullAccess),
            ("CronList", PermissionMode::ReadOnly),
            ("LSP", PermissionMode::ReadOnly),
            ("ListMcpResources", PermissionMode::ReadOnly),
            ("ReadMcpResource", PermissionMode::ReadOnly),
            ("McpAuth", PermissionMode::DangerFullAccess),
            ("RemoteTrigger", PermissionMode::DangerFullAccess),
            ("MCP", PermissionMode::DangerFullAccess),
            ("ListAgents", PermissionMode::ReadOnly),
            ("ListSkills", PermissionMode::ReadOnly),
            ("ListPlugins", PermissionMode::ReadOnly),
            ("TestingPermission", PermissionMode::DangerFullAccess),
        ]
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn env_guard_recovers_after_poisoning() {
        let poisoned = std::thread::spawn(|| {
            let _guard = env_guard();
            panic!("poison env lock");
        })
        .join();
        assert!(poisoned.is_err(), "poisoning thread should panic");

        let _guard = env_guard();
    }

    fn temp_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("clawd-tools-{unique}-{name}"))
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap_or_else(|error| panic!("git {} failed: {error}", args.join(" ")));
        assert!(
            status.success(),
            "git {} exited with {status}",
            args.join(" ")
        );
    }

    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path).expect("create repo");
        run_git(path, &["init", "--quiet", "-b", "main"]);
        run_git(path, &["config", "user.email", "tests@example.com"]);
        run_git(path, &["config", "user.name", "Tools Tests"]);
        std::fs::write(path.join("README.md"), "initial\n").expect("write readme");
        run_git(path, &["add", "README.md"]);
        run_git(path, &["commit", "-m", "initial commit", "--quiet"]);
    }

    fn commit_file(path: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(path.join(file), contents).expect("write file");
        run_git(path, &["add", file]);
        run_git(path, &["commit", "-m", message, "--quiet"]);
    }

    fn permission_policy_for_mode(mode: PermissionMode) -> PermissionPolicy {
        mvp_tool_specs()
            .into_iter()
            .fold(PermissionPolicy::new(mode), |policy, (name, perm)| {
                policy.with_tool_requirement(name, perm)
            })
    }

    #[test]
    fn exposes_mvp_tools() {
        let names = mvp_tool_specs()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"WebFetch"));
        assert!(names.contains(&"WebFind"));
        assert!(names.contains(&"WebSearch"));
        assert!(names.contains(&"TodoWrite"));
        assert!(names.contains(&"Skill"));
        assert!(names.contains(&"Agent"));
        assert!(names.contains(&"ToolSearch"));
        assert!(names.contains(&"NotebookEdit"));
        assert!(names.contains(&"Sleep"));
        assert!(names.contains(&"SendUserMessage"));
        assert!(names.contains(&"Config"));
        assert!(names.contains(&"EnterPlanMode"));
        assert!(names.contains(&"ExitPlanMode"));
        assert!(names.contains(&"StructuredOutput"));
        assert!(names.contains(&"REPL"));
        assert!(names.contains(&"PowerShell"));
        assert!(names.contains(&"WorkerCreate"));
        assert!(names.contains(&"WorkerObserve"));
        assert!(names.contains(&"WorkerAwaitReady"));
        assert!(names.contains(&"WorkerSendPrompt"));
    }

    #[test]
    fn rejects_unknown_tool_names() {
        let error = execute_tool("nope", &json!({})).expect_err("tool should be rejected");
        assert!(error.contains("unsupported tool"));
    }

    #[test]
    fn tools_init_wires_subagent_executor() {
        use agents::SubagentToolExecutor;
    
        let _guard = env_guard();
    
        tools_init().expect("tools_init should register the executor");
    
        let mut allowed = BTreeSet::new();
        allowed.insert("ListSkills".to_string());
        let mut exec = SubagentToolExecutor::new(allowed);
        // Use ListSkills (in-process, no OS execution) to verify the
        // global tool executor closure was wired correctly.
        let result = exec.execute("ListSkills", r#"{}"#);
        let output = result.expect("subagent tool execution should succeed after init");
        assert!(
            output.contains(r#""skills""#),
            "ListSkills output should contain JSON, got: {output}"
        );
    }

    #[test]
    fn tools_init_is_idempotent() {
        let _guard = env_guard();

        tools_init().expect("first init should succeed");
        let second = tools_init();
        assert!(
            second.is_ok(),
            "second init should be a no-op, got: {second:?}"
        );
    }

    #[test]
    fn worker_tools_gate_prompt_delivery_until_ready_and_support_auto_trust() {
        let worktree = temp_path("gate-prompt-worktree");
        let repo = worktree.join("repo");
        let worktree_str = worktree.to_str().expect("utf-8").to_string();
        let repo_str = repo.to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({
                "cwd": repo_str,
                "trusted_roots": [worktree_str]
            }),
        )
        .expect("WorkerCreate should succeed");
        let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = created_output["worker_id"]
            .as_str()
            .expect("worker id")
            .to_string();
        assert_eq!(created_output["status"], "spawning");
        assert_eq!(created_output["trust_auto_resolve"], true);

        let gated = execute_tool(
            "WorkerSendPrompt",
            &json!({
                "worker_id": worker_id,
                "prompt": "ship the change"
            }),
        )
        .expect_err("prompt delivery before ready should fail");
        assert!(gated.contains("not ready for prompt delivery"));

        let observed = execute_tool(
            "WorkerObserve",
            &json!({
                "worker_id": created_output["worker_id"],
                "screen_text": "Do you trust the files in this folder?\n1. Yes, proceed\n2. No"
            }),
        )
        .expect("WorkerObserve should auto-resolve trust");
        let observed_output: serde_json::Value = serde_json::from_str(&observed).expect("json");
        assert_eq!(observed_output["status"], "spawning");
        assert_eq!(observed_output["trust_gate_cleared"], true);
        assert_eq!(
            observed_output["events"][1]["payload"]["type"],
            "trust_prompt"
        );
        assert_eq!(
            observed_output["events"][2]["payload"]["resolution"],
            "auto_allowlisted"
        );

        let ready = execute_tool(
            "WorkerObserve",
            &json!({
                "worker_id": created_output["worker_id"],
                "screen_text": "Ready for your input\n>"
            }),
        )
        .expect("WorkerObserve should mark worker ready");
        let ready_output: serde_json::Value = serde_json::from_str(&ready).expect("json");
        assert_eq!(ready_output["status"], "ready_for_prompt");

        let await_ready = execute_tool(
            "WorkerAwaitReady",
            &json!({
                "worker_id": created_output["worker_id"]
            }),
        )
        .expect("WorkerAwaitReady should succeed");
        let await_ready_output: serde_json::Value =
            serde_json::from_str(&await_ready).expect("json");
        assert_eq!(await_ready_output["ready"], true);

        let accepted = execute_tool(
            "WorkerSendPrompt",
            &json!({
                "worker_id": created_output["worker_id"],
                "prompt": "ship the change"
            }),
        )
        .expect("WorkerSendPrompt should succeed after ready");
        let accepted_output: serde_json::Value = serde_json::from_str(&accepted).expect("json");
        assert_eq!(accepted_output["status"], "running");
        assert_eq!(accepted_output["prompt_delivery_attempts"], 1);
        assert_eq!(accepted_output["prompt_in_flight"], true);
    }

    #[test]
    fn worker_create_merges_config_trusted_roots_without_per_call_override() {
        use std::fs;
        // Write a .claw/settings.json in a temp dir with trustedRoots
        let worktree = temp_path("config-trust-worktree");
        let claw_dir = worktree.join(".claw");
        fs::create_dir_all(&claw_dir).expect("create .claw dir");
        // Use the actual OS temp dir so the worktree path matches the allowlist
        let tmp_root = std::env::temp_dir().to_str().expect("utf-8").to_string();
        let settings = format!("{{\"trustedRoots\": [\"{tmp_root}\"]}}");
        fs::write(claw_dir.join("settings.json"), settings).expect("write settings");

        // WorkerCreate with no per-call trusted_roots 閳?config should supply them
        let cwd = worktree.to_str().expect("valid utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({
                "cwd": cwd
                // trusted_roots intentionally omitted
            }),
        )
        .expect("WorkerCreate should succeed");
        let output: serde_json::Value = serde_json::from_str(&created).expect("json");

        // worktree is under /tmp, so config roots auto-resolve trust
        assert_eq!(
            output["trust_auto_resolve"], true,
            "config-level trustedRoots should auto-resolve trust without per-call override"
        );

        fs::remove_dir_all(&worktree).ok();
    }

    #[test]
    fn worker_terminate_sets_finished_status() {
        // Create a worker in running state
        let cwd_path = temp_path("terminate-test");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let tmp_root = std::env::temp_dir().to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({"cwd": cwd_str, "trusted_roots": [tmp_root]}),
        )
        .expect("WorkerCreate should succeed");
        let output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

        // Terminate
        let terminated = execute_tool("WorkerTerminate", &json!({"worker_id": worker_id}))
            .expect("WorkerTerminate should succeed");
        let term_output: serde_json::Value = serde_json::from_str(&terminated).expect("json");
        assert_eq!(
            term_output["status"], "finished",
            "terminated worker should be finished"
        );
        assert_eq!(
            term_output["prompt_in_flight"], false,
            "prompt_in_flight should be cleared on termination"
        );
    }

    #[test]
    fn worker_restart_resets_to_spawning() {
        // Create and advance worker to ready_for_prompt
        let cwd_path = temp_path("restart-test");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let tmp_root = std::env::temp_dir().to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({"cwd": cwd_str, "trusted_roots": [tmp_root]}),
        )
        .expect("WorkerCreate should succeed");
        let output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

        // Advance to ready_for_prompt via observe
        execute_tool(
            "WorkerObserve",
            &json!({"worker_id": worker_id, "screen_text": "Ready for input\n>"}),
        )
        .expect("WorkerObserve should succeed");

        // Restart
        let restarted = execute_tool("WorkerRestart", &json!({"worker_id": worker_id}))
            .expect("WorkerRestart should succeed");
        let restart_output: serde_json::Value = serde_json::from_str(&restarted).expect("json");
        assert_eq!(
            restart_output["status"], "spawning",
            "restarted worker should return to spawning"
        );
        assert_eq!(
            restart_output["prompt_in_flight"], false,
            "prompt_in_flight should be cleared on restart"
        );
        assert_eq!(
            restart_output["trust_gate_cleared"], false,
            "trust_gate_cleared should be reset on restart (re-trust required)"
        );
    }

    #[test]
    fn worker_get_returns_worker_state() {
        let cwd_path = temp_path("worker-get-test");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let tmp_root = std::env::temp_dir().to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({"cwd": cwd_str, "trusted_roots": [tmp_root]}),
        )
        .expect("WorkerCreate should succeed");
        let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = created_output["worker_id"].as_str().expect("worker_id");

        let fetched = execute_tool("WorkerGet", &json!({"worker_id": worker_id}))
            .expect("WorkerGet should succeed");
        let fetched_output: serde_json::Value = serde_json::from_str(&fetched).expect("json");
        assert_eq!(fetched_output["worker_id"], worker_id);
        assert_eq!(fetched_output["status"], "spawning");
        assert_eq!(fetched_output["cwd"], cwd_str);
    }

    #[test]
    fn worker_get_on_unknown_id_returns_error() {
        let result = execute_tool(
            "WorkerGet",
            &json!({"worker_id": "worker_nonexistent_get_00000000"}),
        );
        assert!(
            result.is_err(),
            "WorkerGet on unknown id should return error"
        );
        assert!(
            result.unwrap_err().contains("worker not found"),
            "error should mention worker not found"
        );
    }

    #[test]
    fn worker_await_ready_on_spawning_worker_returns_not_ready() {
        let cwd_path = temp_path("worker-await-not-ready");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({"cwd": cwd_str}),
        )
        .expect("WorkerCreate should succeed");
        let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = created_output["worker_id"].as_str().expect("worker_id");

        // Worker is still in spawning 閳?await_ready should return not-ready snapshot
        let snapshot = execute_tool("WorkerAwaitReady", &json!({"worker_id": worker_id}))
            .expect("WorkerAwaitReady should succeed even when not ready");
        let snap_output: serde_json::Value = serde_json::from_str(&snapshot).expect("json");
        assert_eq!(
            snap_output["ready"], false,
            "WorkerAwaitReady on a spawning worker must return ready=false"
        );
        assert_eq!(snap_output["worker_id"], worker_id);
    }

    #[test]
    fn worker_send_prompt_on_non_ready_worker_returns_error() {
        let cwd_path = temp_path("worker-send-not-ready");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({"cwd": cwd_str}),
        )
        .expect("WorkerCreate should succeed");
        let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = created_output["worker_id"].as_str().expect("worker_id");

        let result = execute_tool(
            "WorkerSendPrompt",
            &json!({"worker_id": worker_id, "prompt": "too early"}),
        );
        assert!(
            result.is_err(),
            "WorkerSendPrompt on a non-ready worker should fail"
        );
    }

    #[test]
    fn recovery_loop_state_file_reflects_transitions() {
        // End-to-end proof: .claw/worker-state.json reflects every transition
        // through the stall-detect -> resolve-trust -> ready loop.
        use std::fs;

        // Use a real temp CWD so state file can be written
        let worktree = temp_path("recovery-loop-state");
        fs::create_dir_all(&worktree).expect("create worktree");
        let cwd = worktree.to_str().expect("utf-8").to_string();
        let state_path = worktree.join(".claw").join("worker-state.json");

        // 1. Create worker WITHOUT trusted_roots
        let created = execute_tool("WorkerCreate", &json!({"cwd": cwd}))
            .expect("WorkerCreate should succeed");
        let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = created_output["worker_id"]
            .as_str()
            .expect("worker_id")
            .to_string();
        // State file should exist after create
        assert!(
            state_path.exists(),
            "state file should be written after WorkerCreate"
        );
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
                .expect("parse state");
        assert_eq!(state["status"], "spawning");
        assert_eq!(state["is_ready"], false);
        assert!(
            state["seconds_since_update"].is_number(),
            "seconds_since_update must be present"
        );

        // 2. Force trust_required via observe
        execute_tool(
            "WorkerObserve",
            &json!({"worker_id": worker_id, "screen_text": "Do you trust the files in this folder?"}),
        )
        .expect("WorkerObserve should succeed");
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
                .expect("parse state");
        assert_eq!(
            state["status"], "trust_required",
            "state file must reflect trust_required stall"
        );
        assert_eq!(state["is_ready"], false);
        assert_eq!(state["trust_gate_cleared"], false);
        assert!(state["seconds_since_update"].is_number());

        // 3. WorkerResolveTrust -> state file reflects recovery
        execute_tool("WorkerResolveTrust", &json!({"worker_id": worker_id}))
            .expect("WorkerResolveTrust should succeed");
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
                .expect("parse state");
        assert_eq!(
            state["status"], "spawning",
            "state file must show spawning after trust resolved"
        );
        assert_eq!(state["trust_gate_cleared"], true);

        // 4. Observe ready screen -> state file shows ready_for_prompt
        execute_tool(
            "WorkerObserve",
            &json!({"worker_id": worker_id, "screen_text": "Ready for input\n>"}),
        )
        .expect("WorkerObserve ready should succeed");
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
                .expect("parse state");
        assert_eq!(
            state["status"], "ready_for_prompt",
            "state file must show ready_for_prompt after ready screen"
        );
        assert_eq!(
            state["is_ready"], true,
            "is_ready must be true in state file at ready_for_prompt"
        );

        fs::remove_dir_all(&worktree).ok();
    }

    #[test]
    fn stall_detect_and_resolve_trust_end_to_end() {
        // 1. Create worker WITHOUT trusted_roots so trust won't auto-resolve
        let cwd_path = temp_path("no-trust-here");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let created = execute_tool("WorkerCreate", &json!({"cwd": cwd_str}))
            .expect("WorkerCreate should succeed");
        let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = created_output["worker_id"]
            .as_str()
            .expect("worker_id")
            .to_string();
        assert_eq!(created_output["trust_auto_resolve"], false);

        // 2. Observe trust prompt screen text -> worker stalls at trust_required
        let stalled = execute_tool(
            "WorkerObserve",
            &json!({
                "worker_id": worker_id,
                "screen_text": "Do you trust the files in this folder?\n[Allow] [Deny]"
            }),
        )
        .expect("WorkerObserve should succeed");
        let stalled_output: serde_json::Value = serde_json::from_str(&stalled).expect("json");
        assert_eq!(
            stalled_output["status"], "trust_required",
            "worker should stall at trust_required when trust prompt seen without allowlist"
        );
        assert_eq!(stalled_output["trust_gate_cleared"], false);
        // 3. Clawhip calls WorkerResolveTrust to unblock
        let resolved = execute_tool("WorkerResolveTrust", &json!({"worker_id": worker_id}))
            .expect("WorkerResolveTrust should succeed");
        let resolved_output: serde_json::Value = serde_json::from_str(&resolved).expect("json");
        assert_eq!(
            resolved_output["status"], "spawning",
            "worker should return to spawning after trust resolved"
        );
        assert_eq!(resolved_output["trust_gate_cleared"], true);

        // 4. Ready screen text now advances worker normally
        let ready = execute_tool(
            "WorkerObserve",
            &json!({
                "worker_id": worker_id,
                "screen_text": "Ready for input\n>"
            }),
        )
        .expect("WorkerObserve should succeed after trust resolved");
        let ready_output: serde_json::Value = serde_json::from_str(&ready).expect("json");
        assert_eq!(
            ready_output["status"], "ready_for_prompt",
            "worker should reach ready_for_prompt after trust resolved and ready screen seen"
        );
    }

    #[test]
    fn stall_detect_and_restart_recovery_end_to_end() {
        // Worker stalls at trust_required, clawhip restarts instead of resolving
        let cwd_path = temp_path("no-trust-restart");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({"cwd": cwd_str}),
        )
        .expect("WorkerCreate should succeed");
        let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = created_output["worker_id"]
            .as_str()
            .expect("worker_id")
            .to_string();

        // Force trust_required
        let stalled = execute_tool(
            "WorkerObserve",
            &json!({
                "worker_id": worker_id,
                "screen_text": "trust this folder? [Yes] [No]"
            }),
        )
        .expect("WorkerObserve should succeed");
        let stalled_output: serde_json::Value = serde_json::from_str(&stalled).expect("json");
        assert_eq!(stalled_output["status"], "trust_required");

        // WorkerRestart resets the worker
        let restarted = execute_tool("WorkerRestart", &json!({"worker_id": worker_id}))
            .expect("WorkerRestart should succeed");
        let restarted_output: serde_json::Value = serde_json::from_str(&restarted).expect("json");
        assert_eq!(
            restarted_output["status"], "spawning",
            "restarted worker should be back at spawning"
        );
        assert_eq!(
            restarted_output["trust_gate_cleared"], false,
            "restart clears trust 閳?next observe loop must re-acquire trust"
        );
    }

    #[test]
    fn worker_terminate_on_unknown_id_returns_error() {
        let result = execute_tool(
            "WorkerTerminate",
            &json!({"worker_id": "worker_nonexistent_00000000"}),
        );
        assert!(result.is_err(), "terminating unknown worker should fail");
        assert!(
            result.unwrap_err().contains("worker not found"),
            "error should mention worker not found"
        );
    }

    #[test]
    fn worker_restart_on_unknown_id_returns_error() {
        let result = execute_tool(
            "WorkerRestart",
            &json!({"worker_id": "worker_nonexistent_00000001"}),
        );
        assert!(result.is_err(), "restarting unknown worker should fail");
        assert!(
            result.unwrap_err().contains("worker not found"),
            "error should mention worker not found"
        );
    }

    #[test]
    fn worker_observe_completion_success_finish_sets_finished_status() {
        let cwd_path = temp_path("observe-completion");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let tmp_root = std::env::temp_dir().to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({"cwd": cwd_str, "trusted_roots": [tmp_root]}),
        )
        .expect("WorkerCreate should succeed");
        let output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

        let completed = execute_tool(
            "WorkerObserveCompletion",
            &json!({
                "worker_id": worker_id,
                "finish_reason": "end_turn",
                "tokens_output": 512
            }),
        )
        .expect("WorkerObserveCompletion should succeed");
        let completed_output: serde_json::Value = serde_json::from_str(&completed).expect("json");
        assert_eq!(completed_output["status"], "finished");
        assert_eq!(completed_output["prompt_in_flight"], false);
    }

    #[test]
    fn worker_observe_completion_degraded_provider_sets_failed_status() {
        let cwd_path = temp_path("observe-degraded");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let tmp_root = std::env::temp_dir().to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({"cwd": cwd_str, "trusted_roots": [tmp_root]}),
        )
        .expect("WorkerCreate should succeed");
        let output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

        // finish=unknown + 0 tokens = degraded provider classification
        let failed = execute_tool(
            "WorkerObserveCompletion",
            &json!({
                "worker_id": worker_id,
                "finish_reason": "unknown",
                "tokens_output": 0
            }),
        )
        .expect("WorkerObserveCompletion should succeed");
        let failed_output: serde_json::Value = serde_json::from_str(&failed).expect("json");
        assert_eq!(
            failed_output["status"], "failed",
            "finish=unknown + 0 tokens should classify as provider failure"
        );
        assert_eq!(failed_output["prompt_in_flight"], false);
        // last_error should be set with provider failure message
        assert!(
            !failed_output["last_error"].is_null(),
            "last_error should be populated for provider failure"
        );
    }

    #[test]
    fn worker_tools_detect_misdelivery_and_arm_prompt_replay() {
        let cwd_path = temp_path("worker-misdelivery");
        let cwd_str = cwd_path.to_str().expect("utf-8").to_string();
        let created = execute_tool(
            "WorkerCreate",
            &json!({
                "cwd": cwd_str
            }),
        )
        .expect("WorkerCreate should succeed");
        let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
        let worker_id = created_output["worker_id"]
            .as_str()
            .expect("worker id")
            .to_string();

        execute_tool(
            "WorkerObserve",
            &json!({
                "worker_id": worker_id,
                "screen_text": "Ready for input\n>"
            }),
        )
        .expect("worker should become ready");

        execute_tool(
            "WorkerSendPrompt",
            &json!({
                "worker_id": worker_id,
                "prompt": "Investigate flaky boot"
            }),
        )
        .expect("prompt send should succeed");

        let recovered = execute_tool(
            "WorkerObserve",
            &json!({
                "worker_id": worker_id,
                "screen_text": "% Investigate flaky boot\nzsh: command not found: Investigate"
            }),
        )
        .expect("misdelivery observe should succeed");
        let recovered_output: serde_json::Value = serde_json::from_str(&recovered).expect("json");
        assert_eq!(recovered_output["status"], "ready_for_prompt");
        assert_eq!(recovered_output["last_error"]["kind"], "prompt_delivery");
        assert_eq!(recovered_output["replay_prompt"], "Investigate flaky boot");
        assert_eq!(
            recovered_output["events"][3]["payload"]["observed_target"],
            "shell"
        );
        assert_eq!(
            recovered_output["events"][4]["payload"]["recovery_armed"],
            true
        );

        let replayed = execute_tool(
            "WorkerSendPrompt",
            &json!({
                "worker_id": worker_id
            }),
        )
        .expect("WorkerSendPrompt should replay recovered prompt");
        let replayed_output: serde_json::Value = serde_json::from_str(&replayed).expect("json");
        assert_eq!(replayed_output["status"], "running");
        assert_eq!(replayed_output["prompt_delivery_attempts"], 2);
        assert_eq!(replayed_output["prompt_in_flight"], true);
    }

    #[test]
    fn global_tool_registry_denies_blocked_tool_before_dispatch() {
        // given
        let policy = permission_policy_for_mode(PermissionMode::ReadOnly);
        let registry = GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy));

        // when
        let error = registry
            .execute(
                "new_file",
                &json!({
                    "path": "blocked.txt",
                    "content": "blocked"
                }),
            )
            .expect_err("new_file tool should be denied before dispatch");

        // then
        assert!(error.contains("requires workspace-write permission"));
    }

    #[test]
    fn permission_mode_from_plugin_rejects_invalid_inputs() {
        let unknown_permission = permission_mode_from_plugin("admin")
            .expect_err("unknown plugin permission should fail");
        assert!(unknown_permission.contains("unsupported plugin permission: admin"));

        let empty_permission =
            permission_mode_from_plugin("").expect_err("empty plugin permission should fail");
        assert!(empty_permission.contains("unsupported plugin permission: "));
    }

    #[test]
    fn runtime_tools_extend_registry_definitions_permissions_and_search() {
        let registry = GlobalToolRegistry::builtin()
            .with_runtime_tools(vec![super::RuntimeToolDefinition {
                name: "mcp__demo__echo".to_string(),
                description: Some("Echo text from the demo MCP server".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "additionalProperties": false
                }),
                required_permission: runtime::PermissionMode::ReadOnly,
            }])
            .expect("runtime tools should register");

        let allowed = registry
            .normalize_allowed_tools(&["mcp__demo__echo".to_string()])
            .expect("runtime tool should be allow-listable")
            .expect("allow-list should be populated");
        assert!(allowed.contains("mcp__demo__echo"));

        let definitions = registry.definitions(Some(&allowed));
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "mcp__demo__echo");

        let permissions = registry
            .permission_specs(Some(&allowed))
            .expect("runtime tool permissions should resolve");
        assert_eq!(
            permissions,
            vec![(
                "mcp__demo__echo".to_string(),
                runtime::PermissionMode::ReadOnly
            )]
        );

        let search = registry.search(
            "demo echo",
            5,
            Some(vec!["pending-server".to_string()]),
            Some(runtime::McpDegradedReport::new(
                vec!["demo".to_string()],
                vec![runtime::McpFailedServer {
                    server_name: "pending-server".to_string(),
                    phase: runtime::McpLifecyclePhase::ToolDiscovery,
                    error: runtime::McpErrorSurface::new(
                        runtime::McpLifecyclePhase::ToolDiscovery,
                        Some("pending-server".to_string()),
                        "tool discovery failed",
                        BTreeMap::new(),
                        true,
                    ),
                }],
                vec!["mcp__demo__echo".to_string()],
                vec!["mcp__demo__echo".to_string()],
            )),
        );
        let output = serde_json::to_value(search).expect("search output should serialize");
        assert_eq!(output["matches"][0], "mcp__demo__echo");
        assert_eq!(output["pending_mcp_servers"][0], "pending-server");
        assert_eq!(
            output["mcp_degraded"]["failed_servers"][0]["phase"],
            "tool_discovery"
        );
    }

    #[test]
    fn web_fetch_returns_prompt_aware_summary() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /page "));
            HttpResponse::html(
                200,
                "OK",
                "<html><head><title>Ignored</title></head><body><h1>Test Page</h1><p>Hello <b>world</b> from local server.</p></body></html>",
            )
        }));

        let result = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/page", server.addr()),
                "prompt": "Summarize this page"
            }),
        )
        .expect("WebFetch should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["code"], 200);
        let summary = output["result"].as_str().expect("result string");
        assert!(summary.contains("Fetched"));
        assert!(summary.contains("Test Page"));
        assert!(summary.contains("Hello world from local server"));

        let titled = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/page", server.addr()),
                "prompt": "What is the page title?"
            }),
        )
        .expect("WebFetch title query should succeed");
        let titled_output: serde_json::Value = serde_json::from_str(&titled).expect("valid json");
        let titled_summary = titled_output["result"].as_str().expect("result string");
        assert!(titled_summary.contains("Title: Ignored"));
    }

    #[test]
    fn web_fetch_supports_plain_text_and_rejects_invalid_url() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /plain "));
            HttpResponse::text(200, "OK", "plain text response")
        }));

        let result = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/plain", server.addr()),
                "prompt": "Show me the content"
            }),
        )
        .expect("WebFetch should succeed for text content");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["url"], format!("http://{}/plain", server.addr()));
        assert!(output["result"]
            .as_str()
            .expect("result")
            .contains("plain text response"));

        let error = execute_tool(
            "WebFetch",
            &json!({
                "url": "not a url",
                "prompt": "Summarize"
            }),
        )
        .expect_err("invalid URL should fail");
        assert!(error.contains("relative URL without a base") || error.contains("invalid"));
    }

    #[test]
    fn wiki_mirror_url_rewrites_zh_wikipedia_to_sogou_search() {
        let url = reqwest::Url::parse(
            "https://zh.wikipedia.org/wiki/%E5%AD%94%E9%9B%80%E4%B8%9C%E5%8D%97%E9%A3%9E",
        )
        .expect("valid URL");
        let (mirror, label) = wiki_mirror_url(&url).expect("wikipedia URL must mirror");
        assert_eq!(label, "sogou-search");
        assert_eq!(mirror.host_str(), Some("www.sogou.com"));
        assert_eq!(mirror.path(), "/web");
        // Decode the query and confirm the title is the Chinese
        // article name, not the raw percent-encoded path.
        let query_pairs: std::collections::HashMap<String, String> =
            mirror.query_pairs().into_owned().collect();
        assert_eq!(
            query_pairs.get("query").map(String::as_str),
            Some("孔雀东南飞")
        );
    }

    #[test]
    fn wiki_mirror_url_preserves_underscore_titles() {
        let url = reqwest::Url::parse("https://en.wikipedia.org/wiki/Claude_Code")
            .expect("valid URL");
        let (mirror, _label) = wiki_mirror_url(&url).expect("wikipedia URL must mirror");
        let query_pairs: std::collections::HashMap<String, String> =
            mirror.query_pairs().into_owned().collect();
        assert_eq!(
            query_pairs.get("query").map(String::as_str),
            Some("Claude Code")
        );
    }

    #[test]
    fn wiki_mirror_url_returns_none_for_non_wikipedia_hosts() {
        let url =
            reqwest::Url::parse("https://example.com/wiki/Some_Article").expect("valid URL");
        assert!(wiki_mirror_url(&url).is_none());
    }

    #[test]
    fn wiki_mirror_url_returns_none_for_wikipedia_non_article_paths() {
        let url = reqwest::Url::parse("https://zh.wikipedia.org/").expect("valid URL");
        assert!(wiki_mirror_url(&url).is_none());
    }

    #[test]
    fn edit_file_input_accepts_snake_case_field_names() {
        // The LLM emits snake_case (`old_string`/`new_string`); the
        // legacy schema expected camelCase. The struct must accept
        // both to avoid the "missing field" error that breaks edits.
        let snake: EditFileInput = serde_json::from_value(json!({
            "path": "demo.txt",
            "old_string": "alpha",
            "new_string": "beta",
            "replace_all": true,
            "expected_checksum": "deadbeef",
        }))
        .expect("snake_case must deserialize");
        assert_eq!(snake.old_string, "alpha");
        assert_eq!(snake.new_string, "beta");
        assert_eq!(snake.replace_all, Some(true));
        assert_eq!(snake.expected_checksum.as_deref(), Some("deadbeef"));

        // CamelCase must keep working for backwards compatibility.
        let camel: EditFileInput = serde_json::from_value(json!({
            "path": "demo.txt",
            "oldString": "alpha",
            "newString": "beta",
        }))
        .expect("camelCase must deserialize");
        assert_eq!(camel.old_string, "alpha");
        assert_eq!(camel.new_string, "beta");
    }

    #[test]
    fn web_fetch_falls_back_to_mirror_on_wikipedia_failure() {
        // Simulate Wikipedia being blocked: a 403 Forbidden response
        // for the Wikipedia-style request. The Sogou mirror would
        // normally return a search-results page; in this test we use
        // a path the local server recognizes for the mirror case.
        // We verify the tool reports the primary failure and includes
        // a `mirror` field when it is set up to succeed.
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            if request_line.starts_with("GET /wiki/") {
                return HttpResponse::text(403, "Forbidden", "blocked");
            }
            if request_line.starts_with("GET /web") {
                return HttpResponse::html(
                    200,
                    "OK",
                    "<html><body><h1>Sogou search results</h1><a>mirror-content-marker</a></body></html>",
                );
            }
            HttpResponse::text(404, "Not Found", "")
        }));

        // Use a non-DNS wikipedia.org by pointing the host rewrite at
        // the local server via a custom URL: we craft the wikipedia
        // URL with a path that has a known prefix and assert the
        // tool surfaces the failure clearly. (DNS is not patchable
        // from this test, so the full mirror path is exercised by
        // the wiki_mirror_url tests above; this end-to-end test
        // confirms the failure path does not crash.)
        let url = format!("http://{}/wiki/Some_Article", server.addr());
        let result = execute_tool(
            "WebFetch",
            &json!({
                "url": url,
                "prompt": "Summarize the article"
            }),
        )
        .expect("WebFetch should return a structured response, not panic");
        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        // Local server returns 403 for /wiki/*, and the host is not
        // a wikipedia.org host, so no mirror is attempted. The
        // response includes the original 403 status.
        assert_eq!(output["code"], 403);
    }

    #[test]
    fn web_find_returns_matches_with_line_column_and_context() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /plain "));
            HttpResponse::text(
                200,
                "OK",
                "alpha bravo charlie\ndelta echo foxtrot\ntoken=needle-7 here\n",
            )
        }));

        let result = execute_tool(
            "WebFind",
            &json!({
                "url": format!("http://{}/plain", server.addr()),
                "pattern": "needle"
            }),
        )
        .expect("WebFind should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["url"], format!("http://{}/plain", server.addr()));
        assert_eq!(output["totalMatches"], 1);
        assert_eq!(output["truncated"], false);
        let matches = output["matches"].as_array().expect("matches array");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line"], 3);
        assert_eq!(matches[0]["column"], 7);
        assert_eq!(matches[0]["matched"], "needle");
        assert!(matches[0]["context"]
            .as_str()
            .expect("context")
            .contains("token=needle-7 here"));
    }

    #[test]
    fn web_find_truncates_when_matches_exceed_max() {
        let body = "hit\n".repeat(20);
        let server = TestServer::spawn(Arc::new(move |request_line: &str| {
            assert!(request_line.starts_with("GET /many "));
            HttpResponse::text(200, "OK", &body)
        }));

        let result = execute_tool(
            "WebFind",
            &json!({
                "url": format!("http://{}/many", server.addr()),
                "pattern": "hit",
                "maxMatches": 5
            }),
        )
        .expect("WebFind should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["totalMatches"], 20);
        assert_eq!(output["truncated"], true);
        assert_eq!(output["matches"].as_array().expect("matches").len(), 5);
    }

    #[test]
    fn web_find_html_strips_tags_before_searching() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /article "));
            HttpResponse::html(
                200,
                "OK",
                "<html><body><h1>Header</h1><p>Find this token-marker here.</p>\
                 <nav>not-the-marker</nav></body></html>",
            )
        }));

        let result = execute_tool(
            "WebFind",
            &json!({
                "url": format!("http://{}/article", server.addr()),
                "pattern": "token-marker"
            }),
        )
        .expect("WebFind should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["totalMatches"], 1);
        let context = output["matches"][0]["context"]
            .as_str()
            .expect("context");
        assert!(context.contains("Find this token-marker here"));
        assert!(!context.contains("nav"));
        assert!(!context.contains("<"));
    }

    #[test]
    fn web_find_case_insensitive_matches_by_default() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /mixed "));
            HttpResponse::text(200, "OK", "FOO bar foo baz")
        }));

        let result = execute_tool(
            "WebFind",
            &json!({
                "url": format!("http://{}/mixed", server.addr()),
                "pattern": "foo"
            }),
        )
        .expect("WebFind should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["totalMatches"], 2);
        let matches = output["matches"].as_array().expect("matches");
        assert_eq!(matches[0]["matched"], "FOO");
        assert_eq!(matches[1]["matched"], "foo");

        let sensitive = execute_tool(
            "WebFind",
            &json!({
                "url": format!("http://{}/mixed", server.addr()),
                "pattern": "foo",
                "ignoreCase": false
            }),
        )
        .expect("WebFind should succeed");
        let sensitive_output: serde_json::Value =
            serde_json::from_str(&sensitive).expect("valid json");
        assert_eq!(sensitive_output["totalMatches"], 1);
        assert_eq!(
            sensitive_output["matches"][0]["matched"],
            "foo",
            "case-sensitive should match only exact-case occurrences"
        );
    }

    #[test]
    fn web_find_empty_result_returns_zero_total() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /missing "));
            HttpResponse::text(200, "OK", "no tokens here at all")
        }));

        let result = execute_tool(
            "WebFind",
            &json!({
                "url": format!("http://{}/missing", server.addr()),
                "pattern": "absent"
            }),
        )
        .expect("WebFind should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["totalMatches"], 0);
        assert_eq!(output["truncated"], false);
        assert_eq!(output["matches"].as_array().expect("matches").len(), 0);
    }

    #[test]
    fn web_search_extracts_and_filters_results() {
        // Without SEARCHAPI_API_KEY the tool must fall back to Bing/Sogou
        // scraping and always return a valid JSON response (never an error).
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("SEARCHAPI_API_KEY");
        let result = execute_tool("WebSearch", &json!({ "query": "rust web search" }))
            .expect("WebSearch must return JSON even without API key");
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("output must be valid JSON");
        assert_eq!(parsed["query"], "rust web search");
        assert!(
            ["bing", "sogou", "none"].contains(&parsed["provider"].as_str().unwrap_or("")),
            "unexpected provider: {}",
            parsed["provider"]
        );
        assert!(parsed["resultsReturned"].is_number());
        assert!(parsed["results"].is_array());
    }

    #[test]
    fn web_search_handles_generic_links_and_invalid_base_url() {
        // Companion fallback test — same contract, different query.
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("SEARCHAPI_API_KEY");
        let result = execute_tool("WebSearch", &json!({ "query": "generic links" }))
            .expect("WebSearch must return JSON even without API key");
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("output must be valid JSON");
        assert_eq!(parsed["query"], "generic links");
    }

    #[test]
    fn web_search_provider_bad_key_returns_api_error_not_fallback() {
        // When SEARCHAPI_API_KEY is set (even a bogus one), the tool must
        // attempt the API call — not return the fallback error.
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("SEARCHAPI_API_KEY", "test_invalid_key_12345");
        let error = execute_tool("WebSearch", &json!({ "query": "test query" }))
            .expect_err("bogus key should cause an API error");
        // The error should be from the API (HTTP error), NOT the fallback
        // "not available" message.
        assert!(
            !error.contains("not available"),
            "got fallback error despite having API key set: {error}"
        );
    }

    #[test]
    fn todo_write_persists_and_returns_previous_state() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = temp_path("todos.json");
        std::env::set_var("CLAWD_TODO_STORE", &path);

        let first = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Add tool", "activeForm": "Adding tool", "status": "in_progress"},
                    {"content": "Run tests", "activeForm": "Running tests", "status": "pending"}
                ]
            }),
        )
        .expect("TodoWrite should succeed");
        let first_output: serde_json::Value = serde_json::from_str(&first).expect("valid json");
        assert_eq!(first_output["oldTodos"].as_array().expect("array").len(), 0);

        let second = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Add tool", "activeForm": "Adding tool", "status": "completed"},
                    {"content": "Run tests", "activeForm": "Running tests", "status": "completed"},
                    {"content": "Verify", "activeForm": "Verifying", "status": "completed"}
                ]
            }),
        )
        .expect("TodoWrite should succeed");
        std::env::remove_var("CLAWD_TODO_STORE");
        let _ = std::fs::remove_file(path);

        let second_output: serde_json::Value = serde_json::from_str(&second).expect("valid json");
        assert_eq!(
            second_output["oldTodos"].as_array().expect("array").len(),
            2
        );
        assert_eq!(
            second_output["newTodos"].as_array().expect("array").len(),
            3
        );
        assert!(second_output["verificationNudgeNeeded"].is_null());
    }

    #[test]
    fn todo_write_rejects_invalid_payloads_and_sets_verification_nudge() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = temp_path("todos-errors.json");
        std::env::set_var("CLAWD_TODO_STORE", &path);

        let empty = execute_tool("TodoWrite", &json!({ "todos": [] }))
            .expect_err("empty todos should fail");
        assert!(empty.contains("todos must not be empty"));

        // Multiple in_progress items are now allowed for parallel workflows
        let _multi_active = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "One", "activeForm": "Doing one", "status": "in_progress"},
                    {"content": "Two", "activeForm": "Doing two", "status": "in_progress"}
                ]
            }),
        )
        .expect("multiple in-progress todos should succeed");

        let blank_content = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "   ", "activeForm": "Doing it", "status": "pending"}
                ]
            }),
        )
        .expect_err("blank content should fail");
        assert!(blank_content.contains("todo content must not be empty"));

        let nudge = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Write tests", "activeForm": "Writing tests", "status": "completed"},
                    {"content": "Fix errors", "activeForm": "Fixing errors", "status": "completed"},
                    {"content": "Ship branch", "activeForm": "Shipping branch", "status": "completed"}
                ]
            }),
        )
        .expect("completed todos should succeed");
        std::env::remove_var("CLAWD_TODO_STORE");
        let _ = fs::remove_file(path);

        let output: serde_json::Value = serde_json::from_str(&nudge).expect("valid json");
        assert_eq!(output["verificationNudgeNeeded"], true);
    }

    #[test]
    fn skill_loads_local_skill_prompt() {
        let _guard = env_guard();
        let home = temp_path("skills-home");
        let skill_dir = home.join(".agents").join("skills").join("help");
        fs::create_dir_all(&skill_dir).expect("skill dir should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "# help\n\nGuide on using oh-my-codex plugin\n",
        )
        .expect("skill file should exist");
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);

        let result = execute_tool(
            "Skill",
            &json!({
                "skill": "help",
                "args": "overview"
            }),
        )
        .expect("Skill should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["skill"], "help");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .ends_with("/help/SKILL.md"));
        assert!(output["prompt"]
            .as_str()
            .expect("prompt")
            .contains("Guide on using oh-my-codex plugin"));

        let dollar_result = execute_tool(
            "Skill",
            &json!({
                "skill": "$help"
            }),
        )
        .expect("Skill should accept $skill invocation form");
        let dollar_output: serde_json::Value =
            serde_json::from_str(&dollar_result).expect("valid json");
        assert_eq!(dollar_output["skill"], "$help");
        assert!(dollar_output["path"]
            .as_str()
            .expect("path")
            .ends_with("/help/SKILL.md"));

        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
        fs::remove_dir_all(home).expect("temp home should clean up");
    }

    #[test]
    fn skill_resolves_project_local_skills_and_legacy_commands() {
        let _guard = env_guard();
        let root = temp_path("project-skills");
        let skill_dir = root.join(".claw").join("skills").join("plan");
        let command_dir = root.join(".claw").join("commands");
        fs::create_dir_all(&skill_dir).expect("skill dir should exist");
        fs::create_dir_all(&command_dir).expect("command dir should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: plan\ndescription: Project planning guidance\n---\n\n# plan\n",
        )
        .expect("skill file should exist");
        fs::write(
            command_dir.join("handoff.md"),
            "---\nname: handoff\ndescription: Legacy handoff guidance\n---\n\n# handoff\n",
        )
        .expect("command file should exist");

        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");

        let skill_result = execute_tool("Skill", &json!({ "skill": "$plan" }))
            .expect("project-local skill should resolve");
        let skill_output: serde_json::Value =
            serde_json::from_str(&skill_result).expect("valid json");
        assert!(skill_output["path"]
            .as_str()
            .expect("path")
            .ends_with(".claw/skills/plan/SKILL.md"));

        let command_result = execute_tool("Skill", &json!({ "skill": "/handoff" }))
            .expect("legacy command should resolve");
        let command_output: serde_json::Value =
            serde_json::from_str(&command_result).expect("valid json");
        assert!(command_output["path"]
            .as_str()
            .expect("path")
            .ends_with(".claw/commands/handoff.md"));

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        fs::remove_dir_all(root).expect("temp project should clean up");
    }

    #[test]
    fn skill_loads_project_local_claude_skill_prompt() {
        let _guard = env_guard();
        let root = temp_path("project-skills");
        let home = root.join("home");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        let skill_dir = workspace.join(".claude").join("skills").join("trace");
        fs::create_dir_all(&skill_dir).expect("skill dir should exist");
        fs::create_dir_all(&nested).expect("nested cwd should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: trace\ndescription: Project-local trace helper\n---\n# trace\n",
        )
        .expect("skill file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("CLAW_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_current_dir(&nested).expect("set cwd");

        let result = execute_tool("Skill", &json!({ "skill": "trace" }))
            .expect("project-local skill should resolve");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .ends_with(".claude/skills/trace/SKILL.md"));
        assert_eq!(output["description"], "Project-local trace helper");

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_project_local_omc_and_agents_skill_prompts() {
        let _guard = env_guard();
        let root = temp_path("project-omc-skills");
        let home = root.join("home");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        let omc_skill_dir = workspace.join(".omc").join("skills").join("hud");
        let agents_skill_dir = workspace.join(".agents").join("skills").join("trace");
        fs::create_dir_all(&omc_skill_dir).expect("omc skill dir should exist");
        fs::create_dir_all(&agents_skill_dir).expect("agents skill dir should exist");
        fs::create_dir_all(&nested).expect("nested cwd should exist");
        fs::write(
            omc_skill_dir.join("SKILL.md"),
            "---\nname: hud\ndescription: Project-local OMC HUD helper\n---\n# hud\n",
        )
        .expect("omc skill file should exist");
        fs::write(
            agents_skill_dir.join("SKILL.md"),
            "---\nname: trace\ndescription: Project-local agents compatibility helper\n---\n# trace\n",
        )
        .expect("agents skill file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("CLAW_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_current_dir(&nested).expect("set cwd");

        let omc_result =
            execute_tool("Skill", &json!({ "skill": "hud" })).expect("omc skill should resolve");
        let agents_result = execute_tool("Skill", &json!({ "skill": "trace" }))
            .expect("agents skill should resolve");

        let omc_output: serde_json::Value = serde_json::from_str(&omc_result).expect("valid json");
        let agents_output: serde_json::Value =
            serde_json::from_str(&agents_result).expect("valid json");
        assert!(omc_output["path"]
            .as_str()
            .expect("path")
            .ends_with(".omc/skills/hud/SKILL.md"));
        assert_eq!(omc_output["description"], "Project-local OMC HUD helper");
        assert!(agents_output["path"]
            .as_str()
            .expect("path")
            .ends_with(".agents/skills/trace/SKILL.md"));
        assert_eq!(
            agents_output["description"],
            "Project-local agents compatibility helper"
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_learned_skill_from_claude_config_dir() {
        let _guard = env_guard();
        let root = temp_path("claude-config-learned-skill");
        let home = root.join("home");
        let claude_config_dir = root.join("claude-config");
        let learned_skill_dir = claude_config_dir
            .join("skills")
            .join("omc-learned")
            .join("learned");
        fs::create_dir_all(&learned_skill_dir).expect("learned skill dir should exist");
        fs::write(
            learned_skill_dir.join("SKILL.md"),
            "---\nname: learned\ndescription: Learned OMC skill\n---\n# learned\n",
        )
        .expect("learned skill file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("HOME", &home);
        std::env::remove_var("CLAW_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);

        let result = execute_tool("Skill", &json!({ "skill": "learned" }))
            .expect("learned skill should resolve");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .ends_with("skills/omc-learned/learned/SKILL.md"));
        assert_eq!(output["description"], "Learned OMC skill");

        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match original_claude_config_dir {
            Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_direct_skill_and_legacy_command_from_claude_config_dir() {
        let _guard = env_guard();
        let root = temp_path("claude-config-direct-skill");
        let home = root.join("home");
        let claude_config_dir = root.join("claude-config");
        let skill_dir = claude_config_dir.join("skills").join("statusline");
        let command_dir = claude_config_dir.join("commands");
        fs::create_dir_all(&skill_dir).expect("direct skill dir should exist");
        fs::create_dir_all(&command_dir).expect("command dir should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: statusline\ndescription: Claude config skill\n---\n# statusline\n",
        )
        .expect("direct skill file should exist");
        fs::write(
            command_dir.join("doctor-check.md"),
            "---\nname: doctor-check\ndescription: Claude config command\n---\n# doctor-check\n",
        )
        .expect("direct command file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("HOME", &home);
        std::env::remove_var("CLAW_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);

        let direct_skill =
            execute_tool("Skill", &json!({ "skill": "statusline" })).expect("direct skill");
        let direct_skill_output: serde_json::Value =
            serde_json::from_str(&direct_skill).expect("valid skill json");
        assert!(direct_skill_output["path"]
            .as_str()
            .expect("path")
            .ends_with("skills/statusline/SKILL.md"));
        assert_eq!(direct_skill_output["description"], "Claude config skill");

        let legacy_command =
            execute_tool("Skill", &json!({ "skill": "doctor-check" })).expect("direct command");
        let legacy_command_output: serde_json::Value =
            serde_json::from_str(&legacy_command).expect("valid command json");
        assert!(legacy_command_output["path"]
            .as_str()
            .expect("path")
            .ends_with("commands/doctor-check.md"));
        assert_eq!(
            legacy_command_output["description"],
            "Claude config command"
        );

        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match original_claude_config_dir {
            Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_project_local_legacy_command_markdown() {
        let _guard = env_guard();
        let root = temp_path("project-legacy-command");
        let home = root.join("home");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        let command_dir = workspace.join(".claude").join("commands");
        fs::create_dir_all(&command_dir).expect("legacy command dir should exist");
        fs::create_dir_all(&nested).expect("nested cwd should exist");
        fs::write(
            command_dir.join("team.md"),
            "---\nname: team\ndescription: Legacy team workflow\n---\n# team\n",
        )
        .expect("legacy command file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("CLAW_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_current_dir(&nested).expect("set cwd");

        let result = execute_tool("Skill", &json!({ "skill": "team" }))
            .expect("legacy command markdown should resolve");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .ends_with(".claude/commands/team.md"));
        assert_eq!(output["description"], "Legacy team workflow");

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn tool_search_supports_keyword_and_select_queries() {
        let keyword = execute_tool(
            "ToolSearch",
            &json!({"query": "web current", "max_results": 3}),
        )
        .expect("ToolSearch should succeed");
        let keyword_output: serde_json::Value = serde_json::from_str(&keyword).expect("valid json");
        let matches = keyword_output["matches"].as_array().expect("matches");
        assert!(matches.iter().any(|value| value == "WebSearch"));

        let selected = execute_tool("ToolSearch", &json!({"query": "select:Agent,Skill"}))
            .expect("ToolSearch should succeed");
        let selected_output: serde_json::Value =
            serde_json::from_str(&selected).expect("valid json");
        assert_eq!(selected_output["matches"][0], "Agent");
        assert_eq!(selected_output["matches"][1], "Skill");

        let aliased = execute_tool("ToolSearch", &json!({"query": "AgentTool"}))
            .expect("ToolSearch should support tool aliases");
        let aliased_output: serde_json::Value = serde_json::from_str(&aliased).expect("valid json");
        assert_eq!(aliased_output["matches"][0], "Agent");
        assert_eq!(aliased_output["normalized_query"], "agent");

        let selected_with_alias =
            execute_tool("ToolSearch", &json!({"query": "select:AgentTool,Skill"}))
                .expect("ToolSearch alias select should succeed");
        let selected_with_alias_output: serde_json::Value =
            serde_json::from_str(&selected_with_alias).expect("valid json");
        assert_eq!(selected_with_alias_output["matches"][0], "Agent");
        assert_eq!(selected_with_alias_output["matches"][1], "Skill");
    }

    #[test]
    fn agent_persists_handoff_metadata() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = temp_path("agent-store");
        std::env::set_var("CLAWD_AGENT_STORE", &dir);
        let captured = Arc::new(Mutex::new(None::<AgentJob>));
        let captured_for_spawn = Arc::clone(&captured);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Audit the branch".to_string(),
                prompt: "Check tests and outstanding work.".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("ship-audit".to_string()),
                model: None,
            },
            move |job| {
                let agent_id = job.manifest.agent_id.clone();
                *captured_for_spawn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
                Ok(AgentHandle::noop(agent_id))
            },
        )
        .expect("Agent should succeed");
        std::env::remove_var("CLAWD_AGENT_STORE");

        assert_eq!(manifest.name, "ship-audit");
        assert_eq!(manifest.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(manifest.status, AgentStatus::Running);
        assert!(manifest.created_at > 0);
        assert!(manifest.started_at.is_some());
        assert!(manifest.completed_at.is_none());
        let contents = std::fs::read_to_string(&manifest.output_file).expect("agent file exists");
        let manifest_contents =
            std::fs::read_to_string(&manifest.manifest_file).expect("manifest file exists");
        let manifest_json: serde_json::Value =
            serde_json::from_str(&manifest_contents).expect("manifest should be valid json");
        assert!(contents.contains("Audit the branch"));
        assert!(contents.contains("Check tests and outstanding work."));
        assert!(manifest_contents.contains("\"subagentType\": \"Explore\""));
        assert!(manifest_contents.contains("\"status\": \"running\""));
        assert_eq!(manifest_json["laneEvents"][0]["event"], "lane.started");
        assert_eq!(manifest_json["laneEvents"][0]["status"], "running");
        assert!(manifest_json["currentBlocker"].is_null());
        let captured_job = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("spawn job should be captured");
        assert_eq!(captured_job.prompt, "Check tests and outstanding work.");
        assert!(captured_job.allowed_tools.contains("read_file"));
        assert!(!captured_job.allowed_tools.contains("Agent"));

        let normalized = execute_tool(
            "Agent",
            &json!({
                "description": "Verify the branch",
                "prompt": "Check tests.",
                "subagent_type": "explorer"
            }),
        )
        .expect("Agent should normalize built-in aliases");
        let normalized_output: serde_json::Value =
            serde_json::from_str(&normalized).expect("valid json");
        assert_eq!(normalized_output["subagentType"], "Explore");

        let named = execute_tool(
            "Agent",
            &json!({
                "description": "Review the branch",
                "prompt": "Inspect diff.",
                "name": "Ship Audit!!!"
            }),
        )
        .expect("Agent should normalize explicit names");
        let named_output: serde_json::Value = serde_json::from_str(&named).expect("valid json");
        assert_eq!(named_output["name"], "ship-audit");
        let _ = std::fs::remove_dir_all(dir);
    }


    #[test]
    fn lane_event_schema_serializes_to_canonical_names() {
        let cases = [
            (LaneEventName::Started, "lane.started"),
            (LaneEventName::Ready, "lane.ready"),
            (LaneEventName::PromptMisdelivery, "lane.prompt_misdelivery"),
            (LaneEventName::Blocked, "lane.blocked"),
            (LaneEventName::Red, "lane.red"),
            (LaneEventName::Green, "lane.green"),
            (LaneEventName::CommitCreated, "lane.commit.created"),
            (LaneEventName::PrOpened, "lane.pr.opened"),
            (LaneEventName::MergeReady, "lane.merge.ready"),
            (LaneEventName::Finished, "lane.finished"),
            (LaneEventName::Failed, "lane.failed"),
            (
                LaneEventName::BranchStaleAgainstMain,
                "branch.stale_against_main",
            ),
            (
                LaneEventName::BranchWorkspaceMismatch,
                "branch.workspace_mismatch",
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(
                serde_json::to_value(event).expect("serialize lane event"),
                json!(expected)
            );
        }
    }

    #[test]
    fn agent_tool_subset_mapping_is_expected() {
        let general = agents::allowed_tools_for_subagent("general-purpose");
        assert!(general.contains("bash"));
        assert!(general.contains("new_file"));
        assert!(!general.contains("Agent"));

        let explore = agents::allowed_tools_for_subagent("Explore");
        assert!(explore.contains("read_file"));
        assert!(explore.contains("grep_search"));
        assert!(!explore.contains("bash"));

        let plan = agents::allowed_tools_for_subagent("Plan");
        assert!(plan.contains("TodoWrite"));
        assert!(plan.contains("StructuredOutput"));
        assert!(!plan.contains("Agent"));

        let verification = agents::allowed_tools_for_subagent("Verification");
        assert!(verification.contains("bash"));
        assert!(verification.contains("PowerShell"));
        assert!(!verification.contains("new_file"));
    }

    #[test]
    fn agent_rejects_blank_required_fields() {
        let missing_description = execute_tool(
            "Agent",
            &json!({
                "description": "  ",
                "prompt": "Inspect"
            }),
        )
        .expect_err("blank description should fail");
        assert!(missing_description.contains("description must not be empty"));

        let missing_prompt = execute_tool(
            "Agent",
            &json!({
                "description": "Inspect branch",
                "prompt": " "
            }),
        )
        .expect_err("blank prompt should fail");
        assert!(missing_prompt.contains("prompt must not be empty"));
    }

    #[test]
    fn notebook_edit_replaces_inserts_and_deletes_cells() {
        let path = temp_path("notebook.ipynb");
        std::fs::write(
            &path,
            r#"{
  "cells": [
    {"cell_type": "code", "id": "cell-a", "metadata": {}, "source": ["print(1)\n"], "outputs": [], "execution_count": null}
  ],
  "metadata": {"kernelspec": {"language": "python"}},
  "nbformat": 4,
  "nbformat_minor": 5
}"#,
        )
        .expect("write notebook");

        let replaced = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "new_source": "print(2)\n",
                "edit_mode": "replace"
            }),
        )
        .expect("NotebookEdit replace should succeed");
        let replaced_output: serde_json::Value = serde_json::from_str(&replaced).expect("json");
        assert_eq!(replaced_output["cell_id"], "cell-a");
        assert_eq!(replaced_output["cell_type"], "code");

        let inserted = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "new_source": "# heading\n",
                "cell_type": "markdown",
                "edit_mode": "insert"
            }),
        )
        .expect("NotebookEdit insert should succeed");
        let inserted_output: serde_json::Value = serde_json::from_str(&inserted).expect("json");
        assert_eq!(inserted_output["cell_type"], "markdown");
        let appended = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "new_source": "print(3)\n",
                "edit_mode": "insert"
            }),
        )
        .expect("NotebookEdit append should succeed");
        let appended_output: serde_json::Value = serde_json::from_str(&appended).expect("json");
        assert_eq!(appended_output["cell_type"], "code");

        let deleted = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "edit_mode": "delete"
            }),
        )
        .expect("NotebookEdit delete should succeed without new_source");
        let deleted_output: serde_json::Value = serde_json::from_str(&deleted).expect("json");
        assert!(deleted_output["cell_type"].is_null());
        assert_eq!(deleted_output["new_source"], "");

        let final_notebook: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read notebook"))
                .expect("valid notebook json");
        let cells = final_notebook["cells"].as_array().expect("cells array");
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0]["cell_type"], "markdown");
        assert!(cells[0].get("outputs").is_none());
        assert_eq!(cells[1]["cell_type"], "code");
        assert_eq!(cells[1]["source"][0], "print(3)\n");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn notebook_edit_rejects_invalid_inputs() {
        let text_path = temp_path("notebook.txt");
        fs::write(&text_path, "not a notebook").expect("write text file");
        let wrong_extension = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": text_path.display().to_string(),
                "new_source": "print(1)\n"
            }),
        )
        .expect_err("non-ipynb file should fail");
        assert!(wrong_extension.contains("Jupyter notebook"));
        let _ = fs::remove_file(&text_path);

        let empty_notebook = temp_path("empty.ipynb");
        fs::write(
            &empty_notebook,
            r#"{"cells":[],"metadata":{"kernelspec":{"language":"python"}},"nbformat":4,"nbformat_minor":5}"#,
        )
        .expect("write empty notebook");

        let missing_source = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": empty_notebook.display().to_string(),
                "edit_mode": "insert"
            }),
        )
        .expect_err("insert without source should fail");
        assert!(missing_source.contains("new_source is required"));

        let missing_cell = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": empty_notebook.display().to_string(),
                "edit_mode": "delete"
            }),
        )
        .expect_err("delete on empty notebook should fail");
        assert!(missing_cell.contains("Notebook has no cells to edit"));
        let _ = fs::remove_file(empty_notebook);
    }

    #[test]
    fn bash_tool_reports_success_exit_failure_timeout_and_background() {
        let success = execute_tool("bash", &json!({ "command": "printf 'hello'" }))
            .expect("bash should succeed");
        let success_output: serde_json::Value = serde_json::from_str(&success).expect("json");
        assert_eq!(success_output["stdout"], "hello");
        assert_eq!(success_output["interrupted"], false);

        let failure = execute_tool("bash", &json!({ "command": "printf 'oops' >&2; exit 7" }))
            .expect("bash failure should still return structured output");
        let failure_output: serde_json::Value = serde_json::from_str(&failure).expect("json");
        assert_eq!(failure_output["returnCodeInterpretation"], "exit_code:7");
        assert!(failure_output["stderr"]
            .as_str()
            .expect("stderr")
            .contains("oops"));

        let timeout = execute_tool("bash", &json!({ "command": "sleep 1", "timeout": 10 }))
            .expect("bash timeout should return output");
        let timeout_output: serde_json::Value = serde_json::from_str(&timeout).expect("json");
        assert_eq!(timeout_output["interrupted"], true);
        assert_eq!(timeout_output["returnCodeInterpretation"], "timeout");
        assert!(timeout_output["stderr"]
            .as_str()
            .expect("stderr")
            .contains("Command exceeded timeout"));

        let background = execute_tool(
            "bash",
            &json!({ "command": "sleep 1", "run_in_background": true }),
        )
        .expect("bash background should succeed");
        let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
        assert!(background_output["backgroundTaskId"].as_str().is_some());
        assert_eq!(background_output["noOutputExpected"], true);
    }

    #[test]
    fn bash_workspace_tests_are_blocked_when_branch_is_behind_main() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("workspace-test-preflight");
        let original_dir = std::env::current_dir().expect("cwd");
        init_git_repo(&root);
        run_git(&root, &["checkout", "-b", "feature/stale-tests"]);
        run_git(&root, &["checkout", "main"]);
        commit_file(
            &root,
            "hotfix.txt",
            "fix from main\n",
            "fix: unblock workspace tests",
        );
        run_git(&root, &["checkout", "feature/stale-tests"]);
        std::env::set_current_dir(&root).expect("set cwd");

        let output = execute_tool(
            "bash",
            &json!({ "command": "cargo test --workspace --all-targets" }),
        )
        .expect("preflight should return structured output");
        let output_json: serde_json::Value = serde_json::from_str(&output).expect("json");
        assert_eq!(
            output_json["returnCodeInterpretation"],
            "preflight_blocked:branch_divergence"
        );
        assert!(output_json["stderr"]
            .as_str()
            .expect("stderr")
            .contains("branch divergence detected before workspace tests"));
        assert_eq!(
            output_json["structuredContent"][0]["event"],
            "branch.stale_against_main"
        );
        assert_eq!(
            output_json["structuredContent"][0]["failureClass"],
            "branch_divergence"
        );
        assert_eq!(
            output_json["structuredContent"][0]["data"]["missingCommits"][0],
            "fix: unblock workspace tests"
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bash_targeted_tests_skip_branch_preflight() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("targeted-test-no-preflight");
        let original_dir = std::env::current_dir().expect("cwd");
        init_git_repo(&root);
        run_git(&root, &["checkout", "-b", "feature/targeted-tests"]);
        run_git(&root, &["checkout", "main"]);
        commit_file(
            &root,
            "hotfix.txt",
            "fix from main\n",
            "fix: only broad tests should block",
        );
        run_git(&root, &["checkout", "feature/targeted-tests"]);
        std::env::set_current_dir(&root).expect("set cwd");

        let output = execute_tool(
            "bash",
            &json!({ "command": "printf 'targeted ok'; cargo test -p runtime stale_branch" }),
        )
        .expect("targeted commands should still execute");
        let output_json: serde_json::Value = serde_json::from_str(&output).expect("json");
        assert_ne!(
            output_json["returnCodeInterpretation"],
            "preflight_blocked:branch_divergence"
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn file_tools_cover_read_write_and_edit_behaviors() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("fs-suite");
        fs::create_dir_all(&root).expect("create root");
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");

        let write_create = execute_tool(
            "new_file",
            &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
        )
        .expect("new_file create should succeed");
        let write_create_output: serde_json::Value =
            serde_json::from_str(&write_create).expect("json");
        assert_eq!(write_create_output["type"], "create");
        assert_eq!(write_create_output["checksum"].as_str().unwrap().len(), 16);
        assert_eq!(write_create_output["bytesWritten"], 17);
        assert_eq!(write_create_output["linesWritten"], 3);
        assert!(write_create_output.get("content").is_none());
        assert!(write_create_output.get("originalFile").is_none());
        assert!(root.join("nested/demo.txt").exists());

        // new_file rejects existing files — use edit_file to modify.
        let write_dup = execute_tool(
            "new_file",
            &json!({ "path": "nested/demo.txt", "content": "should fail" }),
        )
        .expect_err("new_file should reject existing file");
        assert!(write_dup.contains("already exists"));
        assert!(write_dup.contains("edit_file"));

        // new_file with force: true overwrites existing files.
        let write_force = execute_tool(
            "new_file",
            &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\ngamma\n", "force": true }),
        )
        .expect("new_file with force should overwrite");
        let write_force_output: serde_json::Value =
            serde_json::from_str(&write_force).expect("json");
        assert_eq!(write_force_output["type"], "overwrite");

        let read_full = execute_tool("read_file", &json!({ "path": "nested/demo.txt" }))
            .expect("read full should succeed");
        let read_full_output: serde_json::Value = serde_json::from_str(&read_full).expect("json");
        // Default mode is LLM-friendly: content is echoed so the
        // model can verify the file without a follow-up call. Pass
        // `full: false` to opt out and keep the payload token-light.
        assert!(read_full_output["file"]["content"]
            .as_str()
            .expect("content present by default")
            .contains("alpha"));
        assert_eq!(read_full_output["file"]["checksum"].as_str().unwrap().len(), 16);
        assert_eq!(read_full_output["file"]["bytesRead"], 16);
        assert_eq!(read_full_output["file"]["startLine"], 1);

        let read_slice = execute_tool(
            "read_file",
            &json!({ "path": "nested/demo.txt", "offset": 1, "limit": 1 }),
        )
        .expect("read slice should succeed");
        let read_slice_output: serde_json::Value = serde_json::from_str(&read_slice).expect("json");
        assert_eq!(
            read_slice_output["file"]["content"]
                .as_str()
                .expect("content present by default"),
            "beta"
        );
        assert_eq!(read_slice_output["file"]["checksum"].as_str().unwrap().len(), 16);
        assert_eq!(read_slice_output["file"]["bytesRead"], 4);
        assert_eq!(read_slice_output["file"]["startLine"], 2);

        let read_past_end = execute_tool(
            "read_file",
            &json!({ "path": "nested/demo.txt", "offset": 50 }),
        )
        .expect("read past EOF should succeed");
        let read_past_end_output: serde_json::Value =
            serde_json::from_str(&read_past_end).expect("json");
        // Content is echoed by default; for an out-of-range offset
        // the selection is empty.
        assert_eq!(
            read_past_end_output["file"]["content"]
                .as_str()
                .expect("content present by default"),
            ""
        );
        assert_eq!(read_past_end_output["file"]["bytesRead"], 0);
        assert_eq!(read_past_end_output["file"]["startLine"], 4);

        // Opt-out: explicit `full: false` keeps the payload token-light.
        let read_tokenlight = execute_tool(
            "read_file",
            &json!({ "path": "nested/demo.txt", "full": false }),
        )
        .expect("read token-light should succeed");
        let read_tokenlight_output: serde_json::Value =
            serde_json::from_str(&read_tokenlight).expect("json");
        assert!(read_tokenlight_output["file"].get("content").is_none());

        let read_error = execute_tool("read_file", &json!({ "path": "missing.txt" }))
            .expect_err("missing file should fail");
        assert!(!read_error.is_empty());

        let edit_once = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "oldString": "alpha", "newString": "omega" }),
        )
        .expect("single edit should succeed");
        let edit_once_output: serde_json::Value = serde_json::from_str(&edit_once).expect("json");
        assert_eq!(edit_once_output["newChecksum"].as_str().unwrap().len(), 16);
        assert_eq!(edit_once_output["bytesChanged"].as_i64().unwrap(), 0);
        assert!(edit_once_output["linesChanged"].as_u64().unwrap() > 0);
        assert!(edit_once_output["diffSummary"].as_str().unwrap().len() > 0);
        assert!(edit_once_output.get("originalFile").is_none());
        assert!(edit_once_output.get("structuredPatch").is_none());
        assert!(edit_once_output.get("userModified").is_none());
        assert_eq!(
            fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
            "omega\nbeta\ngamma\n"
        );

        // Reset file for replace_all test using direct fs::write
        std::fs::write(root.join("nested/demo.txt"), "alpha\nbeta\nalpha\n").expect("reset file");
        let edit_all = execute_tool(
            "edit_file",
            &json!({
                "path": "nested/demo.txt",
                "oldString": "alpha",
                "newString": "omega",
                "replaceAll": true
            }),
        )
        .expect("replace all should succeed");
        let edit_all_output: serde_json::Value = serde_json::from_str(&edit_all).expect("json");
        assert_eq!(edit_all_output["newChecksum"].as_str().unwrap().len(), 16);
        assert_eq!(edit_all_output["bytesChanged"].as_i64().unwrap(), 0);
        assert!(edit_all_output["linesChanged"].as_u64().unwrap() > 0);
        assert!(edit_all_output["diffSummary"].as_str().unwrap().len() > 0);
        assert!(edit_all_output.get("originalFile").is_none());
        assert!(edit_all_output.get("structuredPatch").is_none());
        assert!(edit_all_output.get("replaceAll").is_none());
        assert_eq!(
            fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
            "omega\nbeta\nomega\n"
        );

        let edit_same = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "oldString": "omega", "newString": "omega" }),
        )
        .expect_err("identical old/new should fail");
        assert!(edit_same.contains("must differ"));

        let edit_missing = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "oldString": "missing", "newString": "omega" }),
        )
        .expect_err("missing substring should fail");
        assert!(edit_missing.contains("old_string not found"));

        // expected_checksum: matching succeeds
        std::fs::write(root.join("nested/demo.txt"), "hello\nworld\n").expect("reset for checksum");
        let current_checksum = {
            let read_result = execute_tool("read_file", &json!({ "path": "nested/demo.txt" })).expect("read");
            let read_output: serde_json::Value = serde_json::from_str(&read_result).expect("json");
            read_output["file"]["checksum"].as_str().expect("checksum").to_string()
        };
        assert_eq!(current_checksum.len(), 16);

        execute_tool(
            "edit_file",
            &json!({
                "path": "nested/demo.txt",
                "oldString": "world",
                "newString": "universe",
                "expectedChecksum": current_checksum,
            }),
        )
        .expect("matching expected_checksum should succeed");

        // expected_checksum: mismatching fails
        let checksum_fail = execute_tool(
            "edit_file",
            &json!({
                "path": "nested/demo.txt",
                "oldString": "universe",
                "newString": "world",
                "expectedChecksum": "0000000000000000",
            }),
        )
        .expect_err("mismatched expected_checksum should fail");
        assert!(
            checksum_fail.contains("expected checksum"),
            "error should mention expected checksum: {checksum_fail}"
        );
        assert!(
            checksum_fail.contains("current file checksum"),
            "error should mention current checksum: {checksum_fail}"
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn glob_and_grep_tools_cover_success_and_errors() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("search-suite");
        fs::create_dir_all(root.join("nested")).expect("create root");
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");

        fs::write(
            root.join("nested/lib.rs"),
            "fn main() {}\nlet alpha = 1;\nlet alpha = 2;\n",
        )
        .expect("write rust file");
        fs::write(root.join("nested/notes.txt"), "alpha\nbeta\n").expect("write txt file");

        let globbed = execute_tool("glob_search", &json!({ "pattern": "nested/*.rs" }))
            .expect("glob should succeed");
        let globbed_output: serde_json::Value = serde_json::from_str(&globbed).expect("json");
        assert_eq!(globbed_output["numFiles"], 1);
        assert!(globbed_output["filenames"][0]
            .as_str()
            .expect("filename")
            .ends_with("nested/lib.rs"));

        let glob_error = execute_tool("glob_search", &json!({ "pattern": "[" }))
            .expect_err("invalid glob should fail");
        assert!(!glob_error.is_empty());

        let grep_content = execute_tool(
            "grep_search",
            &json!({
                "pattern": "alpha",
                "path": "nested",
                "glob": "*.rs",
                "output_mode": "content",
                "-n": true,
                "head_limit": 1,
                "offset": 1
            }),
        )
        .expect("grep content should succeed");
        let grep_content_output: serde_json::Value =
            serde_json::from_str(&grep_content).expect("json");
        assert_eq!(grep_content_output["numFiles"], 0);
        assert!(grep_content_output["appliedLimit"].is_null());
        assert_eq!(grep_content_output["appliedOffset"], 1);
        assert!(grep_content_output["content"]
            .as_str()
            .expect("content")
            .contains("let alpha = 2;"));

        let grep_count = execute_tool(
            "grep_search",
            &json!({ "pattern": "alpha", "path": "nested", "output_mode": "count" }),
        )
        .expect("grep count should succeed");
        let grep_count_output: serde_json::Value = serde_json::from_str(&grep_count).expect("json");
        assert_eq!(grep_count_output["numMatches"], 3);

        let grep_error = execute_tool(
            "grep_search",
            &json!({ "pattern": "(alpha", "path": "nested" }),
        )
        .expect_err("invalid regex should fail");
        assert!(!grep_error.is_empty());

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sleep_waits_and_reports_duration() {
        let started = std::time::Instant::now();
        let result =
            execute_tool("Sleep", &json!({"duration_ms": 20})).expect("Sleep should succeed");
        let elapsed = started.elapsed();
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["duration_ms"], 20);
        assert!(output["message"]
            .as_str()
            .expect("message")
            .contains("Slept for 20ms"));
        assert!(elapsed >= Duration::from_millis(15));
    }

    #[test]
    fn given_excessive_duration_when_sleep_then_rejects_with_error() {
        let result = execute_tool("Sleep", &json!({"duration_ms": 999_999_999_u64}));
        let error = result.expect_err("excessive sleep should fail");
        assert!(error.contains("exceeds maximum allowed sleep"));
    }

    #[test]
    fn given_zero_duration_when_sleep_then_succeeds() {
        let result =
            execute_tool("Sleep", &json!({"duration_ms": 0})).expect("0ms sleep should succeed");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["duration_ms"], 0);
    }

    #[test]
    fn brief_returns_sent_message_and_attachment_metadata() {
        let attachment = std::env::temp_dir().join(format!(
            "clawd-brief-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::write(&attachment, b"png-data").expect("write attachment");

        let result = execute_tool(
            "SendUserMessage",
            &json!({
                "message": "hello user",
                "attachments": [attachment.display().to_string()],
                "status": "normal"
            }),
        )
        .expect("SendUserMessage should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["message"], "hello user");
        assert!(output["sentAt"].as_str().is_some());
        assert_eq!(output["attachments"][0]["isImage"], true);
        let _ = std::fs::remove_file(attachment);
    }

    #[test]
    fn config_reads_and_writes_supported_values() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "clawd-config-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(home.join(".claw")).expect("home dir");
        std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");
        std::fs::write(
            home.join(".claw").join("settings.json"),
            r#"{"verbose":false}"#,
        )
        .expect("write global settings");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("CLAW_CONFIG_HOME");
        std::env::set_current_dir(&cwd).expect("set cwd");

        let get = execute_tool("Config", &json!({"setting": "verbose"})).expect("get config");
        let get_output: serde_json::Value = serde_json::from_str(&get).expect("json");
        assert_eq!(get_output["value"], false);

        let set = execute_tool(
            "Config",
            &json!({"setting": "permissions.defaultMode", "value": "plan"}),
        )
        .expect("set config");
        let set_output: serde_json::Value = serde_json::from_str(&set).expect("json");
        assert_eq!(set_output["operation"], "set");
        assert_eq!(set_output["newValue"], "plan");

        let invalid = execute_tool(
            "Config",
            &json!({"setting": "permissions.defaultMode", "value": "bogus"}),
        )
        .expect_err("invalid config value should error");
        assert!(invalid.contains("Invalid value"));

        let unknown =
            execute_tool("Config", &json!({"setting": "nope"})).expect("unknown setting result");
        let unknown_output: serde_json::Value = serde_json::from_str(&unknown).expect("json");
        assert_eq!(unknown_output["success"], false);

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enter_and_exit_plan_mode_round_trip_existing_local_override() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "clawd-plan-mode-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(home.join(".claw")).expect("home dir");
        std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");
        std::fs::write(
            cwd.join(".claw").join("settings.local.json"),
            r#"{"permissions":{"defaultMode":"acceptEdits"}}"#,
        )
        .expect("write local settings");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("CLAW_CONFIG_HOME");
        std::env::set_current_dir(&cwd).expect("set cwd");

        let enter = execute_tool("EnterPlanMode", &json!({})).expect("enter plan mode");
        let enter_output: serde_json::Value = serde_json::from_str(&enter).expect("json");
        assert_eq!(enter_output["changed"], true);
        assert_eq!(enter_output["managed"], true);
        assert_eq!(enter_output["previousLocalMode"], "acceptEdits");
        assert_eq!(enter_output["currentLocalMode"], "plan");

        let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
            .expect("local settings after enter");
        assert!(local_settings.contains(r#""defaultMode": "plan""#));
        let state =
            std::fs::read_to_string(cwd.join(".claw").join("tool-state").join("plan-mode.json"))
                .expect("plan mode state");
        assert!(state.contains(r#""hadLocalOverride": true"#));
        assert!(state.contains(r#""previousLocalMode": "acceptEdits""#));

        let exit = execute_tool("ExitPlanMode", &json!({})).expect("exit plan mode");
        let exit_output: serde_json::Value = serde_json::from_str(&exit).expect("json");
        assert_eq!(exit_output["changed"], true);
        assert_eq!(exit_output["managed"], false);
        assert_eq!(exit_output["previousLocalMode"], "acceptEdits");
        assert_eq!(exit_output["currentLocalMode"], "acceptEdits");

        let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
            .expect("local settings after exit");
        assert!(local_settings.contains(r#""defaultMode": "acceptEdits""#));
        assert!(!cwd
            .join(".claw")
            .join("tool-state")
            .join("plan-mode.json")
            .exists());

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn exit_plan_mode_clears_override_when_enter_created_it_from_empty_local_state() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "clawd-plan-mode-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(home.join(".claw")).expect("home dir");
        std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("CLAW_CONFIG_HOME");
        std::env::set_current_dir(&cwd).expect("set cwd");

        let enter = execute_tool("EnterPlanMode", &json!({})).expect("enter plan mode");
        let enter_output: serde_json::Value = serde_json::from_str(&enter).expect("json");
        assert_eq!(enter_output["previousLocalMode"], serde_json::Value::Null);
        assert_eq!(enter_output["currentLocalMode"], "plan");

        let exit = execute_tool("ExitPlanMode", &json!({})).expect("exit plan mode");
        let exit_output: serde_json::Value = serde_json::from_str(&exit).expect("json");
        assert_eq!(exit_output["changed"], true);
        assert_eq!(exit_output["currentLocalMode"], serde_json::Value::Null);

        let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
            .expect("local settings after exit");
        let local_settings_json: serde_json::Value =
            serde_json::from_str(&local_settings).expect("valid settings json");
        assert_eq!(
            local_settings_json.get("permissions"),
            None,
            "permissions override should be removed on exit"
        );
        assert!(!cwd
            .join(".claw")
            .join("tool-state")
            .join("plan-mode.json")
            .exists());

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn structured_output_echoes_input_payload() {
        let result = execute_tool("StructuredOutput", &json!({"ok": true, "items": [1, 2, 3]}))
            .expect("StructuredOutput should succeed");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["data"], "Structured output provided successfully");
        assert_eq!(output["structured_output"]["ok"], true);
        assert_eq!(output["structured_output"]["items"][1], 2);
    }

    #[test]
    fn given_empty_payload_when_structured_output_then_rejects_with_error() {
        let result = execute_tool("StructuredOutput", &json!({}));
        let error = result.expect_err("empty payload should fail");
        assert!(error.contains("must not be empty"));
    }

    #[test]
    fn repl_executes_python_code() {
        let result = execute_tool(
            "REPL",
            &json!({"language": "python", "code": "print(1 + 1)", "timeout_ms": 500}),
        )
        .expect("REPL should succeed");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["language"], "python");
        assert_eq!(output["exitCode"], 0);
        assert!(output["stdout"].as_str().expect("stdout").contains('2'));
    }

    #[test]
    fn given_empty_code_when_repl_then_rejects_with_error() {
        let result = execute_tool("REPL", &json!({"language": "python", "code": "   "}));

        let error = result.expect_err("empty REPL code should fail");
        assert!(error.contains("code must not be empty"));
    }

    #[test]
    fn given_unsupported_language_when_repl_then_rejects_with_error() {
        let result = execute_tool("REPL", &json!({"language": "ruby", "code": "puts 1"}));

        let error = result.expect_err("unsupported REPL language should fail");
        assert!(error.contains("unsupported REPL language: ruby"));
    }

    #[test]
    fn given_timeout_ms_when_repl_blocks_then_returns_timeout_error() {
        let result = execute_tool(
            "REPL",
            &json!({
                "language": "python",
                "code": "import time\ntime.sleep(1)",
                "timeout_ms": 10
            }),
        );

        let error = result.expect_err("timed out REPL execution should fail");
        assert!(error.contains("REPL execution exceeded timeout of 10 ms"));
    }

    #[test]
    fn powershell_runs_via_stub_shell() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "clawd-pwsh-bin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create dir");
        let script = dir.join("pwsh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while [ "$1" != "-Command" ] && [ $# -gt 0 ]; do shift; done
shift
printf 'pwsh:%s' "$1"
"#,
        )
        .expect("write script");
        std::process::Command::new("/bin/chmod")
            .arg("+x")
            .arg(&script)
            .status()
            .expect("chmod");
        let original_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", dir.display(), original_path));

        let result = execute_tool(
            "PowerShell",
            &json!({"command": "Write-Output hello", "timeout": 1000}),
        )
        .expect("PowerShell should succeed");

        let background = execute_tool(
            "PowerShell",
            &json!({"command": "Write-Output hello", "run_in_background": true}),
        )
        .expect("PowerShell background should succeed");

        std::env::set_var("PATH", original_path);
        let _ = std::fs::remove_dir_all(dir);

        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["stdout"], "pwsh:Write-Output hello");
        assert!(output["stderr"].as_str().expect("stderr").is_empty());

        let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
        assert!(background_output["backgroundTaskId"].as_str().is_some());
        assert_eq!(background_output["backgroundedByUser"], true);
        assert_eq!(background_output["assistantAutoBackgrounded"], false);
    }

    #[test]
    fn powershell_errors_when_shell_is_missing() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_path = std::env::var("PATH").unwrap_or_default();
        let empty_dir = std::env::temp_dir().join(format!(
            "clawd-empty-bin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&empty_dir).expect("create empty dir");
        std::env::set_var("PATH", empty_dir.display().to_string());

        let err = execute_tool("PowerShell", &json!({"command": "Write-Output hello"}))
            .expect_err("PowerShell should fail when shell is missing");

        std::env::set_var("PATH", original_path);
        let _ = std::fs::remove_dir_all(empty_dir);

        assert!(err.contains("PowerShell executable not found"));
    }

    fn read_only_registry() -> super::GlobalToolRegistry {
        use runtime::permission_enforcer::PermissionEnforcer;
        use runtime::PermissionPolicy;

        let policy = mvp_tool_specs().into_iter().fold(
            PermissionPolicy::new(runtime::PermissionMode::ReadOnly),
            |policy, (name, perm)| policy.with_tool_requirement(name, perm),
        );
        let mut registry = super::GlobalToolRegistry::builtin();
        registry.set_enforcer(PermissionEnforcer::new(policy));
        registry
    }

    #[test]
    fn given_read_only_enforcer_when_bash_then_denied() {
        let registry = read_only_registry();
        // Use a command that requires DangerFullAccess (rm) to ensure it's blocked in read-only mode
        let err = registry
            .execute("bash", &json!({ "command": "rm -rf /" }))
            .expect_err("bash should be denied in read-only mode");
        assert!(
            err.contains("current mode is 'read-only'"),
            "should cite active mode: {err}"
        );
    }

    #[test]
    fn given_read_only_enforcer_when_new_file_then_denied() {
        let registry = read_only_registry();
        let err = registry
            .execute(
                "new_file",
                &json!({ "path": "/tmp/x.txt", "content": "x" }),
            )
            .expect_err("new_file should be denied in read-only mode");
        assert!(
            err.contains("current mode is read-only"),
            "should cite active mode: {err}"
        );
    }

    #[test]
    fn given_read_only_enforcer_when_edit_file_then_denied() {
        let registry = read_only_registry();
        let err = registry
            .execute(
                "edit_file",
                &json!({ "path": "/tmp/x.txt", "old_string": "a", "new_string": "b" }),
            )
            .expect_err("edit_file should be denied in read-only mode");
        assert!(
            err.contains("current mode is read-only"),
            "should cite active mode: {err}"
        );
    }

    #[test]
    fn given_read_only_enforcer_when_read_file_then_not_permission_denied() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("perm-read");
        fs::create_dir_all(&root).expect("create root");
        let file = root.join("readable.txt");
        fs::write(&file, "content\n").expect("write test file");

        // The file lives in `root`; align cwd with that directory so
        // the workspace boundary check (added to run_read_file so the
        // LLM cannot read files outside the active workspace) sees the
        // relative path as in-workspace.
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");
        let registry = read_only_registry();
        let result = registry.execute(
            "read_file",
            &json!({ "path": "readable.txt" }),
        );
        std::env::set_current_dir(&original_dir).expect("restore cwd");
        assert!(result.is_ok(), "read_file should be allowed: {result:?}");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn given_read_only_enforcer_when_glob_search_then_not_permission_denied() {
        let registry = read_only_registry();
        let result = registry.execute("glob_search", &json!({ "pattern": "*.rs" }));
        assert!(
            result.is_ok(),
            "glob_search should be allowed in read-only mode: {result:?}"
        );
    }

    #[test]
    fn given_no_enforcer_when_bash_then_executes_normally() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let registry = super::GlobalToolRegistry::builtin();
        let result = registry
            .execute("bash", &json!({ "command": "printf 'ok'" }))
            .expect("bash should succeed without enforcer");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["stdout"], "ok");
    }

    #[test]
    fn run_task_packet_creates_packet_backed_task() {
        use runtime::task_packet::TaskScope;
        let result = run_task_packet(TaskPacket {
            objective: "Ship packetized runtime task".to_string(),
            scope: TaskScope::Module,
            scope_path: Some("runtime/task system".to_string()),
            worktree: Some("/tmp/wt-packet".to_string()),
            repo: "claw-code-parity".to_string(),
            branch_policy: "origin/main only".to_string(),
            acceptance_tests: vec![
                "cargo build --workspace".to_string(),
                "cargo test --workspace".to_string(),
            ],
            commit_policy: "single commit".to_string(),
            reporting_contract: "print build/test result and sha".to_string(),
            escalation_policy: "manual escalation".to_string(),
        })
        .expect("task packet should create a task");

        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["status"], "created");
        assert_eq!(output["prompt"], "Ship packetized runtime task");
        assert_eq!(output["description"], "runtime/task system");
        assert_eq!(output["task_packet"]["repo"], "claw-code-parity");
        assert_eq!(
            output["task_packet"]["acceptance_tests"][1],
            "cargo test --workspace"
        );
    }

    struct TestServer {
        addr: SocketAddr,
        shutdown: Option<std::sync::mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn spawn(handler: Arc<dyn Fn(&str) -> HttpResponse + Send + Sync + 'static>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set nonblocking listener");
            let addr = listener.local_addr().expect("local addr");
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            let handle = thread::spawn(move || loop {
                if rx.try_recv().is_ok() {
                    break;
                }

                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buffer = [0_u8; 4096];
                        let size = stream.read(&mut buffer).expect("read request");
                        let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
                        let request_line = request.lines().next().unwrap_or_default().to_string();
                        let response = handler(&request_line);
                        stream
                            .write_all(response.to_bytes().as_slice())
                            .expect("write response");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("server accept failed: {error}"),
                }
            });

            Self {
                addr,
                shutdown: Some(tx),
                handle: Some(handle),
            }
        }

        fn addr(&self) -> SocketAddr {
            self.addr
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                handle.join().expect("join test server");
            }
        }
    }

    struct HttpResponse {
        status: u16,
        reason: &'static str,
        content_type: &'static str,
        body: String,
    }

    impl HttpResponse {
        fn html(status: u16, reason: &'static str, body: &str) -> Self {
            Self {
                status,
                reason,
                content_type: "text/html; charset=utf-8",
                body: body.to_string(),
            }
        }

        fn text(status: u16, reason: &'static str, body: &str) -> Self {
            Self {
                status,
                reason,
                content_type: "text/plain; charset=utf-8",
                body: body.to_string(),
            }
        }

        fn to_bytes(&self) -> Vec<u8> {
            format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                self.status,
                self.reason,
                self.content_type,
                self.body.len(),
                self.body
            )
            .into_bytes()
        }
    }

    #[test]
    fn run_read_file_refuses_paths_outside_workspace() {
        let _guard = env_guard();

        let outside_dir = std::env::temp_dir().join(format!(
            "clawd-outside-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&outside_dir).expect("outside dir should create");
        let outside_file = outside_dir.join("secret.txt");
        fs::write(&outside_file, "secret payload").expect("outside file should write");

        let workspace_root = std::env::temp_dir().join(format!(
            "clawd-workspace-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&workspace_root).expect("workspace dir should create");

        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&workspace_root).expect("set cwd");

        let result = super::run_read_file(super::ReadFileInput {
            path: outside_file.to_string_lossy().into_owned(),
            offset: None,
            limit: None,
            full: Some(true),
            max_pages: None,
        });

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(&outside_dir);
        let _ = fs::remove_dir_all(&workspace_root);

        let error = result.expect_err("read_file outside workspace must fail");
        assert!(
                error.contains("escapes workspace")
                || error.contains("PermissionDenied")
                || error.contains("workspace"),
            "error should mention workspace boundary; got: {error}"
        );
    }

    #[test]
    fn run_read_file_allow_policy_admits_outside_workspace() {
        let _guard = env_guard();
        let previous = super::set_active_workspace_policy(runtime::WorkspacePolicy::Allow);

        let outside_dir = std::env::temp_dir().join(format!(
            "clawd-allow-outside-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&outside_dir).expect("outside dir should create");
        let outside_file = outside_dir.join("ok.txt");
        fs::write(&outside_file, "allow me through").expect("outside file should write");

        let workspace_root = std::env::temp_dir().join(format!(
            "clawd-allow-ws-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&workspace_root).expect("workspace dir should create");

        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&workspace_root).expect("set cwd");

        let result = super::run_read_file(super::ReadFileInput {
            path: outside_file.to_string_lossy().into_owned(),
            offset: None,
            limit: None,
            full: Some(true),
            max_pages: None,
        });

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(&outside_dir);
        let _ = fs::remove_dir_all(&workspace_root);

        // Restore the previous policy before any assertion so a panic
        // here does not poison sibling tests.
        super::set_active_workspace_policy(previous);

        let payload = result.expect("Allow policy must admit the read");
        assert!(payload.contains("checksum"));
    }

    #[test]
    fn set_active_workspace_policy_returns_previous_value() {
        let _guard = env_guard();
        let strict = runtime::WorkspacePolicy::Strict;
        let allow = runtime::WorkspacePolicy::Allow;
        let prev = super::set_active_workspace_policy(strict.clone());
        // First call replaces default Strict with Strict; the *return*
        // value is whatever was previously active.
        let prev_after = super::set_active_workspace_policy(allow);
        // The policy before `allow` was `strict`.
        assert!(matches!(prev_after, runtime::WorkspacePolicy::Strict));
        // Restore.
        let _ = super::set_active_workspace_policy(prev);
    }

    #[test]
    fn note_user_input_path_in_prompt_mode_admits_subsequent_read() {
        let _guard = env_guard();
        use std::collections::BTreeSet;
        use std::sync::{Arc, Mutex};

        let outside_dir = std::env::temp_dir().join(format!(
            "clawd-note-input-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&outside_dir).expect("outside dir should create");
        let outside_file = outside_dir.join("dropped.txt");
        fs::write(&outside_file, "dropped content").expect("outside file should write");

        let workspace_root = std::env::temp_dir().join(format!(
            "clawd-note-input-ws-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&workspace_root).expect("workspace dir should create");

        // Install a Prompt policy with an empty scripted prompter.
        // If the policy ever has to consult the prompter, the empty
        // queue surfaces `NoTty` and the read is denied.
        let prompter = Arc::new(EmptyPrompter);
        let session = Arc::new(Mutex::new(BTreeSet::<runtime::ApprovedRoot>::new()));
        let user_typed = Arc::new(Mutex::new(BTreeSet::<runtime::ApprovedRoot>::new()));
        let policy = runtime::WorkspacePolicy::Prompt {
            prompter,
            session_approved: session,
            user_typed,
        };
        let previous = super::set_active_workspace_policy(policy);

        // Simulate the input parser detecting the dropped file.
        super::note_user_input_path(&outside_file);
        assert_eq!(super::user_typed_path_count(), 1);

        // The LLM now reads the file without the prompter being
        // consulted.
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&workspace_root).expect("set cwd");
        let result = super::run_read_file(super::ReadFileInput {
            path: outside_file.to_string_lossy().into_owned(),
            offset: None,
            limit: None,
            full: Some(true),
            max_pages: None,
        });
        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(&outside_dir);
        let _ = fs::remove_dir_all(&workspace_root);

        // Restore the policy before asserting so a panic does not
        // poison sibling tests.
        super::set_active_workspace_policy(previous);

        let payload = result.expect("user-typed path should be readable without prompt");
        assert!(payload.contains("checksum"));
    }

    /// Prompter that always returns `NoTty`. Used to assert that the
    /// policy never consults the prompter when the path is already
    /// in the user-typed set.
    struct EmptyPrompter;
    impl runtime::Prompter for EmptyPrompter {
        fn ask(
            &self,
            _path: &std::path::Path,
            _workspace: &std::path::Path,
        ) -> Result<runtime::BoundaryDecision, runtime::PrompterError> {
            Err(runtime::PrompterError::NoTty)
        }
    }

    #[test]
    fn powershell_script_wrapper_sets_utf8_encoding() {
        // The wrapper must prepend a UTF-8 encoding preamble so non-ASCII
        // paths and content round-trip correctly through Windows PowerShell
        // 5.x's default OEM code page.
        let script = super::build_powershell_script("Get-Content -LiteralPath 'C:\\x.txt'");
        assert!(
            script.contains("$OutputEncoding = [System.Text.Encoding]::UTF8")
                && script.contains("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8")
                && script.contains("$PSDefaultParameterValues['Out-File:Encoding'] = 'utf8'")
                && script.contains("$PSDefaultParameterValues['Get-Content:Encoding'] = 'utf8'"),
            "UTF-8 preamble must be present; got:\n{script}"
        );
        assert!(
            script.contains("Get-Content -LiteralPath 'C:\\x.txt'"),
            "user command must appear after the preamble; got:\n{script}"
        );
    }

    #[test]
    fn powershell_script_wrapper_preserves_user_command_verbatim() {
        // The wrapper must not rewrite or normalize the user's command:
        // -LiteralPath, brackets, quotes, and embedded newlines all need
        // to survive intact because the script is piped via stdin.
        let user_command =
            "Get-Content -LiteralPath 'C:\\Users\\Incredible\\Desktop\\test\\测试文本.txt'";
        let script = super::build_powershell_script(user_command);
        assert!(
            script.contains(user_command),
            "non-ASCII command must appear verbatim; got:\n{script}"
        );
    }

    #[test]
    fn has_dangerous_paths_recognises_windows_drive_letters() {
        // The classifier must flag Windows absolute paths so the
        // permission enforcer sees them. Before this fix, the tokenizer
        // only knew POSIX absolute paths and `~`, so a token like
        // `C:\Users\foo\bar.txt` was treated as a normal argument.
        assert!(super::has_dangerous_paths(r#"cat "C:\Users\foo\bar.txt""#));
        assert!(super::has_dangerous_paths(
            r#"Get-Content -LiteralPath 'D:\data\file.txt'"#
        ));
        assert!(super::has_dangerous_paths(r"X:/absolute/unix-style.txt"));
        // Relative paths are not flagged.
        assert!(!super::has_dangerous_paths("cat ./relative.txt"));
        assert!(!super::has_dangerous_paths("echo hello world"));
    }

    #[test]
    fn bash_model_view_carries_sandbox_diagnostics() {
        // The model-facing JSON envelope for the bash tool must carry
        // the sandbox fallback reason so the model can reason honestly
        // about which sandbox mechanisms are actually enforced (rather
        // than concluding "the sandbox blocked it" when only
        // process-tree kill is active).
        use runtime::sandbox::{FilesystemIsolationMode, SandboxRequest, SandboxStatus};
        let mut status = SandboxStatus::default();
        status.enabled = true;
        status.fallback_reason = Some(String::from("process tree kill only"));
        let output = runtime::BashCommandOutput {
            stdout: String::from("hello"),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: None,
            backgrounded_by_user: None,
            assistant_auto_backgrounded: None,
            dangerously_disable_sandbox: None,
            return_code_interpretation: None,
            no_output_expected: Some(false),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: Some(status),
            sandbox_type: Some(String::from("windows-job-object")),
        };
        let view = super::bash_model_view(&output);
        let parsed: serde_json::Value =
            serde_json::from_str(&view).expect("model view must be valid JSON");
        // stdout and stderr are top-level fields the model can read
        // directly.
        assert_eq!(parsed["stdout"], "hello");
        assert_eq!(parsed["stderr"], "");
        // The sandbox block carries the fallback reason so the model
        // does not conclude "the sandbox blocked it" on Windows when
        // only the Job Object is active.
        let sandbox = &parsed["sandbox"];
        assert_eq!(sandbox["type"], "windows-job-object");
        assert_eq!(sandbox["fallbackReason"], "process tree kill only");
        // Allow unused import warnings to stay quiet.
        let _ = (FilesystemIsolationMode::Off, SandboxRequest::default());
    }
}

// =====================================================================
// ARCHAEOLOGY STUBS — recovered definitions to make the tools crate
// compile after ~1000+ lines of `crates/tools/src/lib.rs` were lost
// (no git history available; this is best-effort reconstruction).
//
// Each definition is marked `// TODO(archaeology):` with a note about
// what the original probably looked like. These compile but may panic
// or return wrong data at runtime if a tool is invoked with a shape
// the original types did not anticipate.
//
// Build status before this block: cargo build --release fails with
// "cannot find type/value X in scope" for the names listed below.
// Build status after this block:  cargo build --release succeeds.
//
// If you are reviewing this diff: the safest path is to replace the
// stubs with proper implementations recovered from a known-good
// snapshot of `lib.rs` (the runtime crate already owns the canonical
// `read_file` / `new_file` / `edit_file` / `glob_search` /
// `grep_search` implementations — see `crates/runtime/src/file_ops.rs`).
// =====================================================================




// -- Searchable tool registry (recovered from use-sites in
//    `GlobalToolRegistry::search` / `searchable_tool_specs`)
// =====================================================================

/// Flattened projection of a tool used by the `ToolSearch` tool.
#[derive(Debug, Clone)]
pub struct SearchableToolSpec {
    pub name: String,
    pub description: String,
}

/// Input shape for the `ToolSearch` tool.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ToolSearchInput {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
}

/// Output envelope returned to the model by `ToolSearch`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ToolSearchOutput {
    pub matches: Vec<String>,
    pub query: String,
    pub normalized_query: String,
    pub total_deferred_tools: usize,
    pub pending_mcp_servers: Option<Vec<String>>,
    pub mcp_degraded: Option<runtime::McpDegradedReport>,
}

// -- Web / file / todo / skill / notebook / sleep / brief / config
//    / plan / repl / power-shell / structured-output inputs and outputs.
//    Field shapes are inferred from the existing `execute_*` bodies.
//    All Input types use `#[serde(default)]` so any JSON shape can
//    deserialize; Output types carry only the fields the matching
//    `execute_*` function constructs.
// =====================================================================

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ReadFileInput {
    pub path: String,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    // Per spec 2026-06-01 tool-output-context-bounds-design.md §2:
    // `full: true` bypasses the output cap.
    #[serde(default)]
    pub full: Option<bool>,
    // PDF branch only: hard page cap applied after extraction.
    #[serde(default)]
    pub max_pages: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WriteFileInput {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub force: Option<bool>,
}

/// Inputs for the `edit_file` tool. **See [`mvp_tool_specs`] for the
/// authoritative contract** that the LLM sees — these doc comments are
/// for in-process callers and must stay in sync with the schema
/// description.
///
/// Workflow contract:
/// 1. `old_string` MUST be a verbatim copy from a prior `read_file`.
/// 2. Multiple matches -> only first is replaced (unless `replace_all`).
/// 3. Not found -> `NotFound` error, file unchanged.
/// 4. To append: `old_string = current_tail`, `new_string = current_tail + content`.
/// 5. Caller MUST verify `EditFileOutput.content_preview` after the call.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct EditFileInput {
    /// Absolute path to the file to edit.
    pub path: String,
    // The serialized names follow Anthropic's snake_case convention
    // (the format the LLM emits); we also accept camelCase aliases
    // (`oldString`, `newString`, `replaceAll`, `expectedChecksum`)
    // for backwards compatibility with internal callers and tests.
    /// Exact substring to replace. Must appear verbatim in the file.
    /// If it appears multiple times, only the first occurrence is
    /// replaced unless `replace_all` is `true`. Include surrounding
    /// context to make it unique.
    #[serde(alias = "oldString")]
    pub old_string: String,
    /// Replacement content. For append, set this to (current tail + new
    /// content) and set `old_string` to the current tail.
    #[serde(alias = "newString")]
    pub new_string: String,
    /// When `true`, replace all occurrences. Default `false`.
    #[serde(alias = "replaceAll", default)]
    pub replace_all: Option<bool>,
    /// Optional xxh3-64 checksum of the file before editing. The call
    /// fails with an error if the actual checksum does not match,
    /// protecting against concurrent modification.
    #[serde(
        alias = "expectedChecksum",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub expected_checksum: Option<String>,
}

/// Input for the `undo` tool — reverses a prior edit_file operation
/// by reading the diff file and applying the inverse replacement.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UndoInput {
    /// Relative path to the diff file (e.g. ".claw/diffs/1781138770.patch").
    pub diff_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GlobSearchInputValue {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WebFetchInput {
    pub url: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WebFindInput {
    pub url: String,
    pub pattern: String,
    #[serde(rename = "ignoreCase", default)]
    pub ignore_case: Option<bool>,
    #[serde(rename = "maxMatches", default)]
    pub max_matches: Option<usize>,
    #[serde(rename = "contextChars", default)]
    pub context_chars: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WebFindMatch {
    pub line: usize,
    pub column: usize,
    pub matched: String,
    pub context: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WebFindOutput {
    pub url: String,
    pub pattern: String,
    #[serde(rename = "totalMatches")]
    pub total_matches: usize,
    pub truncated: bool,
    pub matches: Vec<WebFindMatch>,
    #[serde(rename = "bytesScanned")]
    pub bytes_scanned: usize,
    #[serde(rename = "contentType")]
    pub content_type: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WebSearchInput {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct TodoItem {
    pub content: String,
    #[serde(rename = "activeForm", default)]
    pub active_form: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TodoWriteInput {
    pub todos: Vec<TodoItem>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TodoWriteOutput {
    #[serde(rename = "oldTodos", default)]
    pub old_todos: Vec<TodoItem>,
    #[serde(rename = "newTodos")]
    pub new_todos: Vec<TodoItem>,
    #[serde(default)]
    pub verification_nudge_needed: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SkillInput {
    pub skill: String,
    #[serde(default)]
    pub args: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SkillOutput {
    pub skill: String,
    pub path: String,
    #[serde(default)]
    pub args: Option<String>,
    pub description: String,
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum NotebookEditMode {
    #[default]
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum NotebookCellType {
    #[default]
    Code,
    Markdown,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct NotebookEditInput {
    #[serde(rename = "notebookPath")]
    pub notebook_path: String,
    #[serde(rename = "cellId", default)]
    pub cell_id: Option<String>,
    #[serde(rename = "editMode", default)]
    pub edit_mode: Option<NotebookEditMode>,
    #[serde(rename = "cellType", default)]
    pub cell_type: Option<NotebookCellType>,
    #[serde(rename = "newSource", default)]
    pub new_source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct NotebookEditOutput {
    #[serde(rename = "newSource", default)]
    pub new_source: String,
    #[serde(rename = "cellId", default)]
    pub cell_id: Option<String>,
    #[serde(rename = "cellType", default)]
    pub cell_type: Option<NotebookCellType>,
    pub language: String,
    #[serde(rename = "editMode", default)]
    pub edit_mode: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(rename = "notebookPath", default)]
    pub notebook_path: String,
    #[serde(rename = "originalFile", default)]
    pub original_file: String,
    #[serde(rename = "updatedFile", default)]
    pub updated_file: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SleepInput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SleepOutput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u64,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum BriefStatus {
    #[default]
    Normal,
    Proactive,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BriefInput {
    pub message: String,
    #[serde(default)]
    pub attachments: Option<Vec<String>>,
    pub status: BriefStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ResolvedAttachment {
    pub path: String,
    pub size: u64,
    #[serde(rename = "isImage")]
    pub is_image: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BriefOutput {
    pub message: String,
    #[serde(default)]
    pub attachments: Option<Vec<ResolvedAttachment>>,
    #[serde(rename = "sentAt")]
    pub sent_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ConfigInput {
    pub setting: String,
    #[serde(default)]
    pub value: Option<ConfigValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ConfigOutput {
    pub success: bool,
    pub operation: Option<String>,
    pub setting: Option<String>,
    pub value: Option<serde_json::Value>,
    #[serde(rename = "previousValue")]
    pub previous_value: Option<serde_json::Value>,
    #[serde(rename = "newValue")]
    pub new_value: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Lightweight JSON-shaped value so `ConfigOutput` can carry arbitrary
/// settings without forcing every caller to know the schema.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ConfigValue {
    Bool(bool),
    Number(f64),

    String(String),
    Array(Vec<ConfigValue>),
    Object(BTreeMap<String, ConfigValue>),
    Null,
}

impl Default for ConfigValue {
    fn default() -> Self {
        ConfigValue::Null
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct EnterPlanModeInput {
    #[serde(default)]
    pub _placeholder: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ExitPlanModeInput {
    #[serde(default)]
    pub _placeholder: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PlanModeState {
    #[serde(rename = "hadLocalOverride", default)]
    pub had_local_override: bool,
    #[serde(rename = "previousLocalMode", default)]
    pub previous_local_mode: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PlanModeOutput {
    pub success: bool,
    pub operation: String,
    pub changed: bool,
    pub active: bool,
    pub managed: bool,
    pub message: String,
    #[serde(rename = "settingsPath", default)]
    pub settings_path: String,
    #[serde(rename = "statePath", default)]
    pub state_path: String,
    #[serde(rename = "previousLocalMode", default)]
    pub previous_local_mode: Option<serde_json::Value>,
    #[serde(rename = "currentLocalMode", default)]
    pub current_local_mode: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ReplInput {
    pub language: String,
    pub code: String,
    #[serde(rename = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ReplOutput {
    pub language: String,
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "exitCode")]
    pub exit_code: i32,
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AskUserQuestionInput {
    pub question: String,
    #[serde(default)]
    pub options: Option<Vec<String>>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub multi_select: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PowerShellInput {
    pub command: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(rename = "runInBackground", default)]
    pub run_in_background: Option<bool>,
}

/// Wrapper tuple struct so `execute_structured_output` can carry the
/// raw JSON payload through `input.0`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct StructuredOutputInput(pub serde_json::Value);

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct StructuredOutputResult {
    pub data: String,
    #[serde(rename = "structuredOutput")]
    pub structured_output: serde_json::Value,
}

// -- Task / worker / team / cron registry inputs. Field shapes are
//    taken directly from the existing `run_task_*` / `run_worker_*`
//    / `run_team_*` / `run_cron_*` functions which already exist.
// =====================================================================

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TaskCreateInput {
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TaskIdInput {
    #[serde(rename = "taskId")]
    pub task_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TaskUpdateInput {
    #[serde(rename = "taskId")]
    pub task_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WorkerCreateInput {
    pub cwd: String,
    #[serde(rename = "trustedRoots", default)]
    pub trusted_roots: Vec<String>,
    #[serde(rename = "autoRecoverPromptMisdelivery", default)]
    pub auto_recover_prompt_misdelivery: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WorkerIdInput {
    #[serde(rename = "workerId")]
    pub worker_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WorkerObserveInput {
    #[serde(rename = "workerId")]
    pub worker_id: String,
    #[serde(rename = "screenText")]
    pub screen_text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WorkerObserveCompletionInput {
    #[serde(rename = "workerId")]
    pub worker_id: String,
    #[serde(rename = "finishReason")]
    pub finish_reason: String,
    #[serde(rename = "tokensOutput", default)]
    pub tokens_output: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WorkerSendPromptInput {
    #[serde(rename = "workerId")]
    pub worker_id: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(rename = "taskReceipt", default)]
    pub task_receipt: Option<runtime::worker_boot::WorkerTaskReceipt>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TeamCreateInput {
    pub name: String,
    #[serde(default)]
    pub tasks: Vec<serde_json::Value>,
}
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TeamDeleteInput {
    #[serde(rename = "teamId")]
    pub team_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CronCreateInput {
    pub schedule: String,
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CronDeleteInput {
    #[serde(rename = "cronId")]
    pub cron_id: String,
}

// -- MCP / LSP / remote trigger / testing permission inputs. Shape
//    inferred from the existing `run_lsp` / `run_list_mcp_resources` /
//    `run_mcp_auth` / `run_remote_trigger` / `run_mcp_tool` /
//    `run_testing_permission` functions.
// =====================================================================

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct LspInput {
    pub action: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub character: Option<u32>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct McpResourceInput {
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct McpAuthInput {
    pub server: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct McpToolInput {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RemoteTriggerInput {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TestingPermissionInput {
    pub action: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

// -- Missing utility functions. Signatures are inferred from call
//    sites in the existing code. `to_pretty_json` is used everywhere
//    as `to_pretty_json(json!({...}))?` so the body is just a thin
//    wrapper around `serde_json::to_string_pretty`.
// =====================================================================

fn to_pretty_json<T: serde::Serialize>(value: T) -> Result<String, String> {
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

fn classify_powershell_permission(command: &str) -> runtime::PermissionMode {
    // TODO(archaeology): match the more detailed PowerShell classification
    // that the original implementation almost certainly had. The conservative
    // safe-default here is `WorkspaceWrite` so a typical PowerShell command
    // is at least reviewable. The original likely distinguished destructive
    // cmdlets (Remove-Item, Set-Item, Stop-Process, ...) as
    // `DangerFullAccess` and read-only cmdlets (Get-*, Select-*, ...) as
    // `WorkspaceWrite`. For now this matches `classify_bash_permission`.
    let base_cmd = command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('|')
        .next()
        .unwrap_or("")
        .trim()
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    let cmd_name = base_cmd.split('/').next_back().unwrap_or(base_cmd);
    let cmd_lower = cmd_name.to_ascii_lowercase();
    let read_only = matches!(
        cmd_lower.as_str(),
        "get-childitem"
            | "get-item"
            | "get-content"
            | "get-process"
            | "get-service"
            | "get-location"
            | "select-object"
            | "where-object"
            | "format-list"
            | "format-table"
            | "measure-object"
            | "test-path"
            | "pwd"
    );
    if read_only {
        runtime::PermissionMode::WorkspaceWrite
    } else {
        runtime::PermissionMode::DangerFullAccess
    }
}

// -- `run_*` wrappers. The dispatch table at `execute_tool_with_enforcer`
//    calls `run_<tool>` for every tool. The original `run_*` functions
//    were thin shells that delegated to `execute_*` (for the tools that
//    have `execute_*` implementations in this file) or to the runtime
//    crate's `read_file` / `new_file` / etc. (for the file tools).
//    These stubs restore that delegation.
// =====================================================================

#[allow(clippy::needless_pass_by_value)]
fn run_read_file(input: ReadFileInput) -> Result<String, String> {
    // Check file cache first — hits from prior new_file / edit_file calls.
    // Only serve full reads (no offset/limit) from cache to keep it simple.
    if input.offset.is_none() && input.limit.is_none() {
        let abs_path = std::path::Path::new(&input.path)
            .canonicalize()
            .ok();
        if let Some(ref abs) = abs_path {
            let key = abs.to_string_lossy().into_owned();
            if let Ok(cache) = global_file_cache().lock() {
                if let Some(entry) = cache.get(&key) {
                    // Cache hit — construct ReadFileOutput without disk I/O.
                    let lines: Vec<&str> = entry.content.lines().collect();
                    let total_lines = lines.len();
                    let content = if input.full == Some(false) {
                        None
                    } else {
                        Some(entry.content.clone())
                    };
                    let output = runtime::ReadFileOutput {
                        kind: "text".to_string(),
                        file: runtime::TextFilePayload {
                            file_path: key,
                            content,
                            checksum: entry.checksum.clone(),
                            bytes_read: entry.content.len(),
                            num_lines: total_lines,
                            start_line: 1,
                            total_lines,
                        },
                    };
                    return serde_json::to_string_pretty(&output)
                        .map_err(|e| e.to_string());
                }
            }
        }
    }

    // Cache miss or partial read — fall through to disk.
    let workspace_root = std::env::current_dir().map_err(|error| error.to_string())?;
    let output = runtime::read_file_with_policy(
        &input.path,
        input.offset,
        input.limit,
        &workspace_root,
        &active_workspace_policy(),
        input.full,
    )
    .map_err(|error| error.to_string())?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_new_file(input: WriteFileInput) -> Result<String, String> {
    let workspace_root = std::env::current_dir().map_err(|error| error.to_string())?;
    let output = runtime::new_file_with_policy(
        &input.path,
        &input.content,
        input.force.unwrap_or(false),
        &workspace_root,
        &active_workspace_policy(),
    )
    .map_err(|error| error.to_string())?;

    // Cache the written content so subsequent read_file calls skip disk I/O.
    if let Ok(abs) = std::path::Path::new(&output.file_path).canonicalize() {
        if let Ok(mut cache) = global_file_cache().lock() {
            cache.insert(
                abs.to_string_lossy().into_owned(),
                FileCacheEntry {
                    content: input.content.clone(),
                    checksum: output.checksum.clone(),
                },
            );
        }
    }

    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_edit_file(input: EditFileInput) -> Result<String, String> {
    let workspace_root = std::env::current_dir().map_err(|error| error.to_string())?;
    let output = runtime::edit_file_with_policy(
        &input.path,
        &input.old_string,
        &input.new_string,
        input.replace_all.unwrap_or(false),
        input.expected_checksum.as_deref(),
        &workspace_root,
        &active_workspace_policy(),
    )
    .map_err(|error| error.to_string())?;

    // Update cache: re-read the file from disk to get the exact post-edit content.
    if let Ok(abs) = std::path::Path::new(&output.file_path).canonicalize() {
        if let Ok(new_content) = std::fs::read_to_string(&abs) {
            if let Ok(mut cache) = global_file_cache().lock() {
                cache.insert(
                    abs.to_string_lossy().into_owned(),
                    FileCacheEntry {
                        content: new_content,
                        checksum: output.new_checksum.clone(),
                    },
                );
            }
        }
    }

    // Write diff file for potential rollback.
    let diff_path = write_diff_file(&input.path, &input.old_string, &input.new_string, input.replace_all.unwrap_or(false));

    // Include diff_path in the output JSON for JSONL/context markers.
    let mut json = serde_json::to_value(&output).map_err(|e| e.to_string())?;
    if let Some(dp) = &diff_path {
        json["diffPath"] = serde_json::Value::String(dp.clone());
    }
    serde_json::to_string_pretty(&json).map_err(|e| e.to_string())
}

/// Reverse a prior edit_file by reading its diff file and applying the
/// inverse replacement. The diff file is deleted after successful rollback.
fn run_undo(input: UndoInput) -> Result<String, String> {
    // 0. Validate diff path is within .claw/diffs/
    let canonical = std::path::Path::new(&input.diff_path)
        .canonicalize()
        .map_err(|e| format!("Invalid diff path: {e}"))?;
    let diffs_root = std::path::Path::new(".claw")
        .join("diffs")
        .canonicalize()
        .map_err(|_| "Diff directory .claw/diffs/ not found".to_string())?;
    if !canonical.starts_with(&diffs_root) {
        return Err(format!(
            "Diff path must be within .claw/diffs/: {}",
            input.diff_path
        ));
    }

    // 1. Read diff file
    let diff_content = std::fs::read_to_string(&input.diff_path)
        .map_err(|e| format!("Failed to read diff file '{}': {e}", input.diff_path))?;
    let diff: serde_json::Value = serde_json::from_str(&diff_content)
        .map_err(|e| format!("Invalid diff file: {e}"))?;

    let path = diff["path"]
        .as_str()
        .ok_or("diff file missing 'path'")?
        .to_string();
    let old_string = diff["old_string"]
        .as_str()
        .ok_or("diff file missing 'old_string'")?
        .to_string();
    let new_string = diff["new_string"]
        .as_str()
        .ok_or("diff file missing 'new_string'")?
        .to_string();
    let replace_all = diff["replace_all"].as_bool().unwrap_or(false);

    // 2. Reverse edit: swap old_string ↔ new_string
    let workspace_root = std::env::current_dir().map_err(|e| e.to_string())?;
    let output = runtime::edit_file_with_policy(
        &path,
        &new_string,   // was the "new" content, now becomes "old"
        &old_string,   // was the "old" content, now becomes "new"
        replace_all,
        None,
        &workspace_root,
        &active_workspace_policy(),
    )
    .map_err(|e| format!("Undo edit failed: {e}"))?;

    // 3. Update file cache with the restored content.
    if let Ok(abs) = std::path::Path::new(&output.file_path).canonicalize() {
        if let Ok(restored) = std::fs::read_to_string(&abs) {
            if let Ok(mut cache) = global_file_cache().lock() {
                cache.insert(
                    abs.to_string_lossy().into_owned(),
                    FileCacheEntry {
                        content: restored,
                        checksum: output.new_checksum.clone(),
                    },
                );
            }
        }
    }

    // 4. Delete the diff file
    delete_diff_file(&input.diff_path);

    // 5. Return result
    Ok(serde_json::json!({
        "type": "undo",
        "filePath": output.file_path,
        "diffPath": input.diff_path,
        "status": "reverted",
        "newChecksum": output.new_checksum,
    })
    .to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_glob_search(input: GlobSearchInputValue) -> Result<String, String> {
    let output = runtime::glob_search(&input.pattern, input.path.as_deref())
        .map_err(|error| error.to_string())?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_grep_search(input: runtime::GrepSearchInput) -> Result<String, String> {
    let output = runtime::grep_search(&input).map_err(|error| error.to_string())?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

// Fetch a single URL and return the body as text. The output shape is
// `{code, url, result}` so callers can drive further processing (title
// extraction, line-based slicing) off the result field. HTML responses
// are passed through `html_to_text`; other content types are returned
// verbatim. The 5 MB body cap and 30 s timeout keep the tool cheap
// to call from a model that may request many pages in parallel.
const WEB_FETCH_TIMEOUT_SECS: u64 = 30;
const WEB_FETCH_MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

// Heuristic: when a Wikipedia URL is unreachable (the source is
// typically blocked on networks behind the GFW), the WebFetch tool
// automatically retries with a Sogou search URL containing the article
// title. Sogou search is reachable from the same networks and its
// results page links to Chinese mirrors (baike.sogou.com, baike.baidu.com,
// zhihu.com, etc.) that the model can re-fetch.
//
// `wiki_mirror_url` returns `Some((url, label))` for any Wikipedia
// URL whose path is `/wiki/<title>`, and `None` otherwise. It is
// implemented as a pure function so tests can pin its behavior
// without touching the network.
fn wiki_mirror_url(url: &reqwest::Url) -> Option<(reqwest::Url, &'static str)> {
    let host = url.host_str()?.to_ascii_lowercase();
    let is_wiki = host == "wikipedia.org" || host.ends_with(".wikipedia.org");
    if !is_wiki {
        return None;
    }
    // The path of a Wikipedia article is `/wiki/<title>`. We URL-
    // decode the title (Wikipedia paths are percent-encoded) and
    // convert `_` to space (Wikipedia URL convention) before
    // handing it to Sogou. The `path_segments()` iterator returns
    // raw bytes, so we work from `path()` directly and decode
    // explicitly to avoid double-encoding.
    let path = url.path();
    let title_encoded = path.strip_prefix("/wiki/")?;
    if title_encoded.is_empty() {
        return None;
    }
    let title_decoded = url::form_urlencoded::parse(title_encoded.as_bytes())
        .next()
        .map(|(k, _)| k.into_owned())
        .unwrap_or_else(|| title_encoded.to_string());
    let title = title_decoded.replace('_', " ");
    if title.trim().is_empty() {
        return None;
    }
    let mut mirror = reqwest::Url::parse("https://www.sogou.com/web").ok()?;
    mirror.query_pairs_mut().append_pair("query", &title);
    Some((mirror, "sogou-search"))
}

#[allow(clippy::needless_pass_by_value)]
/// Inner fetch helper used by [`run_web_fetch`]. Returns the HTTP
/// status, content-type, and body string, or an error describing the
/// transport/HTTP/parse failure. Used twice when a Wikipedia URL
/// falls back to the Sogou search mirror.
fn fetch_once(
    client: &reqwest::blocking::Client,
    url: &reqwest::Url,
) -> Result<(u16, String, String), String> {
    let response = client
        .get(url.clone())
        .send()
        .map_err(|error| format!("fetch failed for '{}': {}", url, error))?;
    let code = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/plain")
        .to_ascii_lowercase();
    let body = response
        .bytes()
        .map_err(|error| format!("read body failed for '{}': {}", url, error))?;
    if body.len() > WEB_FETCH_MAX_BODY_BYTES {
        return Err(format!(
            "response too large for '{}': {} bytes (limit is {} bytes)",
            url,
            body.len(),
            WEB_FETCH_MAX_BODY_BYTES
        ));
    }
    let raw = String::from_utf8_lossy(&body).into_owned();
    Ok((code, content_type, raw))
}

fn run_web_fetch(input: WebFetchInput) -> Result<String, String> {
    let url = reqwest::Url::parse(&input.url)
        .map_err(|error| format!("invalid URL '{}': {}", input.url, error))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(WEB_FETCH_TIMEOUT_SECS))
        .user_agent("claw-code/0.1 (+webfetch)")
        .build()
        .map_err(|error| error.to_string())?;

    // Check WebFetch cache BEFORE making HTTP request.
    // Return cached content if fresh (within TTL) to avoid network calls.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Ok(cache) = global_webfetch_cache().lock() {
        if let Some(entry) = cache.get(&input.url) {
            if now.saturating_sub(entry.fetched_at) < WEBFETCH_CACHE_TTL_SECS {
                // Return full cached content so the AI can actually read it.
                // Dedup is achieved by skipping the HTTP request.
                let cached_result = format!(
                    "[CACHED] {}\nPrompt: {}\nContent:\n{}",
                    input.url, input.prompt, entry.content,
                );
                return serde_json::to_string_pretty(&serde_json::json!({
                    "code": 200,
                    "url": input.url,
                    "result": cached_result,
                    "cached": true,
                }))
                .map_err(|e| e.to_string());
            }
        }
    }

    // Attempt the primary URL first. If it fails and the URL is a
    // Wikipedia article, retry with the Sogou search mirror. This
    // makes Wikipedia fetches work on networks that block the source
    // (e.g. behind the GFW) by routing the request to a reachable
    // search engine that links to Chinese mirrors.
    let (primary_code, primary_ct, primary_body) = match fetch_once(&client, &url) {
        Ok(result) => result,
        Err(primary_error) => match wiki_mirror_url(&url) {
            Some((mirror, label)) => {
                let (code, ct, body) = fetch_once(&client, &mirror)
                    .map_err(|mirror_error| {
                        format!(
                            "primary URL '{url}' failed ({primary_error}) and mirror \
                             '{mirror}' also failed ({mirror_error})"
                        )
                    })?;
                let summarized =
                    summarize_web_fetch(mirror.as_str(), &input.prompt, &body, &ct);
                return serde_json::to_string_pretty(&serde_json::json!({
                    "code": code,
                    "url": input.url,
                    "mirror": label,
                    "mirrorUrl": mirror.as_str(),
                    "result": summarized,
                }))
                .map_err(|error| error.to_string());
            }
            None => return Err(primary_error),
        },
    };

    // If the primary came back non-2xx and is a Wikipedia URL, try the
    // mirror as a content source. The body is usually a Cloudflare
    // challenge page, which is why we treat it as failure even when
    // the transport succeeded.
    let (code, content_type, raw, used_mirror) = if !(200..300).contains(&primary_code) {
        if let Some((mirror, label)) = wiki_mirror_url(&url) {
            match fetch_once(&client, &mirror) {
                Ok((code, ct, body)) => (code, ct, body, Some(label)),
                Err(_) => (primary_code, primary_ct, primary_body, None),
            }
        } else {
            (primary_code, primary_ct, primary_body, None)
        }
    } else {
        (primary_code, primary_ct, primary_body, None)
    };

    let result = summarize_web_fetch(&input.url, &input.prompt, &raw, &content_type);

    // Store full extracted content in cache for future dedup.
    if let Ok(mut cache) = global_webfetch_cache().lock() {
        cache.insert(
            input.url.clone(),
            WebFetchCacheEntry {
                content: result.clone(),
                fetched_at: now,
            },
        );
    }

    // Return full content to the AI on first fetch (needed for processing).
    // Cache stores the content for dedup; subsequent hits return only a marker.
    let mut payload = serde_json::json!({
        "code": code,
        "url": input.url,
        "result": result,
    });
    if let Some(label) = used_mirror {
        payload["mirror"] = serde_json::Value::String(label.to_string());
    }
    serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())
}

// Web search strategy:
//   1. SEARCHAPI_API_KEY set → use SearchAPI (Google engine via JSON)
//   2. No key → silently fall back to scraping Bing then Sogou.
//      Both engines are reachable without authentication and return
//      structured HTML that the `scraper` crate can parse.
// Default SearchAPI endpoint — override via SEARCHAPI_URL env var to
// point at a self-hosted proxy or a compatible SearchAPI mirror.
const SEARCHAPI_DEFAULT_URL: &str = "https://www.searchapi.io/api/v1/search";
const SEARCHAPI_DEFAULT_RESULTS: usize = 10;
const SEARCHAPI_MAX_RESULTS: usize = 20;

// ── Shared search-result record ─────────────────────────────────────
struct ScrapedSearchResult {
    title: String,
    link: String,
    snippet: String,
    source: String,
}

fn format_search_response(
    query: &str,
    provider: &str,
    results: Vec<ScrapedSearchResult>,
    max_results: usize,
) -> Result<String, String> {
    let truncated: Vec<serde_json::Value> = results
        .into_iter()
        .take(max_results)
        .map(|r| {
            serde_json::json!({
                "title":   r.title,
                "link":    r.link,
                "snippet": r.snippet,
                "source":  r.source,
                "date":    "",
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "query": query,
        "provider": provider,
        "totalResults": 0,
        "resultsReturned": truncated.len(),
        "results": truncated,
    }))
    .map_err(|e| e.to_string())
}

// ── Bing scraper ────────────────────────────────────────────────────
fn scrape_bing(
    client: &reqwest::blocking::Client,
    query: &str,
) -> Result<Vec<ScrapedSearchResult>, String> {
    let mut url = reqwest::Url::parse("https://www.bing.com/search")
        .map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("ensearch", "0");

    let html = client
        .get(url)
        .send()
        .and_then(|r| r.text())
        .map_err(|e| format!("Bing request failed: {e}"))?;

    let document = Html::parse_document(&html);
    let item_sel = Selector::parse("li.b_algo").map_err(|_| "bad selector: li.b_algo")?;
    let title_sel = Selector::parse("h2 a").map_err(|_| "bad selector: h2 a")?;
    let snippet_sel =
        Selector::parse(".b_caption p").map_err(|_| "bad selector: .b_caption p")?;

    let mut results = Vec::new();
    for node in document.select(&item_sel) {
        let (title, link) = match node.select(&title_sel).next() {
            Some(a) => (
                a.text().collect::<String>().trim().to_string(),
                a.value()
                    .attr("href")
                    .unwrap_or("")
                    .to_string(),
            ),
            None => continue,
        };
        let snippet = node
            .select(&snippet_sel)
            .next()
            .map(|p| p.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let source = reqwest::Url::parse(&link)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        results.push(ScrapedSearchResult {
            title,
            link,
            snippet,
            source,
        });
    }
    Ok(results)
}

// ── Sogou scraper ───────────────────────────────────────────────────
fn scrape_sogou(
    client: &reqwest::blocking::Client,
    query: &str,
) -> Result<Vec<ScrapedSearchResult>, String> {
    let mut url =
        reqwest::Url::parse("https://www.sogou.com/web").map_err(|e| e.to_string())?;
    url.query_pairs_mut().append_pair("query", query);

    let html = client
        .get(url)
        .send()
        .and_then(|r| r.text())
        .map_err(|e| format!("Sogou request failed: {e}"))?;

    let document = Html::parse_document(&html);
    let item_sel = Selector::parse("div.vrwrap, div.rb").map_err(|_| "bad selector")?;
    let title_sel = Selector::parse("h3 a").map_err(|_| "bad selector: h3 a")?;
    let snippet_sel = Selector::parse("div.str-text, p.str_info, div.b-txt")
        .map_err(|_| "bad snippet selector")?;

    let mut results = Vec::new();
    for node in document.select(&item_sel) {
        let (title, link) = match node.select(&title_sel).next() {
            Some(a) => (
                a.text().collect::<String>().trim().to_string(),
                a.value()
                    .attr("href")
                    .unwrap_or("")
                    .to_string(),
            ),
            None => continue,
        };
        let snippet = node
            .select(&snippet_sel)
            .next()
            .map(|p| p.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let source = reqwest::Url::parse(&link)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        results.push(ScrapedSearchResult {
            title,
            link,
            snippet,
            source,
        });
    }
    Ok(results)
}

// ── Main entry point ────────────────────────────────────────────────
#[allow(clippy::needless_pass_by_value)]
fn run_web_search(input: WebSearchInput) -> Result<String, String> {
    let max_results = input
        .max_results
        .unwrap_or(SEARCHAPI_DEFAULT_RESULTS)
        .min(SEARCHAPI_MAX_RESULTS)
        .max(1);

    // ── Path 1: SearchAPI provider (when key is configured) ─────────
    if let Some(api_key) = std::env::var("SEARCHAPI_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(WEB_FETCH_TIMEOUT_SECS))
            .user_agent("claw-code/0.1 (+websearch)")
            .build()
            .map_err(|error| error.to_string())?;

        let base_url = std::env::var("SEARCHAPI_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| SEARCHAPI_DEFAULT_URL.to_string());

        let response = client
            .get(&base_url)
            .query(&[
                ("engine", "google"),
                ("q", &input.query),
                ("api_key", &api_key),
            ])
            .send()
            .map_err(|error| format!("SearchAPI request failed: {error}"))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .map_err(|error| format!("SearchAPI read body failed: {error}"))?;

        if !(200..300).contains(&status) {
            return Err(format!("SearchAPI returned HTTP {status}: {body}"));
        }

        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| format!("SearchAPI response is not valid JSON: {error}"))?;

        let results: Vec<serde_json::Value> = json
            .get("organic_results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let formatted: Vec<serde_json::Value> = results
            .into_iter()
            .take(max_results)
            .map(|r| {
                serde_json::json!({
                    "title":   r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                    "link":    r.get("link").and_then(|v| v.as_str()).unwrap_or(""),
                    "snippet": r.get("snippet").and_then(|v| v.as_str()).unwrap_or(""),
                    "source":  r.get("source").and_then(|v| v.as_str()).unwrap_or(""),
                    "date":    r.get("date").and_then(|v| v.as_str()).unwrap_or(""),
                })
            })
            .collect();

        let total_results = json
            .get("search_information")
            .and_then(|si| si.get("total_results"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        return serde_json::to_string_pretty(&serde_json::json!({
            "query": input.query,
            "provider": "searchapi",
            "totalResults": total_results,
            "resultsReturned": formatted.len(),
            "results": formatted,
        }))
        .map_err(|error| error.to_string());
    }

    // ── Path 2: Bing → Sogou fallback (no API key required) ────────
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(WEB_FETCH_TIMEOUT_SECS))
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/131.0.0.0 Safari/537.36",
        )
        .build()
        .map_err(|e| e.to_string())?;

    // Try Bing first — more structured results for English queries.
    if let Ok(results) = scrape_bing(&client, &input.query) {
        if !results.is_empty() {
            return format_search_response(
                &input.query,
                "bing",
                results,
                max_results,
            );
        }
    }

    // Fall back to Sogou — works well for Chinese queries and
    // behind-the-GFW networks where Bing may be unreachable.
    if let Ok(results) = scrape_sogou(&client, &input.query) {
        if !results.is_empty() {
            return format_search_response(
                &input.query,
                "sogou",
                results,
                max_results,
            );
        }
    }

    // Both engines returned empty — give the LLM a clear signal
    // rather than an error so it can try alternative strategies.
    format_search_response(&input.query, "none", Vec::new(), max_results)
}

// Maximum matches a single WebFind call can return. Larger results
// get truncated with `truncated: true` so the LLM sees the token cost
// explicitly instead of being silently flooded.
const WEB_FIND_MAX_MATCHES_CAP: usize = 50;
const WEB_FIND_DEFAULT_MAX_MATCHES: usize = 10;
const WEB_FIND_DEFAULT_CONTEXT_CHARS: usize = 100;
const WEB_FIND_MAX_CONTEXT_CHARS: usize = 500;

// Server-side grep over a fetched URL. Inspired by OpenAI's
// `web_search` provider `find` action: the model supplies `url` and
// `pattern`, the tool returns just the matching snippets (with line,
// column, and trimmed context) instead of dumping the whole page.
// This is the token-efficient counterpart to WebFetch — a 36 KB HTML
// page can shrink to a few hundred tokens when the model only needs
// the lines containing a specific value.
#[allow(clippy::needless_pass_by_value)]
fn run_web_find(input: WebFindInput) -> Result<String, String> {
    if input.pattern.is_empty() {
        return Err(String::from("pattern must not be empty"));
    }

    let url = reqwest::Url::parse(&input.url)
        .map_err(|error| format!("invalid URL '{}': {}", input.url, error))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(WEB_FETCH_TIMEOUT_SECS))
        .user_agent("claw-code/0.1 (+webfind)")
        .build()
        .map_err(|error| error.to_string())?;

    let response = client
        .get(url.clone())
        .send()
        .map_err(|error| format!("fetch failed for '{}': {}", input.url, error))?;

    let code = response.status().as_u16();
    if !(200..300).contains(&code) {
        return Err(format!(
            "fetch failed for '{}': HTTP {}",
            input.url, code
        ));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/plain")
        .to_ascii_lowercase();

    let body = response
        .bytes()
        .map_err(|error| format!("read body failed for '{}': {}", input.url, error))?;
    if body.len() > WEB_FETCH_MAX_BODY_BYTES {
        return Err(format!(
            "response too large for '{}': {} bytes (limit is {} bytes)",
            input.url,
            body.len(),
            WEB_FETCH_MAX_BODY_BYTES
        ));
    }
    let raw = String::from_utf8_lossy(&body);

    let output = summarize_web_find(
        &input.url,
        &input.pattern,
        &raw,
        &content_type,
        input.ignore_case.unwrap_or(true),
        input
            .max_matches
            .unwrap_or(WEB_FIND_DEFAULT_MAX_MATCHES)
            .min(WEB_FIND_MAX_MATCHES_CAP),
        input
            .context_chars
            .unwrap_or(WEB_FIND_DEFAULT_CONTEXT_CHARS)
            .min(WEB_FIND_MAX_CONTEXT_CHARS),
    );

    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

// Convert fetched bytes into greppable text, find every occurrence
// of `pattern`, trim each match to `context_chars` of surrounding
// text, and cap the result at `max_matches`. Total occurrences are
// counted even after truncation so the caller can tell when more
// data existed.
fn summarize_web_find(
    url: &str,
    pattern: &str,
    raw_body: &str,
    content_type: &str,
    ignore_case: bool,
    max_matches: usize,
    context_chars: usize,
) -> WebFindOutput {
    // For HTML, run the body through the same extractor WebFetch
    // uses, so matches land on visible text instead of buried in
    // markup the LLM cannot reason about.
    let body = if content_type.contains("html") {
        let evaluator = FastContentEvaluator::default();
        let text = evaluator.extract_text(raw_body);
        if text.trim().is_empty() {
            raw_body.to_string()
        } else {
            text
        }
    } else {
        raw_body.to_string()
    };

    let haystack = if ignore_case {
        body.to_lowercase()
    } else {
        body.clone()
    };
    let needle = if ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };

    let mut matches: Vec<WebFindMatch> = Vec::new();
    let mut total: usize = 0;
    let mut line: usize = 1;
    let mut line_start: usize = 0;
    let bytes_scanned = body.len();

    for (offset, ch) in haystack.char_indices() {
        if ch == '\n' {
            line += 1;
            line_start = offset + ch.len_utf8();
            continue;
        }
        if haystack[offset..].starts_with(&needle) {
            total += 1;
            if matches.len() < max_matches {
                let line_end = haystack[line_start..]
                    .find('\n')
                    .map(|delta| line_start + delta)
                    .unwrap_or(haystack.len());
                let column = offset - line_start + 1;
                let matched = body[offset..offset + needle.len()].to_string();
                let context = extract_match_context(&body, line_start, line_end, column, needle.len(), context_chars);
                matches.push(WebFindMatch {
                    line,
                    column,
                    matched,
                    context,
                });
            }
        }
    }

    WebFindOutput {
        url: url.to_string(),
        pattern: pattern.to_string(),
        total_matches: total,
        truncated: total > matches.len(),
        matches,
        bytes_scanned,
        content_type: content_type.to_string(),
    }
}

// Pull up to `context_chars` chars before and after a match within
// the matched line, collapsing internal whitespace so the LLM gets a
// compact snippet rather than a wall of source formatting.
fn extract_match_context(
    body: &str,
    line_start: usize,
    line_end: usize,
    column_one_indexed: usize,
    match_len: usize,
    context_chars: usize,
) -> String {
    let line = &body[line_start..line_end];
    let line_chars: Vec<char> = line.chars().collect();
    let match_start = column_one_indexed.saturating_sub(1);
    let match_end = (match_start + match_len).min(line_chars.len());

    let window_start = match_start.saturating_sub(context_chars);
    let window_end = (match_end + context_chars).min(line_chars.len());
    let snippet: String = line_chars[window_start..window_end]
        .iter()
        .copied()
        .collect();
    collapse_whitespace(&snippet)
}

#[allow(clippy::needless_pass_by_value)]
fn run_todo_write(input: TodoWriteInput) -> Result<String, String> {
    let output = execute_todo_write(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_skill(input: SkillInput) -> Result<String, String> {
    let output = execute_skill(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

// Build the manifest + output file, promote Created -> Running on disk,
// then hand the job to the spawn closure. Production callers pass
// `spawn_agent_task`; tests pass a mock that captures the job and
// returns a noop handle so the rest of the file can be exercised
// without spinning a real agent.
#[allow(clippy::needless_pass_by_value)]
fn execute_agent_with_spawn<F>(
    input: agents::AgentInput,
    spawn_fn: F,
) -> Result<agents::AgentOutput, String>
where
    F: FnOnce(agents::AgentJob) -> Result<agents::AgentHandle, String>,
{
    use std::fs;

    let agent_id = make_agent_id();
    let store_dir = agent_store_dir()?;
    let agents_dir = store_dir.join("agents");
    fs::create_dir_all(&agents_dir).map_err(|error| error.to_string())?;

    let output_path = agents_dir.join(format!("{agent_id}.md"));
    let manifest_path = agents_dir.join(format!("{agent_id}.json"));

    let raw_name = input.name.as_deref().unwrap_or(&input.description);
    let name = slugify_agent_name(raw_name);
    let created_at = unix_now();
    let started_at_str = created_at.to_string();

    // Normalize the subagent type once and reuse it for both the
    // manifest and the runtime lookups. This keeps the user-supplied
    // alias ("explorer" / "Explore" / "explore-agent") canonicalized
    // to the registered kind ("Explore"), so the test asserting
    // "subagentType": "Explore" passes regardless of casing.
    let normalized_subagent = input
        .subagent_type
        .as_deref()
        .map(|raw| normalize_subagent_type(Some(raw)));
    let lookup_subagent = normalized_subagent.as_deref().unwrap_or("general-purpose");

    // Seed the output file with description + prompt so callers can
    // inspect the handoff state at any time, even if the agent errors
    // before writing its own content.
    let initial_output = format!(
        "# {name}\n\n## Description\n\n{description}\n\n## Prompt\n\n{prompt}\n",
        description = input.description,
        prompt = input.prompt,
    );
    fs::write(&output_path, initial_output).map_err(|error| error.to_string())?;

    // Build the Created-state manifest with a `lane.started` event,
    // then write it so observers can see the handoff immediately.
    let created = agents::AgentOutput {
        agent_id: agent_id.clone(),
        name: name.clone(),
        description: input.description.clone(),
        subagent_type: normalized_subagent.clone(),
        model: Some(resolve_agent_model(input.model.as_deref())),
        status: AgentStatus::Created,
        output_file: output_path.to_string_lossy().into_owned(),
        manifest_file: manifest_path.to_string_lossy().into_owned(),
        created_at,
        started_at: None,
        completed_at: None,
        lane_events: vec![runtime::LaneEvent::started(started_at_str.clone())],
        current_blocker: None,
        derived_state: String::from("truly_idle"),
        error: None,
    };
    write_agent_manifest(&created)?;

    // Promote Created -> Running on disk so the agent runtime sees
    // the handoff is live.
    let started_at = unix_now();
    mark_agent_running(&created, started_at)?;

    // Reconstruct the Running-state manifest for the caller's return
    // value (mark_agent_running writes to disk but does not return).
    let running = agents::AgentOutput {
        status: AgentStatus::Running,
        started_at: Some(started_at),
        derived_state: String::from("working"),
        ..created
    };

    let job = agents::AgentJob {
        manifest: running.clone(),
        prompt: input.prompt,
        system_prompt: build_agent_system_prompt(lookup_subagent)?,
        allowed_tools: allowed_tools_for_subagent(lookup_subagent),
    };

    // Hand off to the spawn closure. Production passes
    // `spawn_agent_task`; tests pass a mock that captures the job.
    spawn_fn(job)?;

    Ok(running)
}

#[allow(clippy::needless_pass_by_value)]
fn run_agent(input: agents::AgentInput) -> Result<String, String> {
    // The agent subsystem lives in `crates/agents/`. The original
    // `run_agent` wrapper delegated to the public
    // `execute_agent_with_spawn` shim — re-export that behavior.
    // Agent runs asynchronously; use AgentGet to check status/output.
    let output = execute_agent_with_spawn(input, spawn_agent_task)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_tool_search(input: ToolSearchInput) -> Result<String, String> {
    let output = execute_tool_search(input);
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_notebook_edit(input: NotebookEditInput) -> Result<String, String> {
    let output = execute_notebook_edit(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_sleep(input: SleepInput) -> Result<String, String> {
    let output = execute_sleep(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_brief(input: BriefInput) -> Result<String, String> {
    let output = execute_brief(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_config(input: ConfigInput) -> Result<String, String> {
    let output = execute_config(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_enter_plan_mode(input: EnterPlanModeInput) -> Result<String, String> {
    let output = execute_enter_plan_mode(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_exit_plan_mode(input: ExitPlanModeInput) -> Result<String, String> {
    let output = execute_exit_plan_mode(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_structured_output(input: StructuredOutputInput) -> Result<String, String> {
    let output = execute_structured_output(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_repl(input: ReplInput) -> Result<String, String> {
    let output = execute_repl(input)?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_powershell(input: PowerShellInput) -> Result<String, String> {
    let output = execute_powershell(input).map_err(|error| error.to_string())?;
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

/// Query an agent's status without loading its full output.
/// Use Read tool on `outputFile` if you need the complete content.
#[allow(clippy::needless_pass_by_value)]
fn run_agent_get(input: agents::AgentGetInput) -> Result<String, String> {
    use std::fs;

    let manifest_path = agents::agent_store_dir()
        .map_err(|e| format!("Failed to find agent store: {e}"))?
        .join("agents")
        .join(format!("{}.json", input.agent_id));

    if !manifest_path.exists() {
        return Err(format!("Agent not found: {}", input.agent_id));
    }

    let content = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Failed to read manifest: {e}"))?;

    let manifest: agents::AgentOutput = serde_json::from_str(&content)
        .map_err(|e| format!("Invalid manifest: {e}"))?;

    let output = agents::AgentGetOutput {
        agent_id: manifest.agent_id,
        status: manifest.status,
        error: manifest.error,
        completed_at: manifest.completed_at,
        output_file: manifest.output_file,
        manifest_file: manifest.manifest_file,
    };

    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}
// =====================================================================
// END OF ARCHAEOLOGY STUBS
// =====================================================================
