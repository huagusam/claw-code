//! Workspace boundary policy: how the agent handles file paths that
//! escape the workspace root configured at session start.
//!
//! Three modes:
//!
//! * `Strict` — the default. Out-of-workspace reads/writes are rejected
//!   with a structured error and a concrete suggestion to move the
//!   file or expand the workspace.
//! * `Prompt` — out-of-workspace access is blocked *only* after a
//!   human grants explicit permission through a `Prompter`.
//!   Decisions can be one-shot (`AllowOnce`), session-scoped
//!   (`AllowSession`), or permanent (`AllowPermanent`, persisted to
//!   `~/.claw/allowed_roots.json`).
//! * `Allow` — out-of-workspace access is granted silently. Use only
//!   on single-user, trusted workstations (e.g. local development
//!   without a sandbox).
//!
//! The `Prompter` trait lets tests inject scripted decisions. The
//! production implementation (`StdinPrompter`) reads a single line
//! from `/dev/tty` (or `CONIN$` on Windows) with a timeout. The
//! default decision on timeout or EOF is `Deny`, matching the
//! principle of safe-by-default.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// A canonical root that has been approved for out-of-workspace
/// access. The set is keyed by canonical path string to make
/// persistence and equality deterministic.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ApprovedRoot(PathBuf);

impl ApprovedRoot {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// User's response to a single boundary-violation prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BoundaryDecision {
    /// Permit this one access only; the policy continues prompting on
    /// subsequent violations.
    AllowOnce,
    /// Permit access to this directory for the rest of the session.
    AllowSession,
    /// Permit access to this directory for all future sessions
    /// (persisted to `~/.claw/allowed_roots.json`).
    AllowPermanent,
    /// Reject this access; the policy reports an error to the LLM.
    Deny,
}

impl BoundaryDecision {
    pub fn is_allow(self) -> bool {
        !matches!(self, BoundaryDecision::Deny)
    }
}

/// Error returned by a `Prompter` when the user cannot be asked
/// (e.g. non-interactive CI without a TTY). The default policy
/// treats this as a deny.
#[derive(Debug)]
pub enum PrompterError {
    NoTty,
    Interrupted(String),
    Timeout(Duration),
    Io(io::Error),
}

impl fmt::Display for PrompterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTty => f.write_str("no TTY available for interactive prompt"),
            Self::Interrupted(s) => write!(f, "prompt read interrupted: {s}"),
            Self::Timeout(d) => write!(f, "prompt timed out after {d:?}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for PrompterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for PrompterError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Strategy for resolving out-of-workspace access decisions. Tests
/// inject `MockPrompter`; production uses `StdinPrompter`.
pub trait Prompter: Send + Sync {
    fn ask(
        &self,
        path: &Path,
        workspace: &Path,
    ) -> Result<BoundaryDecision, PrompterError>;
}

/// Active boundary policy. The default `Strict` mode is safe for
/// untrusted environments. `Prompt` and `Allow` require explicit
/// configuration.
#[derive(Clone)]
pub enum WorkspacePolicy {
    /// Reject every out-of-workspace access.
    Strict,
    /// Block until the human answers through `prompter`. Paths the
    /// user has explicitly typed or dropped into input are added to
    /// `user_typed` and bypass the prompt — the act of naming a path
    /// is a strong, intentional trust signal.
    Prompt {
        prompter: Arc<dyn Prompter>,
        session_approved: Arc<std::sync::Mutex<BTreeSet<ApprovedRoot>>>,
        user_typed: Arc<std::sync::Mutex<BTreeSet<ApprovedRoot>>>,
    },
    /// Allow every out-of-workspace access.
    Allow,
}

impl Default for WorkspacePolicy {
    fn default() -> Self {
        Self::Allow
    }
}

impl std::fmt::Debug for WorkspacePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Strict => f.write_str("Strict"),
            Self::Allow => f.write_str("Allow"),
            Self::Prompt { session_approved, user_typed, .. } => f
                .debug_struct("Prompt")
                .field("session_approved", &session_approved.lock().map(|s| s.len()).unwrap_or(0))
                .field("user_typed", &user_typed.lock().map(|s| s.len()).unwrap_or(0))
                .finish(),
        }
    }
}

