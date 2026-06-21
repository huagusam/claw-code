use std::cmp::Reverse;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use glob::Pattern;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use xxhash_rust::xxh3::xxh3_64;

use crate::boundary::{
    canonicalize_maybe_missing, classify_boundary, BoundaryCheck, PolicyOutcome, WorkspacePolicy,
};

/// Maximum file size that can be read (10 MB).
const MAX_READ_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum file size that can be written (10 MB).
const MAX_WRITE_SIZE: usize = 10 * 1024 * 1024;

/// Check whether a file appears to contain binary content by examining
/// the first chunk for NUL bytes.
fn is_binary_file(path: &Path) -> io::Result<bool> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 8192];
    let bytes_read = file.read(&mut buffer)?;
    Ok(buffer[..bytes_read].contains(&0))
}

/// Normalize path for output by converting backslashes to forward slashes.
/// This ensures consistent path format in JSON responses across platforms.
fn normalize_path_for_output(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Text payload returned by file-reading operations.
/// Content is returned by default (`full: true`); pass `full: false`
/// for a token-light payload that omits `content`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextFilePayload {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub checksum: String,
    #[serde(rename = "bytesRead")]
    pub bytes_read: usize,
    #[serde(rename = "numLines")]
    pub num_lines: usize,
    #[serde(rename = "startLine")]
    pub start_line: usize,
    #[serde(rename = "totalLines")]
    pub total_lines: usize,
}

/// Output envelope for the `read_file` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    pub file: TextFilePayload,
}

/// Structured patch hunk emitted by write and edit operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredPatchHunk {
    #[serde(rename = "oldStart")]
    pub old_start: usize,
    #[serde(rename = "oldLines")]
    pub old_lines: usize,
    #[serde(rename = "newStart")]
    pub new_start: usize,
    #[serde(rename = "newLines")]
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// Syntax validation result for write/edit operations.
/// Binary or unknown types are `Skipped`; parse errors carry the error message and line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxCheck {
    Valid,
    Invalid {
        message: String,
        line: Option<usize>,
    },
    Skipped,
}

/// Output envelope for full-file write operations.
/// Includes a content preview (truncated) so the model can verify the
/// new contents without re-reading the file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub checksum: String,
    #[serde(rename = "bytesWritten")]
    pub bytes_written: usize,
    #[serde(rename = "linesWritten")]
    pub lines_written: usize,
    /// Truncated preview of the file content *after* the write, so the
    /// model can verify the change. `None` only when the file is too
    /// large to preview. By default the preview is included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax: Option<SyntaxCheck>,
}

/// Output envelope for targeted string-replacement edits.
/// Includes a content preview (truncated) so the model can verify the
/// change without re-reading the file. The full file is not echoed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "oldString")]
    pub old_string: String,
    #[serde(rename = "newString")]
    pub new_string: String,
    #[serde(rename = "newChecksum")]
    pub new_checksum: String,
    #[serde(rename = "bytesChanged")]
    pub bytes_changed: isize,
    #[serde(rename = "linesChanged")]
    pub lines_changed: usize,
    /// Number of times `old_string` matched in the file. Useful for
    /// detecting ambiguity: if > 1 and `replace_all` was not requested,
    /// the caller may have hit the wrong occurrence and should re-read
    /// the file to verify.
    #[serde(rename = "occurrencesMatched", default)]
    pub occurrences_matched: usize,
    #[serde(rename = "diffSummary")]
    pub diff_summary: String,
    /// Truncated preview of the file content *after* the edit, so the
    /// model can verify the change. `None` only when the file is too
    /// large to preview. By default the preview is included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax: Option<SyntaxCheck>,
}

/// Result of a glob-based filename search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobSearchOutput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub truncated: bool,
}

/// Parameters accepted by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchInput {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(rename = "output_mode")]
    pub output_mode: Option<String>,
    #[serde(rename = "-B")]
    pub before: Option<usize>,
    #[serde(rename = "-A")]
    pub after: Option<usize>,
    #[serde(rename = "-C")]
    pub context_short: Option<usize>,
    pub context: Option<usize>,
    #[serde(rename = "-n")]
    pub line_numbers: Option<bool>,
    #[serde(rename = "-i")]
    pub case_insensitive: Option<bool>,
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    pub multiline: Option<bool>,
}

