use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use runtime::{
    dedupe_superseded_commit_events, LaneCommitProvenance, LaneEvent, LaneEventBlocker,
    LaneFailureClass,
};

use crate::types::{AgentOutput, AgentStatus};

pub const DEFAULT_AGENT_MODEL: &str = "claude-opus-4-6";
pub const DEFAULT_AGENT_SYSTEM_DATE: &str = "2026-03-31";
pub const DEFAULT_AGENT_MAX_ITERATIONS: usize = 32;

static AGENT_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn agent_store_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("CLAW_AGENT_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("CLAWD_AGENT_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    for ancestor in cwd.ancestors() {
        if ancestor.join(".claw").is_dir() {
            return Ok(ancestor.join(".claw-agents"));
        }
    }
    if let Some(workspace_root) = cwd.ancestors().nth(2) {
        return Ok(workspace_root.join(".claw-agents"));
    }
    Ok(cwd.join(".claw-agents"))
}

pub fn make_agent_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|error| {
            eprintln!("[agent] system clock is before epoch ({error}); using 0 for agent ID");
            std::time::Duration::ZERO
        })
        .as_nanos();
    let n = AGENT_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("agent-{nanos:x}-{n:x}")
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|error| {
            eprintln!("[agent] system clock is before epoch ({error}); using 0 for timestamp");
            std::time::Duration::ZERO
        })
        .as_secs()
}

pub fn slugify_agent_name(description: &str) -> String {
    let mut out: String = description
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(32).collect()
}

pub fn write_agent_manifest(manifest: &AgentOutput) -> Result<(), String> {
    let mut normalized = manifest.clone();
    normalized.lane_events = dedupe_superseded_commit_events(&normalized.lane_events);
    let bytes = serde_json::to_string_pretty(&normalized).map_err(|error| error.to_string())?;
    let path = std::path::Path::new(&normalized.manifest_file);
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|error| error.to_string())?;
        f.write_all(bytes.as_bytes())
            .map_err(|error| error.to_string())?;
        f.sync_all().map_err(|error| error.to_string())?;
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.to_string());
    }
    Ok(())
}

/// Best-effort transition: Created -> Running. Sets `status` and
/// `started_at` and writes the manifest. A failure to write is logged
/// by the caller; this function returns the error so the caller can
/// decide whether to abort the spawn.
pub fn mark_agent_running(manifest: &AgentOutput, started_at_secs: u64) -> Result<(), String> {
    let mut next = manifest.clone();
    next.status = AgentStatus::Running;
    next.started_at = Some(started_at_secs);
    next.derived_state = derive_agent_state("running", None, None, None).to_string();
    write_agent_manifest(&next)
}

pub fn persist_agent_terminal_state(
    manifest: &AgentOutput,
    status: AgentStatus,
    result: Option<&str>,
    error: Option<String>,
) -> Result<(), String> {
    debug_assert_ne!(
        status,
        AgentStatus::Running,
        "persist_agent_terminal_state is for terminal states only; \
         use mark_agent_running for the Created->Running transition"
    );
    debug_assert_eq!(
        error.is_some(),
        status == AgentStatus::Failed,
        "invariant: error.is_some() <=> status == Failed"
    );
    if error.is_none() && status == AgentStatus::Failed {
        return Err(String::from("Failed status requires Some(error)"));
    }
    let blocker = error.as_deref().map(classify_lane_blocker);
    let terminal_status = status.as_terminal_str();
    append_agent_output(
        &manifest.output_file,
        &format_agent_terminal_output(terminal_status, result, blocker.as_ref(), error.as_deref()),
    )?;
    let mut next_manifest = manifest.clone();
    next_manifest.status = status;
    let now_secs = unix_now();
    next_manifest.completed_at = Some(now_secs.max(manifest.created_at));
    next_manifest.current_blocker.clone_from(&blocker);
    let status_str = status.to_string();
    next_manifest.derived_state =
        derive_agent_state(&status_str, result, error.as_deref(), blocker.as_ref()).to_string();
    next_manifest.error = error;
    let now_str = now_secs.to_string();
    if let Some(blocker) = blocker {
        next_manifest.lane_events.push(LaneEvent::blocked(now_str.clone(), &blocker));
        next_manifest.lane_events.push(LaneEvent::failed(now_str.clone(), &blocker));
    } else {
        next_manifest.current_blocker = None;
        next_manifest
            .lane_events
            .push(LaneEvent::finished(now_str.clone(), None));
        if let Some(provenance) = maybe_commit_provenance(result) {
            next_manifest.lane_events.push(LaneEvent::commit_created(
                now_str.clone(),
                Some(format!("commit {}", provenance.commit)),
                provenance,
            ));
        }
    }
    write_agent_manifest(&next_manifest)
}

pub fn append_agent_output(path: &str, suffix: &str) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(suffix.as_bytes())
        .map_err(|error| error.to_string())
}