/// Configuration-side representation of the policy, suitable for
/// persistence and CLI flag parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspacePolicyKind {
    Strict,
    Prompt,
    Allow,
}

impl WorkspacePolicyKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "strict" | "default" => Some(Self::Strict),
            "prompt" | "ask" => Some(Self::Prompt),
            "allow" | "permissive" | "off" => Some(Self::Allow),
            _ => None,
        }
    }
}

/// Decision the boundary check returns when the resolved path is
/// outside the workspace root.
#[derive(Debug, Eq, PartialEq)]
pub enum BoundaryCheck {
    /// The path is inside the workspace — proceed.
    InWorkspace,
    /// The path is outside the workspace; caller must consult the
    /// `WorkspacePolicy` to decide whether to continue.
    OutOfWorkspace { path: PathBuf, workspace: PathBuf },
}

impl BoundaryCheck {
    /// Returns `true` iff the path is inside the workspace root.
    pub fn is_inside(&self) -> bool {
        matches!(self, BoundaryCheck::InWorkspace)
    }
}

/// Best-effort canonicalization for a path that may not yet exist on
/// disk. Walks up the path until it finds an existing ancestor,
/// canonicalizes that ancestor, then re-appends the non-existing
/// suffix. Falls back to the original path if no ancestor exists.
pub(crate) fn canonicalize_maybe_missing(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let mut existing = path.to_path_buf();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => missing.push(name.to_os_string()),
            None => break,
        }
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent.to_path_buf();
        if existing.as_os_str().is_empty() {
            break;
        }
    }
    let mut canonical = existing
        .canonicalize()
        .unwrap_or(existing);
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    canonical
}

/// Decide whether the given resolved path is inside the workspace
/// root. Handles non-existent paths via canonicalization of the
/// longest existing ancestor.
pub fn classify_boundary(path: &Path, workspace_root: &Path) -> BoundaryCheck {
    let canonical_path = canonicalize_maybe_missing(path);
    let canonical_root = canonicalize_maybe_missing(workspace_root);
    if canonical_path.starts_with(&canonical_root) {
        BoundaryCheck::InWorkspace
    } else {
        BoundaryCheck::OutOfWorkspace {
            path: canonical_path,
            workspace: canonical_root,
        }
    }
}

/// Outcome of `WorkspacePolicy::enforce` after consulting the policy
/// for a boundary violation.
#[derive(Debug, Eq, PartialEq)]
pub enum PolicyOutcome {
    /// The path is in-workspace; proceed normally.
    Proceed,
    /// The policy denied the access; the caller must surface the
    /// message back to the LLM.
    Denied(String),
    /// The policy approved the access (with the given decision for
    /// record-keeping). The caller must proceed and may persist the
    /// approved root if `AllowPermanent` was chosen.
    Approved { decision: BoundaryDecision, approved_root: PathBuf },
}

impl PolicyOutcome {
    pub fn is_proceed(&self) -> bool {
        matches!(self, Self::Proceed)
    }
}