/// Result payload returned by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchOutput {
    pub mode: Option<String>,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub content: Option<String>,
    #[serde(rename = "numLines")]
    pub num_lines: Option<usize>,
    #[serde(rename = "numMatches")]
    pub num_matches: Option<usize>,
    #[serde(rename = "appliedLimit")]
    pub applied_limit: Option<usize>,
    #[serde(rename = "appliedOffset")]
    pub applied_offset: Option<usize>,
}

/// Reads a text file and returns a line-windowed payload.
///
/// When `full` is `Some(true)` (default) the entire selected window is returned
/// in `content`; when `Some(false)`, `content` is `None` (token-light mode).
pub fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    full: Option<bool>,
) -> io::Result<ReadFileOutput> {
    let absolute_path = normalize_path(path)?;

    // Check file size before reading
    let metadata = fs::metadata(&absolute_path)?;
    if metadata.len() > MAX_READ_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file is too large ({} bytes, max {} bytes)",
                metadata.len(),
                MAX_READ_SIZE
            ),
        ));
    }

    // Detect binary files
    if is_binary_file(&absolute_path)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file appears to be binary",
        ));
    }

    let content = fs::read_to_string(&absolute_path)?;
    let checksum = format!("{:016x}", xxh3_64(content.as_bytes()));
    let lines: Vec<&str> = content.lines().collect();
    let start_index = offset.unwrap_or(0).min(lines.len());
    let end_index = limit.map_or(lines.len(), |limit| {
        start_index.saturating_add(limit).min(lines.len())
    });
    let selected = lines[start_index..end_index].join("\n");
    let bytes_read = selected.len();

    let content = if full == Some(false) {
        None
    } else {
        Some(selected)
    };

    Ok(ReadFileOutput {
        kind: String::from("text"),
        file: TextFilePayload {
            file_path: absolute_path.to_string_lossy().into_owned(),
            content,
            checksum,
            bytes_read,
            num_lines: end_index.saturating_sub(start_index),
            start_line: start_index.saturating_add(1),
            total_lines: lines.len(),
        },
    })
}

/// Maximum bytes for an echoed `content_preview` on write/edit results.
/// 2 KiB keeps the tool_result envelope small while still giving the
/// model enough text to verify a single targeted change.
const CONTENT_PREVIEW_MAX: usize = 2_048;

/// Returns a truncated preview of the given content, or `None` when
/// the content is empty. The preview is wrapped in [`CONTENT_PREVIEW_MAX`]
/// bytes; truncation is indicated by a trailing marker so the model
/// knows the echo was clipped.
fn preview_for(content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    if content.len() <= CONTENT_PREVIEW_MAX {
        return Some(content.to_owned());
    }
    let mut end = CONTENT_PREVIEW_MAX;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 64);
    out.push_str(&content[..end]);
    out.push_str("\n…[truncated, full content written to file]");
    Some(out)
}

/// Creates a new file and returns metadata plus a truncated content preview.
/// When `force` is false (default), fails if the file already exists — use `edit_file` to modify.
/// When `force` is true, overwrites the existing file entirely.
pub fn new_file(path: &str, content: &str, force: bool) -> io::Result<WriteFileOutput> {
    if content.len() > MAX_WRITE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "content is too large ({} bytes, max {} bytes)",
                content.len(),
                MAX_WRITE_SIZE
            ),
        ));
    }

    let absolute_path = normalize_path_allow_missing(path)?;

    if absolute_path.exists() && absolute_path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "path '{}' is a directory, cannot create file",
                absolute_path.display()
            ),
        ));
    }

    let is_existing = absolute_path.exists();

    if is_existing && !force {
        let existing = fs::read_to_string(&absolute_path).unwrap_or_default();
        let line_count = existing.lines().count();
        let byte_count = existing.len();
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "File already exists at '{}' ({} lines, {} bytes). \
                 Use `edit_file` to modify existing files, \
                 or set `force: true` to overwrite entirely.",
                absolute_path.display(), line_count, byte_count
            ),
        ));
    }

    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if is_existing {
        // Overwrite mode: truncate + write
        fs::write(&absolute_path, content)?;
    } else {
        // Atomic create: fails if file was created between our exists() check and now.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&absolute_path)?;
        file.write_all(content.as_bytes())?;
    }

    let checksum = format!("{:016x}", xxh3_64(content.as_bytes()));
    let bytes_written = content.len();
    let lines_written = if content.is_empty() { 0 } else { content.lines().count() };

    Ok(WriteFileOutput {
        kind: if is_existing { String::from("overwrite") } else { String::from("create") },
        file_path: normalize_path_for_output(&absolute_path),
        checksum,
        bytes_written,
        lines_written,
        content_preview: preview_for(content),
        syntax: Some(validate_syntax(&absolute_path, content)),
    })
}