pub fn format_agent_terminal_output(
    status: &str,
    result: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
    error: Option<&str>,
) -> String {
    let mut sections = vec![format!("\n## Result\n\n- status: {status}\n")];
    if let Some(blocker) = blocker {
        sections.push(format!(
            "\n### Blocker\n\n- failure_class: {}\n- detail: {}\n",
            serde_json::to_string(&blocker.failure_class)
                .unwrap_or_else(|_| "\"infra\"".to_string())
                .trim_matches('"'),
            blocker.detail.trim()
        ));
    }
    if let Some(result) = result.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Final response\n\n{}\n", result.trim()));
    }
    if let Some(error) = error.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Error\n\n{}\n", error.trim()));
    }
    sections.join("")
}

pub fn derive_agent_state(
    status: &str,
    result: Option<&str>,
    error: Option<&str>,
    #[allow(unused_variables)] blocker: Option<&LaneEventBlocker>,
) -> &'static str {
    let normalized_status = status.trim().to_ascii_lowercase();
    let normalized_error = error.unwrap_or_default().to_ascii_lowercase();

    if normalized_status == "running" {
        return "working";
    }
    if normalized_status == "completed" {
        return if result.is_some_and(|value| !value.trim().is_empty()) {
            "finished_cleanable"
        } else {
            "finished_pending_report"
        };
    }
    if normalized_error.contains("background") {
        return "blocked_background_job";
    }
    if normalized_error.contains("merge conflict") || normalized_error.contains("cherry-pick") {
        return "blocked_merge_conflict";
    }
    if normalized_error.contains("mcp") {
        return "degraded_mcp";
    }
    if normalized_error.contains("transport")
        || normalized_error.contains("broken pipe")
        || normalized_error.contains("connection")
        || normalized_error.contains("interrupted")
    {
        return "interrupted_transport";
    }
    "truly_idle"
}

fn classify_lane_blocker(error: &str) -> LaneEventBlocker {
    let detail = error.trim().to_string();
    LaneEventBlocker {
        failure_class: classify_lane_failure(error),
        detail,
        subphase: None,
    }
}

fn classify_lane_failure(error: &str) -> LaneFailureClass {
    let normalized = error.to_ascii_lowercase();

    if normalized.contains("prompt") && normalized.contains("deliver") {
        LaneFailureClass::PromptDelivery
    } else if normalized.contains("trust") {
        LaneFailureClass::TrustGate
    } else if normalized.contains("branch")
        && (normalized.contains("stale") || normalized.contains("diverg"))
    {
        LaneFailureClass::BranchDivergence
    } else if normalized.contains("gateway") || normalized.contains("routing") {
        LaneFailureClass::GatewayRouting
    } else if normalized.contains("compile")
        || normalized.contains("build failed")
        || normalized.contains("cargo check")
    {
        LaneFailureClass::Compile
    } else if normalized.contains("test") {
        LaneFailureClass::Test
    } else if normalized.contains("tool failed")
        || normalized.contains("runtime tool")
        || normalized.contains("tool runtime")
    {
        LaneFailureClass::ToolRuntime
    } else if normalized.contains("workspace") && normalized.contains("mismatch") {
        LaneFailureClass::WorkspaceMismatch
    } else if normalized.contains("plugin") {
        LaneFailureClass::PluginStartup
    } else if normalized.contains("mcp") && normalized.contains("handshake") {
        LaneFailureClass::McpHandshake
    } else if normalized.contains("mcp") {
        LaneFailureClass::McpStartup
    } else {
        LaneFailureClass::Infra
    }
}

fn maybe_commit_provenance(result: Option<&str>) -> Option<LaneCommitProvenance> {
    let commit = extract_commit_sha(result?)?;
    let branch = current_git_branch().unwrap_or_else(|| "unknown".to_string());
    let worktree = std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string());
    Some(LaneCommitProvenance {
        commit: commit.clone(),
        branch,
        worktree,
        canonical_commit: Some(commit.clone()),
        superseded_by: None,
        lineage: vec![commit],
    })
}

/// Extract a commit SHA reference from a free-form result string.
///
/// The strategy is intentionally conservative: a 40-character token of
/// hex digits is a strong signal (full SHA-1) and is accepted whenever
/// it appears. Shorter SHAs are only accepted when a contextual marker
/// (`commit `, `sha `, `sha:`, or `@`) precedes them, and only within
/// the 7..=12 char window. UUID fragments, raw 16-char hex sequences,
/// and short hex embedded in URLs are all rejected.
pub fn extract_commit_sha(result: &str) -> Option<String> {
    for token in result.split(|c: char| !c.is_ascii_hexdigit()) {
        if token.len() == 40 {
            return Some(token.to_string());
        }
    }
    let lower = result.to_ascii_lowercase();
    for marker in ["commit ", "sha ", "sha:", "@"] {
        if let Some(idx) = lower.find(marker) {
            let after = &result[idx + marker.len()..];
            let token: String = after.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if (7..=12).contains(&token.len()) {
                return Some(token);
            }
        }
    }
    None
}

fn current_git_branch() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