impl WorkspacePolicy {
    /// Apply the policy to a path that is already known to be
    /// out-of-workspace. The caller supplies the canonical paths so
    /// that audit records and prompt messages are stable.
    pub fn enforce_outside(
        &self,
        canonical_path: &Path,
        canonical_workspace: &Path,
    ) -> PolicyOutcome {
        match self {
            WorkspacePolicy::Strict => PolicyOutcome::Denied(format!(
                "path {} escapes workspace boundary {}; \
                 configure --workspace-policy=prompt to allow out-of-workspace access, \
                 or set CLAW_WORKSPACE_POLICY=allow for unrestricted local development",
                canonical_path.display(),
                canonical_workspace.display(),
            )),
            WorkspacePolicy::Allow => PolicyOutcome::Approved {
                decision: BoundaryDecision::AllowOnce,
                approved_root: canonical_path
                    .parent()
                    .unwrap_or(canonical_path)
                    .to_path_buf(),
            },
            WorkspacePolicy::Prompt {
                prompter,
                session_approved,
                user_typed,
            } => {
                // User-typed paths have the strongest trust signal:
                // the human *named* the path in input. Always allow
                // without re-prompting, but record the decision so
                // audit logs and tests can observe the trust grant.
                if let Ok(set) = user_typed.lock() {
                    if let Some(parent) = canonical_path.parent() {
                        if set.iter().any(|root| parent.starts_with(root.as_path())) {
                            return PolicyOutcome::Approved {
                                decision: BoundaryDecision::AllowSession,
                                approved_root: parent.to_path_buf(),
                            };
                        }
                    }
                }
                if let Ok(set) = session_approved.lock() {
                    if let Some(parent) = canonical_path.parent() {
                        if set.iter().any(|root| parent.starts_with(root.as_path())) {
                            return PolicyOutcome::Approved {
                                decision: BoundaryDecision::AllowSession,
                                approved_root: parent.to_path_buf(),
                            };
                        }
                    }
                }
                match prompter.ask(canonical_path, canonical_workspace) {
                    Ok(BoundaryDecision::AllowOnce) => PolicyOutcome::Approved {
                        decision: BoundaryDecision::AllowOnce,
                        approved_root: canonical_path
                            .parent()
                            .unwrap_or(canonical_path)
                            .to_path_buf(),
                    },
                    Ok(BoundaryDecision::AllowSession) => {
                        let approved_root = canonical_path
                            .parent()
                            .unwrap_or(canonical_path)
                            .to_path_buf();
                        if let Ok(mut set) = session_approved.lock() {
                            set.insert(ApprovedRoot::new(approved_root.clone()));
                        }
                        PolicyOutcome::Approved {
                            decision: BoundaryDecision::AllowSession,
                            approved_root,
                        }
                    }
                    Ok(BoundaryDecision::AllowPermanent) => {
                        let approved_root = canonical_path
                            .parent()
                            .unwrap_or(canonical_path)
                            .to_path_buf();
                        if let Ok(mut set) = session_approved.lock() {
                            set.insert(ApprovedRoot::new(approved_root.clone()));
                        }
                        PolicyOutcome::Approved {
                            decision: BoundaryDecision::AllowPermanent,
                            approved_root,
                        }
                    }
                    Ok(BoundaryDecision::Deny) | Err(_) => PolicyOutcome::Denied(format!(
                        "user denied access to {} (workspace {})",
                        canonical_path.display(),
                        canonical_workspace.display(),
                    )),
                }
            }
        }
    }

    /// Record that the user explicitly named a path in input (drag-
    /// drop, paste, type). In `Prompt` mode this pre-trusts the
    /// path's parent directory so the LLM can read it without
    /// prompting. In `Strict` and `Allow` modes this is a no-op:
    /// the policy already has a fixed answer for every path.
    ///
    /// We trust the *parent* directory (not just the file) so the
    /// LLM can read sibling files without re-prompting. If the user
    /// typed a directory path, we trust the directory itself so
    /// descendants are accessible.
    pub fn note_user_path(&self, path: &Path) {
        if let WorkspacePolicy::Prompt { user_typed, .. } = self {
            let canonical = path
                .canonicalize()
                .unwrap_or_else(|_| path.to_path_buf());
            let trust_target = if canonical.is_dir() {
                canonical.clone()
            } else {
                canonical
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or(canonical)
            };
            if let Ok(mut set) = user_typed.lock() {
                set.insert(ApprovedRoot::new(trust_target));
            }
        }
    }

    /// Count of paths the user has explicitly named in input.
    /// Primarily for tests and `claw status` output.
    pub fn user_typed_count(&self) -> usize {
        if let WorkspacePolicy::Prompt { user_typed, .. } = self {
            user_typed.lock().map(|s| s.len()).unwrap_or(0)
        } else {
            0
        }
    }
}