/// Performs an in-file string replacement and returns metadata plus a
/// truncated content preview so the model can verify the change.
pub fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    expected_checksum: Option<&str>,
) -> io::Result<EditFileOutput> {
    let absolute_path = normalize_path(path)?;
    let original_content = fs::read_to_string(&absolute_path)?;

    if let Some(expected) = expected_checksum {
        let actual = format!("{:016x}", xxh3_64(original_content.as_bytes()));
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("expected checksum {expected} but current file checksum is {actual}"),
            ));
        }
    }

    if old_string == new_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "old_string and new_string must differ",
        ));
    }
    if !original_content.contains(old_string) {
        let line_count = original_content.lines().count();
        let tail: Vec<&str> = original_content.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect();
        let tail_preview = tail.join("\n");
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "old_string not found in file ({} lines). \
                 The file may have been modified since you last read it. \
                 Last 5 lines of the file:
---
{}
---
\
                 Please call read_file to see the current content before retrying.",
                line_count, tail_preview
            ),
        ));
    }

    let occurrences_matched = original_content.matches(old_string).count();

    let new_content = if replace_all {
        original_content.replace(old_string, new_string)
    } else {
        original_content.replacen(old_string, new_string, 1)
    };
    fs::write(&absolute_path, &new_content)?;

    let new_checksum = format!("{:016x}", xxh3_64(new_content.as_bytes()));
    let bytes_changed = new_content.len() as isize - original_content.len() as isize;

    let patch = make_patch(&original_content, &new_content);
    let lines_changed: usize = patch.iter().map(|h| h.lines.len()).sum();

    let diff_summary = if serde_json::to_string(&patch).map_or(true, |s| s.len() > 2048) {
        serde_json::json!({
            "truncated": true,
            "hunks_count": patch.len(),
            "first_hunk_range": patch.first().map(|h| {
                format!("@@ -{},{} +{},{} @@", h.old_start, h.old_lines, h.new_start, h.new_lines)
            }).unwrap_or_default(),
            "total_lines_changed": lines_changed,
        }).to_string()
    } else {
        serde_json::to_string(&patch).unwrap_or_default()
    };

    Ok(EditFileOutput {
        kind: String::from("edit"),
        file_path: normalize_path_for_output(&absolute_path),
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        new_checksum,
        bytes_changed,
        lines_changed,
        occurrences_matched,
        diff_summary,
        content_preview: preview_for(&new_content),
        syntax: Some(validate_syntax(&absolute_path, &new_content)),
    })
}

/// Expands a glob pattern and returns matching filenames.
pub fn glob_search(pattern: &str, path: Option<&str>) -> io::Result<GlobSearchOutput> {
    let started = Instant::now();
    let base_dir = path
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let search_pattern = if Path::new(pattern).is_absolute() {
        pattern.to_owned()
    } else {
        base_dir.join(pattern).to_string_lossy().into_owned()
    };

    // The `glob` crate does not support brace expansion ({a,b,c}).
    // Expand braces into multiple patterns so patterns like
    // `Assets/**/*.{cs,uxml,uss}` work correctly.
    let expanded = expand_braces(&search_pattern);

    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    for pat in &expanded {
        let entries = glob::glob(pat)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        for entry in entries.flatten() {
            if entry.is_file() && seen.insert(entry.clone()) {
                matches.push(entry);
            }
        }
    }

    matches.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(Reverse)
    });

    let truncated = matches.len() > 100;
    let filenames = matches
        .into_iter()
        .take(100)
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    Ok(GlobSearchOutput {
        duration_ms: started.elapsed().as_millis(),
        num_files: filenames.len(),
        filenames,
        truncated,
    })
}

/// Runs a regex search over workspace files with optional context lines.
pub fn grep_search(input: &GrepSearchInput) -> io::Result<GrepSearchOutput> {
    let base_path = input
        .path
        .as_deref()
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);

    let regex = RegexBuilder::new(&input.pattern)
        .case_insensitive(input.case_insensitive.unwrap_or(false))
        .dot_matches_new_line(input.multiline.unwrap_or(false))
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let glob_filter = input
        .glob
        .as_deref()
        .map(Pattern::new)
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let file_type = input.file_type.as_deref();
    let output_mode = input
        .output_mode
        .clone()
        .unwrap_or_else(|| String::from("files_with_matches"));
    let context = input.context.or(input.context_short).unwrap_or(0);

    let mut filenames = Vec::new();
    let mut content_lines = Vec::new();
    let mut total_matches = 0usize;

    for file_path in collect_search_files(&base_path)? {
        if !matches_optional_filters(&file_path, glob_filter.as_ref(), file_type) {
            continue;
        }

        let Ok(file_contents) = fs::read_to_string(&file_path) else {
            continue;
        };

        if output_mode == "count" {
            let count = regex.find_iter(&file_contents).count();
            if count > 0 {
                filenames.push(file_path.to_string_lossy().into_owned());
                total_matches += count;
            }
            continue;
        }

        let lines: Vec<&str> = file_contents.lines().collect();
        let mut matched_lines = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if regex.is_match(line) {
                total_matches += 1;
                matched_lines.push(index);
            }
        }

        if matched_lines.is_empty() {
            continue;
        }

        filenames.push(file_path.to_string_lossy().into_owned());
        if output_mode == "content" {
            for index in matched_lines {
                let start = index.saturating_sub(input.before.unwrap_or(context));
                let end = (index + input.after.unwrap_or(context) + 1).min(lines.len());
                for (current, line) in lines.iter().enumerate().take(end).skip(start) {
                    let prefix = if input.line_numbers.unwrap_or(true) {
                        format!("{}:{}:", file_path.to_string_lossy(), current + 1)
                    } else {
                        format!("{}:", file_path.to_string_lossy())
                    };
                    content_lines.push(format!("{prefix}{line}"));
                }
            }
        }
    }

    let (filenames, applied_limit, applied_offset) =
        apply_limit(filenames, input.head_limit, input.offset);
    let content_output = if output_mode == "content" {
        let (lines, limit, offset) = apply_limit(content_lines, input.head_limit, input.offset);
        return Ok(GrepSearchOutput {
            mode: Some(output_mode),
            num_files: filenames.len(),
            filenames,
            num_lines: Some(lines.len()),
            content: Some(lines.join("\n")),
            num_matches: None,
            applied_limit: limit,
            applied_offset: offset,
        });
    } else {
        None
    };

    Ok(GrepSearchOutput {
        mode: Some(output_mode.clone()),
        num_files: filenames.len(),
        filenames,
        content: content_output,
        num_lines: None,
        num_matches: (output_mode == "count").then_some(total_matches),
        applied_limit,
        applied_offset,
    })
}

fn collect_search_files(base_path: &Path) -> io::Result<Vec<PathBuf>> {
    if base_path.is_file() {
        return Ok(vec![base_path.to_path_buf()]);
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(base_path) {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

fn matches_optional_filters(
    path: &Path,
    glob_filter: Option<&Pattern>,
    file_type: Option<&str>,
) -> bool {
    if let Some(glob_filter) = glob_filter {
        let path_string = path.to_string_lossy();
        if !glob_filter.matches(&path_string) && !glob_filter.matches_path(path) {
            return false;
        }
    }

    if let Some(file_type) = file_type {
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension != Some(file_type) {
            return false;
        }
    }

    true
}

fn apply_limit<T>(
    items: Vec<T>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> (Vec<T>, Option<usize>, Option<usize>) {
    let offset_value = offset.unwrap_or(0);
    let mut items = items.into_iter().skip(offset_value).collect::<Vec<_>>();
    let explicit_limit = limit.unwrap_or(250);
    if explicit_limit == 0 {
        return (items, None, (offset_value > 0).then_some(offset_value));
    }

    let truncated = items.len() > explicit_limit;
    items.truncate(explicit_limit);
    (
        items,
        truncated.then_some(explicit_limit),
        (offset_value > 0).then_some(offset_value),
    )
}

fn make_patch(original: &str, updated: &str) -> Vec<StructuredPatchHunk> {
    let mut lines = Vec::new();
    for line in original.lines() {
        lines.push(format!("-{line}"));
    }
    for line in updated.lines() {
        lines.push(format!("+{line}"));
    }

    vec![StructuredPatchHunk {
        old_start: 1,
        old_lines: original.lines().count(),
        new_start: 1,
        new_lines: updated.lines().count(),
        lines,
    }]
}

fn normalize_path(path: &str) -> io::Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    if !candidate.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("file not found: {}", candidate.display()),
        ));
    }
    Ok(candidate)
}

fn normalize_path_allow_missing(path: &str) -> io::Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(candidate)
}