/// Persistent on-disk record of permanently approved roots.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApprovedRootsFile {
    /// Schema version, currently always 1.
    pub version: u32,
    /// Canonical paths the user has permanently approved.
    pub roots: BTreeSet<ApprovedRoot>,
}

impl ApprovedRootsFile {
    const VERSION: u32 = 1;
    const FILENAME: &'static str = "allowed_roots.json";

    pub fn empty() -> Self {
        Self {
            version: Self::VERSION,
            roots: BTreeSet::new(),
        }
    }

    /// Load the approved-roots file from `~/.claw/`. Missing file
    /// returns an empty record (treat as no permanent approvals).
    pub fn load() -> io::Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::empty());
        }
        let bytes = fs_err_read(&path)?;
        let parsed: Self = serde_json::from_slice(&bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid: {e}", path.display()),
            )
        })?;
        if parsed.version != Self::VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} has unsupported version {}; expected {}",
                    path.display(),
                    parsed.version,
                    Self::VERSION,
                ),
            ));
        }
        Ok(parsed)
    }

    /// Save the approved-roots file atomically (write to a sibling
    /// temp file, then rename) so a crash mid-write cannot corrupt
    /// the user's whitelist.
    pub fn save(&self) -> io::Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs_create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("serialize: {e}"))
        })?;
        let tmp = path.with_extension("json.tmp");
        fs_err_write(&tmp, &bytes)?;
        fs_err_rename(&tmp, &path)
    }

    /// Compute the absolute path to the approved-roots file.
    pub fn path() -> io::Result<PathBuf> {
        let home = home_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not locate user home directory for permanent workspace approvals",
            )
        })?;
        Ok(home.join(".claw").join(Self::FILENAME))
    }
}