/// Read a file with workspace boundary enforcement that consults a
/// `WorkspacePolicy` on out-of-workspace paths. When the path is
/// inside the workspace, behavior is identical to
/// `read_file_in_workspace`. When the path escapes the workspace, the
/// policy decides: `Strict` denies, `Allow` permits silently, and
/// `Prompt` asks the human.
#[allow(dead_code)]
pub fn read_file_with_policy(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    workspace_root: &Path,
    policy: &WorkspacePolicy,
    full: Option<bool>,
) -> io::Result<ReadFileOutput> {
    let absolute_path = normalize_path(path)?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_path = canonicalize_maybe_missing(&absolute_path);
    let check = classify_boundary(&canonical_path, &canonical_root);
    if matches!(check, BoundaryCheck::OutOfWorkspace { .. }) {
        match policy.enforce_outside(&canonical_path, &canonical_root) {
            PolicyOutcome::Proceed | PolicyOutcome::Approved { .. } => {}
            PolicyOutcome::Denied(msg) => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
            }
        }
    }
    // `full` flows through: callers that want the default LLM-friendly
    // echo pass `None` (or `Some(true)`); callers that need the
    // legacy token-light payload pass `Some(false)`. A prior version
    // hardcoded `None` here, which silently ignored `full: false`
    // and always echoed the content.
    read_file(path, offset, limit, full)
}

/// Write a file with workspace boundary enforcement that consults a
/// `WorkspacePolicy` on out-of-workspace paths. See
/// `read_file_with_policy` for the policy contract.
#[allow(dead_code)]
pub fn new_file_with_policy(
    path: &str,
    content: &str,
    force: bool,
    workspace_root: &Path,
    policy: &WorkspacePolicy,
) -> io::Result<WriteFileOutput> {
    let absolute_path = normalize_path_allow_missing(path)?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_path = canonicalize_maybe_missing(&absolute_path);
    let check = classify_boundary(&canonical_path, &canonical_root);
    if matches!(check, BoundaryCheck::OutOfWorkspace { .. }) {
        match policy.enforce_outside(&canonical_path, &canonical_root) {
            PolicyOutcome::Proceed | PolicyOutcome::Approved { .. } => {}
            PolicyOutcome::Denied(msg) => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
            }
        }
    }
    new_file(path, content, force)
}

/// Edit a file with workspace boundary enforcement that consults a
/// `WorkspacePolicy` on out-of-workspace paths. See
/// `read_file_with_policy` for the policy contract.
#[allow(dead_code)]
pub fn edit_file_with_policy(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    expected_checksum: Option<&str>,
    workspace_root: &Path,
    policy: &WorkspacePolicy,
) -> io::Result<EditFileOutput> {
    let absolute_path = normalize_path(path)?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_path = canonicalize_maybe_missing(&absolute_path);
    let check = classify_boundary(&canonical_path, &canonical_root);
    if matches!(check, BoundaryCheck::OutOfWorkspace { .. }) {
        match policy.enforce_outside(&canonical_path, &canonical_root) {
            PolicyOutcome::Proceed | PolicyOutcome::Approved { .. } => {}
            PolicyOutcome::Denied(msg) => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
            }
        }
    }
    edit_file(path, old_string, new_string, replace_all, expected_checksum)
}

/// Expand shell-style brace groups in a glob pattern.
///
/// Handles one level of braces: `foo.{a,b,c}` → `["foo.a", "foo.b", "foo.c"]`.
/// Nested braces are not expanded (uncommon in practice).
/// Patterns without braces pass through unchanged.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else {
        // Unmatched brace — treat as literal.
        return vec![pattern.to_owned()];
    };
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let alternatives = &pattern[open + 1..close];
    alternatives
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{prefix}{alt}{suffix}")))
        .collect()
}

/// Validate file syntax based on extension.
/// Returns `Valid` for well-formed JSON/TOML, `Invalid(reason)` for parse errors,
/// or `Skipped` for unsupported or binary file types.
fn validate_syntax(path: &Path, content: &str) -> SyntaxCheck {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => match serde_json::from_str::<serde_json::Value>(content) {
            Ok(_) => SyntaxCheck::Valid,
            Err(e) => SyntaxCheck::Invalid {
                message: e.to_string(),
                line: Some(e.line()),
            },
        },
        Some("toml") => match toml::from_str::<toml::Value>(content) {
            Ok(_) => SyntaxCheck::Valid,
            Err(e) => SyntaxCheck::Invalid {
                message: e.to_string(),
                line: None,
            },
        },
        _ => SyntaxCheck::Skipped,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        edit_file, expand_braces, glob_search, grep_search, new_file, new_file_with_policy,
        preview_for, read_file, read_file_with_policy, GrepSearchInput, MAX_WRITE_SIZE,
    };
    use crate::boundary::{BoundaryDecision, Prompter, PrompterError, WorkspacePolicy};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir().join(format!("clawd-native-{name}-{unique}"))
    }

    #[test]
    fn reads_and_writes_files() {
        let path = temp_path("read-write.txt");
        let write_output = new_file(path.to_string_lossy().as_ref(), "one\ntwo\nthree", false)
            .expect("write should succeed");
        assert_eq!(write_output.kind, "create");

        let read_output = read_file(path.to_string_lossy().as_ref(), Some(1), Some(1), Some(true))
            .expect("read should succeed");
        assert_eq!(read_output.file.content, Some("two".to_string()));
    }

    #[test]
    fn rejects_binary_files() {
        let path = temp_path("binary-test.bin");
        std::fs::write(&path, b"\x00\x01\x02\x03binary content").expect("write should succeed");
        let result = read_file(path.to_string_lossy().as_ref(), None, None, None);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("binary"));
    }

    #[test]
    fn rejects_oversized_writes() {
        let path = temp_path("oversize-write.txt");
        let huge = "x".repeat(MAX_WRITE_SIZE + 1);
        let result = new_file(path.to_string_lossy().as_ref(), &huge, false);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn globs_and_greps_directory() {
        let dir = temp_path("search-dir");
        std::fs::create_dir_all(&dir).expect("directory should be created");
        let file = dir.join("demo.rs");
        new_file(
            file.to_string_lossy().as_ref(),
            "fn main() {\n println!(\"hello\");\n}\n",
            false,
        )
        .expect("file write should succeed");

        let globbed = glob_search("**/*.rs", Some(dir.to_string_lossy().as_ref()))
            .expect("glob should succeed");
        assert_eq!(globbed.num_files, 1);

        let grep_output = grep_search(&GrepSearchInput {
            pattern: String::from("hello"),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: Some(String::from("**/*.rs")),
            output_mode: Some(String::from("content")),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: Some(true),
            case_insensitive: Some(false),
            file_type: None,
            head_limit: Some(10),
            offset: Some(0),
            multiline: Some(false),
        })
        .expect("grep should succeed");
        assert!(grep_output.content.unwrap_or_default().contains("hello"));
    }

    #[test]
    fn expand_braces_no_braces() {
        assert_eq!(expand_braces("*.rs"), vec!["*.rs"]);
    }

    #[test]
    fn expand_braces_single_group() {
        let mut result = expand_braces("Assets/**/*.{cs,uxml,uss}");
        result.sort();
        assert_eq!(
            result,
            vec!["Assets/**/*.cs", "Assets/**/*.uss", "Assets/**/*.uxml",]
        );
    }

    #[test]
    fn expand_braces_nested() {
        let mut result = expand_braces("src/{a,b}.{rs,toml}");
        result.sort();
        assert_eq!(
            result,
            vec!["src/a.rs", "src/a.toml", "src/b.rs", "src/b.toml"]
        );
    }

    #[test]
    fn expand_braces_unmatched() {
        assert_eq!(expand_braces("foo.{bar"), vec!["foo.{bar"]);
    }

    #[test]
    fn glob_search_with_braces_finds_files() {
        let dir = temp_path("glob-braces");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("b.toml"), "[package]").unwrap();
        std::fs::write(dir.join("c.txt"), "hello").unwrap();

        let result =
            glob_search("*.{rs,toml}", Some(dir.to_str().unwrap())).expect("glob should succeed");
        assert_eq!(
            result.num_files, 2,
            "should match .rs and .toml but not .txt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-only scripted prompter mirroring the one in
    /// `boundary::tests::ScriptedPrompter`. We keep a local copy so
    /// `file_ops` tests do not depend on `boundary::tests`.
    struct ScriptedPrompter {
        decisions: Mutex<VecDeque<Result<BoundaryDecision, PrompterError>>>,
    }

    impl ScriptedPrompter {
        fn new(decisions: Vec<BoundaryDecision>) -> Self {
            Self {
                decisions: Mutex::new(decisions.into_iter().map(Ok).collect()),
            }
        }
    }

    impl Prompter for ScriptedPrompter {
        fn ask(
            &self,
            _path: &std::path::Path,
            _workspace: &std::path::Path,
        ) -> Result<BoundaryDecision, PrompterError> {
            self.decisions
                .lock()
                .expect("scripted prompter mutex poisoned")
                .pop_front()
                .unwrap_or(Err(PrompterError::NoTty))
        }
    }

    fn outside_workspace_setup(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let workspace = temp_path(&format!("policy-ws-{label}"));
        let outside = temp_path(&format!("policy-out-{label}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&outside).expect("create outside");
        (workspace, outside)
    }

    #[test]
    fn read_file_with_policy_strict_denies_outside_workspace() {
        let (workspace, outside) = outside_workspace_setup("strict-read");
        let file = outside.join("data.txt");
        new_file(file.to_string_lossy().as_ref(), "secret", false).expect("write outside");
        let result = read_file_with_policy(
            file.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &WorkspacePolicy::Strict,
            None,
        );
        let err = result.expect_err("strict policy must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("escapes workspace"));
        assert!(err.to_string().contains("--workspace-policy=prompt"));
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn read_file_with_policy_allow_permits_outside_workspace() {
        let (workspace, outside) = outside_workspace_setup("allow-read");
        let file = outside.join("data.txt");
        new_file(file.to_string_lossy().as_ref(), "ok", false).expect("write outside");
        let result = read_file_with_policy(
            file.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &WorkspacePolicy::Allow,
            None,
        );
        // The read should succeed; the policy admitted the access.
        let payload = result.expect("allow policy must permit");
        // Checksum is set even when content is not echoed.
        assert!(!payload.file.checksum.is_empty());
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn read_file_with_policy_prompt_allow_once_returns_file() {
        let (workspace, outside) = outside_workspace_setup("prompt-once");
        let file = outside.join("data.txt");
        new_file(file.to_string_lossy().as_ref(), "once", false).expect("write outside");
        let prompter = Arc::new(ScriptedPrompter::new(vec![BoundaryDecision::AllowOnce]));
        let session = Arc::new(Mutex::new(BTreeSet::<crate::boundary::ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(Mutex::new(BTreeSet::<crate::boundary::ApprovedRoot>::new())),
        };
        let result = read_file_with_policy(
            file.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &policy,
            None,
        );
        let payload = result.expect("AllowOnce should admit the read");
        assert!(!payload.file.checksum.is_empty());
        assert!(session.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn read_file_with_policy_prompt_deny_blocks_with_user_facing_error() {
        let (workspace, outside) = outside_workspace_setup("prompt-deny");
        let file = outside.join("data.txt");
        new_file(file.to_string_lossy().as_ref(), "secret", false).expect("write outside");
        let prompter = Arc::new(ScriptedPrompter::new(vec![BoundaryDecision::Deny]));
        let session = Arc::new(Mutex::new(BTreeSet::<crate::boundary::ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(Mutex::new(BTreeSet::<crate::boundary::ApprovedRoot>::new())),
        };
        let result = read_file_with_policy(
            file.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &policy,
            None,
        );
        let err = result.expect_err("Deny must reject");
        assert!(err.to_string().contains("user denied access"));
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn read_file_with_policy_prompt_allow_session_skips_second_prompt() {
        let (workspace, outside) = outside_workspace_setup("prompt-sess");
        let file = outside.join("data.txt");
        new_file(file.to_string_lossy().as_ref(), "sess", false).expect("write outside");
        let prompter = Arc::new(ScriptedPrompter::new(vec![BoundaryDecision::AllowSession]));
        let session = Arc::new(Mutex::new(BTreeSet::<crate::boundary::ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(Mutex::new(BTreeSet::<crate::boundary::ApprovedRoot>::new())),
        };
        let _ = read_file_with_policy(
            file.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &policy,
            None,
        )
        .expect("first read should succeed");
        // The scripted prompter is now empty; a second read would
        // surface a `NoTty` error if it were invoked.
        let payload = read_file_with_policy(
            file.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &policy,
            None,
        )
        .expect("second read must not re-prompt");
        assert!(!payload.file.checksum.is_empty());
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn read_file_with_policy_in_workspace_skips_policy_check() {
        let (workspace, _outside) = outside_workspace_setup("in-ws");
        let inside = workspace.join("in.txt");
        new_file(inside.to_string_lossy().as_ref(), "inside", false).expect("write inside");
        // Even with Strict policy, an in-workspace path proceeds
        // without consulting the prompter.
        let prompter = Arc::new(ScriptedPrompter::new(vec![]));
        let session = Arc::new(Mutex::new(BTreeSet::<crate::boundary::ApprovedRoot>::new()));
        let policy = WorkspacePolicy::Prompt {
            prompter: prompter.clone(),
            session_approved: session.clone(),
            user_typed: Arc::new(Mutex::new(BTreeSet::<crate::boundary::ApprovedRoot>::new())),
        };
        let result = read_file_with_policy(
            inside.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &policy,
            None,
        );
        let payload = result.expect("in-workspace read should succeed");
        assert!(!payload.file.checksum.is_empty());
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn new_file_with_policy_strict_denies_outside_workspace() {
        let (workspace, outside) = outside_workspace_setup("write-strict");
        let target = outside.join("new.txt");
        let result = new_file_with_policy(
            target.to_string_lossy().as_ref(),
            "x",
            false,
            &workspace,
            &WorkspacePolicy::Strict,
        );
        let err = result.expect_err("strict policy must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("escapes workspace"));
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn new_file_with_policy_allow_writes_to_outside_workspace() {
        let (workspace, outside) = outside_workspace_setup("write-allow");
        let target = outside.join("new.txt");
        let result = new_file_with_policy(
            target.to_string_lossy().as_ref(),
            "ok",
            false,
            &workspace,
            &WorkspacePolicy::Allow,
        );
        let payload = result.expect("allow policy must permit write");
        assert!(target.exists(), "file should be created");
        let written = std::fs::read_to_string(&target).expect("read back");
        assert_eq!(written, "ok");
        assert!(payload.bytes_written > 0);
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn read_file_with_policy_respects_full_false_opt_out() {
        // Regression: `full: false` must propagate through the
        // policy wrapper. A prior version hardcoded `None` here,
        // which silently echoed content even when the caller asked
        // for a token-light payload.
        let workspace = temp_path("full-false-workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let inside = workspace.join("echo.txt");
        new_file(inside.to_string_lossy().as_ref(), "echo this", false).expect("write should succeed");
        let payload_tokenlight = read_file_with_policy(
            inside.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &WorkspacePolicy::Allow,
            Some(false),
        )
        .expect("token-light read should succeed");
        assert!(
            payload_tokenlight.file.content.is_none(),
            "full=false must suppress the content echo"
        );
        let payload_echo = read_file_with_policy(
            inside.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
            &WorkspacePolicy::Allow,
            None,
        )
        .expect("default read should succeed");
        assert_eq!(
            payload_echo
                .file
                .content
                .as_deref()
                .expect("content present by default"),
            "echo this"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn new_file_echoes_content_preview() {
        let path = temp_path("preview-write.txt");
        let payload = new_file(
            path.to_string_lossy().as_ref(),
            "alpha\nbeta\ngamma",
            false,
        )
        .expect("write should succeed");
        let preview = payload
            .content_preview
            .as_deref()
            .expect("content_preview must be populated by default");
        assert!(preview.contains("alpha"));
        assert!(preview.contains("beta"));
        assert!(preview.contains("gamma"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn new_file_truncates_oversized_content_preview() {
        let path = temp_path("preview-large.txt");
        let large = "a".repeat(8_000);
        let payload = new_file(path.to_string_lossy().as_ref(), &large, false)
            .expect("write should succeed");
        let preview = payload
            .content_preview
            .as_deref()
            .expect("content_preview must be populated by default");
        // Preview must be smaller than the original content and
        // include the truncation marker so the model knows it was clipped.
        assert!(preview.len() < large.len());
        assert!(preview.contains("[truncated"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn edit_file_echoes_content_preview_of_new_file() {
        let path = temp_path("preview-edit.txt");
        new_file(path.to_string_lossy().as_ref(), "first\nsecond\nthird", false)
            .expect("seed write should succeed");
        let payload = edit_file(
            path.to_string_lossy().as_ref(),
            "second",
            "SECOND-EDITED",
            false,
            None,
        )
        .expect("edit should succeed");
        let preview = payload
            .content_preview
            .as_deref()
            .expect("content_preview must be populated by default");
        // Preview must reflect the *post-edit* state so the model can
        // verify the change without re-reading the file.
        assert!(preview.contains("SECOND-EDITED"));
        assert!(!preview.contains("first\nsecond\nthird\nsecond"));
        // The full new content must still be on disk.
        let on_disk = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "first\nSECOND-EDITED\nthird");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preview_for_handles_empty_and_small_and_oversized() {
        assert_eq!(preview_for(""), None);
        assert_eq!(preview_for("hi"), Some("hi".to_owned()));
        let big = "x".repeat(5_000);
        let clipped = preview_for(&big).expect("non-empty");
        assert!(clipped.contains("[truncated"));
        assert!(clipped.len() < big.len());
    }
}