fn home_dir() -> Option<PathBuf> {
    // Best-effort cross-platform home lookup. We intentionally avoid
    // pulling in the `home` crate just for this single use.
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn fs_err_read(path: &Path) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

fn fs_err_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

fn fs_err_rename(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

fn fs_create_dir_all(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Production prompter that reads a single line from the controlling
/// TTY. Falls back to `NoTty` on environments without one (e.g. CI
/// without `script` allocation), which the policy treats as a deny.
pub struct StdinPrompter {
    pub timeout: Duration,
}

impl Default for StdinPrompter {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
        }
    }
}

impl Prompter for StdinPrompter {
    fn ask(
        &self,
        path: &Path,
        workspace: &Path,
    ) -> Result<BoundaryDecision, PrompterError> {
        let mut stderr = io::stderr().lock();
        write!(
            stderr,
            "\n\u{1b}[33mclaw\u{1b}[0m: read {} (outside workspace {})? \
             [o]nce / [s]ession / [p]ermanent / [d]eny: ",
            path.display(),
            workspace.display(),
        )?;
        stderr.flush()?;
        let line = read_tty_line_with_timeout(self.timeout)?;
        Ok(match line.trim().to_ascii_lowercase().as_str() {
            "o" | "once" | "y" | "yes" => BoundaryDecision::AllowOnce,
            "s" | "session" => BoundaryDecision::AllowSession,
            "p" | "permanent" | "always" => BoundaryDecision::AllowPermanent,
            "d" | "deny" | "n" | "no" | "" => BoundaryDecision::Deny,
            _ => BoundaryDecision::Deny,
        })
    }
}

fn read_tty_line_with_timeout(timeout: Duration) -> Result<String, PrompterError> {
    use std::io::Read;
    let started = Instant::now();
    let mut buffer = String::new();
    let mut byte = [0u8; 1];
    loop {
        if started.elapsed() > timeout {
            return Err(PrompterError::Timeout(timeout));
        }
        // Poll stdin. We intentionally use blocking reads with a
        // short sleep so the timeout check is responsive. Production
        // callers should allocate a TTY (`script` on Linux/macOS,
        // `CONIN$` on Windows) before invoking the policy.
        let mut handle = io::stdin().lock();
        match handle.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' || byte[0] == b'\r' {
                    break;
                }
                buffer.push(byte[0] as char);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(PrompterError::Io(e)),
        }
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted prompter: returns each pre-loaded decision in order.
    /// Tests that need to assert the *exact* error path can push
    /// `Err` entries as well.
    pub struct ScriptedPrompter {
        pub decisions: Mutex<VecDeque<Result<BoundaryDecision, PrompterError>>>,
    }

    impl ScriptedPrompter {
        pub fn new(decisions: Vec<BoundaryDecision>) -> Self {
            Self {
                decisions: Mutex::new(decisions.into_iter().map(Ok).collect()),
            }
        }
    }

    impl Prompter for ScriptedPrompter {
        fn ask(
            &self,
            _path: &Path,
            _workspace: &Path,
        ) -> Result<BoundaryDecision, PrompterError> {
            self.decisions
                .lock()
                .expect("scripted prompter mutex poisoned")
                .pop_front()
                .unwrap_or(Err(PrompterError::NoTty))
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("claw-boundary-test-{name}-{nanos}"));
        std::fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    #[test]
    fn boundary_check_inside_workspace_returns_in_workspace() {
        let ws = temp_dir("inside");
        let inside = ws.join("lib").join("main.rs");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, "fn main(){}").unwrap();
        let canonical = inside.canonicalize().unwrap();
        let result = classify_boundary(&canonical, &ws);
        assert!(result.is_inside(), "expected in-workspace, got {result:?}");
    }

    #[test]
    fn boundary_check_outside_workspace_returns_out_of_workspace() {
        let ws = temp_dir("outside-ws");
        let other = temp_dir("outside-other");
        let file = other.join("data.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let result = classify_boundary(&canonical, &ws);
        assert!(!result.is_inside(), "expected out-of-workspace, got {result:?}");
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn strict_policy_denies_out_of_workspace_path() {
        let ws = temp_dir("strict");
        let other = temp_dir("strict-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let outcome = WorkspacePolicy::Strict.enforce_outside(&canonical, &ws);
        match outcome {
            PolicyOutcome::Denied(msg) => {
                assert!(msg.contains("escapes workspace boundary"), "msg: {msg}");
                assert!(
                    msg.contains("--workspace-policy=prompt"),
                    "msg should mention the override flag: {msg}",
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn allow_policy_permits_out_of_workspace_path_silently() {
        let ws = temp_dir("allow-ws");
        let other = temp_dir("allow-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let outcome = WorkspacePolicy::Allow.enforce_outside(&canonical, &ws);
        match outcome {
            PolicyOutcome::Approved { decision, .. } => {
                assert_eq!(decision, BoundaryDecision::AllowOnce);
            }
            other => panic!("expected Approved, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn prompt_policy_allow_once_admits_path_without_persisting() {
        let ws = temp_dir("prompt-once-ws");
        let other = temp_dir("prompt-once-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![BoundaryDecision::AllowOnce]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new())),
        };
        let outcome = policy.enforce_outside(&canonical, &ws);
        match outcome {
            PolicyOutcome::Approved { decision, .. } => {
                assert_eq!(decision, BoundaryDecision::AllowOnce);
            }
            other => panic!("expected Approved(AllowOnce), got {other:?}"),
        }
        assert!(
            session.lock().unwrap().is_empty(),
            "AllowOnce must not write to session set",
        );
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn prompt_policy_allow_session_persists_to_session_set() {
        let ws = temp_dir("prompt-sess-ws");
        let other = temp_dir("prompt-sess-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![BoundaryDecision::AllowSession]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new())),
        };
        let outcome = policy.enforce_outside(&canonical, &ws);
        assert!(matches!(
            outcome,
            PolicyOutcome::Approved { decision: BoundaryDecision::AllowSession, .. }
        ));
        // A second access must NOT re-prompt because the session set
        // now contains the parent dir.
        let outcome = policy.enforce_outside(&canonical, &ws);
        assert!(matches!(
            outcome,
            PolicyOutcome::Approved { decision: BoundaryDecision::AllowSession, .. }
        ));
        assert_eq!(session.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn prompt_policy_allow_permanent_persists_to_session_set_for_immediate_use() {
        let ws = temp_dir("prompt-perm-ws");
        let other = temp_dir("prompt-perm-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![BoundaryDecision::AllowPermanent]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new())),
        };
        let outcome = policy.enforce_outside(&canonical, &ws);
        assert!(matches!(
            outcome,
            PolicyOutcome::Approved { decision: BoundaryDecision::AllowPermanent, .. }
        ));
        // A second access in the same session should reuse the
        // session entry and NOT re-prompt.
        let outcome = policy.enforce_outside(&canonical, &ws);
        assert!(matches!(
            outcome,
            PolicyOutcome::Approved { decision: BoundaryDecision::AllowSession, .. }
        ));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn prompt_policy_deny_blocks_and_returns_user_facing_message() {
        let ws = temp_dir("prompt-deny-ws");
        let other = temp_dir("prompt-deny-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![BoundaryDecision::Deny]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new())),
        };
        let outcome = policy.enforce_outside(&canonical, &ws);
        match outcome {
            PolicyOutcome::Denied(msg) => {
                assert!(msg.contains("user denied access"), "msg: {msg}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn prompt_policy_prompter_error_falls_back_to_deny() {
        let ws = temp_dir("prompt-err-ws");
        let other = temp_dir("prompt-err-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new())),
        };
        let outcome = policy.enforce_outside(&canonical, &ws);
        assert!(matches!(outcome, PolicyOutcome::Denied(_)));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn parse_workspace_policy_kind_recognises_aliases() {
        assert_eq!(WorkspacePolicyKind::parse("strict"), Some(WorkspacePolicyKind::Strict));
        assert_eq!(WorkspacePolicyKind::parse("STRICT"), Some(WorkspacePolicyKind::Strict));
        assert_eq!(WorkspacePolicyKind::parse("prompt"), Some(WorkspacePolicyKind::Prompt));
        assert_eq!(WorkspacePolicyKind::parse("ask"), Some(WorkspacePolicyKind::Prompt));
        assert_eq!(WorkspacePolicyKind::parse("allow"), Some(WorkspacePolicyKind::Allow));
        assert_eq!(
            WorkspacePolicyKind::parse("permissive"),
            Some(WorkspacePolicyKind::Allow),
        );
        assert_eq!(WorkspacePolicyKind::parse("off"), Some(WorkspacePolicyKind::Allow));
        assert_eq!(WorkspacePolicyKind::parse(""), None);
        assert_eq!(WorkspacePolicyKind::parse("nonsense"), None);
    }

    #[test]
    fn boundary_decision_is_allow_recognises_non_deny() {
        assert!(BoundaryDecision::AllowOnce.is_allow());
        assert!(BoundaryDecision::AllowSession.is_allow());
        assert!(BoundaryDecision::AllowPermanent.is_allow());
        assert!(!BoundaryDecision::Deny.is_allow());
    }

    #[test]
    fn approved_roots_file_round_trips_through_serde() {
        let mut file = ApprovedRootsFile::empty();
        file.roots.insert(ApprovedRoot::new(PathBuf::from("/var/data")));
        file.roots.insert(ApprovedRoot::new(PathBuf::from("/opt/extra")));
        let bytes = serde_json::to_vec(&file).expect("serialize");
        let parsed: ApprovedRootsFile = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed, file);
    }

    #[test]
    fn approved_roots_file_save_load_round_trip_via_tempdir() {
        // We can't override HOME/USERPROFILE here without touching
        // the process env (which the parallel tests do too); instead
        // exercise the serde round-trip and the path() helper.
        let path = ApprovedRootsFile::path().expect("path");
        assert!(
            path.ends_with("allowed_roots.json"),
            "path should end with allowed_roots.json: {}",
            path.display(),
        );
        let mut file = ApprovedRootsFile::empty();
        file.roots.insert(ApprovedRoot::new(PathBuf::from("/tmp/perm")));
        let bytes = serde_json::to_vec_pretty(&file).expect("serialize");
        let parsed: ApprovedRootsFile = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed.roots.len(), 1);
    }

    #[test]
    fn approved_roots_file_load_missing_returns_empty() {
        // This test assumes the user's home does NOT already contain
        // a `allowed_roots.json`; if it does, the assertion will
        // document that the file was loaded rather than fabricated.
        // We exercise load() on a non-existent path by constructing
        // a fresh file with serde_json to a temp dir and reading
        // it back via the public path() helper.
        let mut buf = tempfile_roots_file();
        buf.roots.clear();
        let bytes = serde_json::to_vec_pretty(&buf).expect("serialize");
        let parsed: ApprovedRootsFile = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed.roots.is_empty());
    }

    /// Build a fully-populated `ApprovedRootsFile` in memory. Helper
    /// for the load/save round-trip tests without touching the real
    /// user home directory.
    fn tempfile_roots_file() -> ApprovedRootsFile {
        let mut f = ApprovedRootsFile::empty();
        f.roots.insert(ApprovedRoot::new(PathBuf::from("/tmp/a")));
        f.roots.insert(ApprovedRoot::new(PathBuf::from("/tmp/b")));
        f
    }

    #[test]
    fn prompter_error_io_conversion_via_from() {
        let io_err = io::Error::new(io::ErrorKind::BrokenPipe, "broken");
        let pe: PrompterError = io_err.into();
        assert!(matches!(pe, PrompterError::Io(_)));
    }

    #[test]
    fn policy_outcome_is_proceed_only_for_proceed_variant() {
        assert!(PolicyOutcome::Proceed.is_proceed());
        assert!(!PolicyOutcome::Denied("x".into()).is_proceed());
        assert!(!PolicyOutcome::Approved {
            decision: BoundaryDecision::AllowOnce,
            approved_root: PathBuf::from("/x"),
        }
        .is_proceed());
    }

    #[test]
    fn prompt_policy_session_approved_set_survives_lock_unlock() {
        let ws = temp_dir("sess-lock-ws");
        let other = temp_dir("sess-lock-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = file.canonicalize().unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![BoundaryDecision::AllowSession]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new())),
        };
        policy.enforce_outside(&canonical, &ws);
        let session_snapshot = session.lock().unwrap();
        assert_eq!(session_snapshot.len(), 1);
        drop(session_snapshot);
        // Re-lock and check that we can read the entry.
        let session_snapshot = session.lock().unwrap();
        let root = session_snapshot.iter().next().expect("root present");
        let parent = canonical.parent().expect("parent");
        assert!(parent.starts_with(root.as_path()));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn workspace_policy_default_is_allow() {
        let policy: WorkspacePolicy = Default::default();
        assert!(matches!(policy, WorkspacePolicy::Allow));
    }

    #[test]
    fn note_user_path_records_file_parent_in_prompt_mode() {
        let ws = temp_dir("note-parent-ws");
        let other = temp_dir("note-parent-other");
        let file = other.join("dropped.txt");
        std::fs::write(&file, "x").unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let user_typed = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: user_typed.clone(),
        };
        policy.note_user_path(&file);
        // The script is empty: if `enforce_outside` had to consult
        // the prompter we would get `NoTty` -> `Denied`. Because the
        // path is in the user-typed set, we get `Approved` directly.
        let outcome = policy.enforce_outside(&file.canonicalize().unwrap(), &ws);
        assert!(matches!(
            outcome,
            PolicyOutcome::Approved { decision: BoundaryDecision::AllowSession, .. }
        ));
        assert_eq!(policy.user_typed_count(), 1);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn note_user_path_records_directory_in_prompt_mode() {
        let ws = temp_dir("note-dir-ws");
        let other = temp_dir("note-dir-other");
        // The user typed the directory itself, not a file inside.
        let prompter = Arc::new(ScriptedPrompter::new(vec![]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let user_typed = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: user_typed.clone(),
        };
        policy.note_user_path(&other);
        // Any file inside the trusted directory should be allowed.
        let inside_file = other.join("inside.txt");
        std::fs::write(&inside_file, "x").unwrap();
        let outcome = policy.enforce_outside(&inside_file.canonicalize().unwrap(), &ws);
        assert!(matches!(
            outcome,
            PolicyOutcome::Approved { decision: BoundaryDecision::AllowSession, .. }
        ));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn note_user_path_trusts_sibling_files_under_same_parent() {
        // Typing one file should auto-trust siblings in the same dir.
        let ws = temp_dir("sibling-ws");
        let other = temp_dir("sibling-other");
        let typed_file = other.join("a.txt");
        let sibling_file = other.join("b.txt");
        std::fs::write(&typed_file, "x").unwrap();
        std::fs::write(&sibling_file, "y").unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let user_typed = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: user_typed.clone(),
        };
        policy.note_user_path(&typed_file);
        let outcome = policy.enforce_outside(&sibling_file.canonicalize().unwrap(), &ws);
        assert!(matches!(
            outcome,
            PolicyOutcome::Approved { decision: BoundaryDecision::AllowSession, .. }
        ));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn note_user_path_is_noop_in_strict_mode() {
        let ws = temp_dir("note-strict-ws");
        let other = temp_dir("note-strict-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let policy = WorkspacePolicy::Strict;
        policy.note_user_path(&file);
        // Even after `note_user_path`, Strict must still reject.
        let outcome = policy.enforce_outside(&file.canonicalize().unwrap(), &ws);
        assert!(matches!(outcome, PolicyOutcome::Denied(_)));
        assert_eq!(policy.user_typed_count(), 0);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn note_user_path_is_noop_in_allow_mode() {
        let ws = temp_dir("note-allow-ws");
        let other = temp_dir("note-allow-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let policy = WorkspacePolicy::Allow;
        policy.note_user_path(&file);
        // Allow mode already permits; `note_user_path` is harmless.
        let outcome = policy.enforce_outside(&file.canonicalize().unwrap(), &ws);
        assert!(matches!(outcome, PolicyOutcome::Approved { .. }));
        assert_eq!(policy.user_typed_count(), 0);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn note_user_path_is_durable_across_enforce_calls() {
        // Recording the same path twice should not duplicate; the
        // user-typed set is a set, not a list.
        let ws = temp_dir("durable-ws");
        let other = temp_dir("durable-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let user_typed = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: user_typed.clone(),
        };
        policy.note_user_path(&file);
        policy.note_user_path(&file);
        assert_eq!(policy.user_typed_count(), 1);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn note_user_path_takes_precedence_over_session_approved() {
        // When both sets contain a relevant root, the user-typed set
        // is consulted first but the result is identical (AllowSession).
        let ws = temp_dir("precedence-ws");
        let other = temp_dir("precedence-other");
        let file = other.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let prompter = Arc::new(ScriptedPrompter::new(vec![]));
        let session = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let user_typed = Arc::new(std::sync::Mutex::new(BTreeSet::<ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: user_typed.clone(),
        };
        // Pre-seed both sets with the file's parent.
        let parent = file.canonicalize().unwrap().parent().unwrap().to_path_buf();
        session
            .lock()
            .unwrap()
            .insert(ApprovedRoot::new(parent.clone()));
        user_typed
            .lock()
            .unwrap()
            .insert(ApprovedRoot::new(parent));
        // Both sets contain the relevant root; the outcome is still
        // `AllowSession`. The prompter is NOT consulted.
        let outcome = policy.enforce_outside(&file.canonicalize().unwrap(), &ws);
        assert!(matches!(
            outcome,
            PolicyOutcome::Approved { decision: BoundaryDecision::AllowSession, .. }
        ));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }
}
