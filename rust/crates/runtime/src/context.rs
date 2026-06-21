//! Context management for LLM API requests.
//!
//! This module filters session messages before sending to the LLM:
//! - Removes Thinking block content (preserves signature for API round-trip)
//! - Estimates token usage
//! - Truncates messages that exceed context window

use std::sync::OnceLock;

use crate::session::{ContentBlock, ConversationMessage, MessageRole};

/// Minimum output size (bytes) to trigger filtering in API context.
const TOOLRESULT_MIN_BYTES: usize = 500;

/// Number of recent messages whose ToolResult output is preserved verbatim.
/// Older messages have large ToolResults replaced with structured summaries.
const PRESERVE_RECENT_MESSAGES: usize = 6;

/// Tools whose output should be compressed in subsequent API rounds.
const FILTER_TOOLS: &[&str] = &[
    "WebFetch", "WebSearch", "read_file", "new_file", "edit_file", "bash", "grep_search",
];

/// Filters conversation messages for LLM API requests.
///
/// - Thinking blocks: content removed, signature preserved for API round-trip.
/// - Large ToolResult (WebFetch, read_file, new_file, edit_file, bash, grep_search):
///   output replaced with structured summary to avoid re-sending content that
///   the AI has already processed.
/// - Position-aware: the last [`PRESERVE_RECENT_MESSAGES`] messages keep their
///   full ToolResult output so the model retains access to recent context.
pub fn filter_for_api(messages: &[ConversationMessage]) -> Vec<ConversationMessage> {
    filter_for_api_inner(messages, PRESERVE_RECENT_MESSAGES)
}

/// Internal implementation with configurable preservation window.
fn filter_for_api_inner(
    messages: &[ConversationMessage],
    preserve_recent: usize,
) -> Vec<ConversationMessage> {
    let preserve_from = messages.len().saturating_sub(preserve_recent);
    messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            let is_recent = idx >= preserve_from;
            let filtered_blocks: Vec<ContentBlock> = msg
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Thinking { signature, .. } => {
                        // Strip thinking content, keep signature for API round-trip
                        ContentBlock::Thinking {
                            thinking: String::new(),
                            signature: signature.clone(),
                        }
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        tool_name,
                        output,
                        is_error,
                    } if !is_recent
                        && !is_error
                        && output.len() > TOOLRESULT_MIN_BYTES
                        && FILTER_TOOLS.contains(&tool_name.as_str()) =>
                    {
                        // Generate structured summary preserving key metadata.
                        let summary = summarize_tool_result(tool_name, output);
                        ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            tool_name: tool_name.clone(),
                            output: summary,
                            is_error: *is_error,
                        }
                    }
                    other => other.clone(),
                })
                .collect();

            // Drop messages that contain ONLY Thinking blocks.  After API
            // conversion strips Thinking blocks entirely (convert.rs), such
            // messages would produce an empty `content` array and get skipped,
            // causing `cached_message_values` to be shorter than
            // `request.messages`.  That index misalignment corrupts the
            // IncrementalBody per-message byte cache, producing duplicate
            // same-role messages that the Anthropic API rejects with
            // "Cannot have 2 or more assistant messages at the end of the
            // list" (or the equivalent user-role error).
            let has_non_thinking = filtered_blocks
                .iter()
                .any(|b| !matches!(b, ContentBlock::Thinking { .. }));
            if !has_non_thinking {
                return None;
            }

            Some(ConversationMessage {
                role: msg.role,
                blocks: filtered_blocks,
                usage: msg.usage.clone(),
                cached_tokens: msg.cached_tokens.clone(),
                cached_input_message: OnceLock::new(),
            })
        })
        .collect()
}

/// Generate a structured summary for a tool result, preserving key metadata
/// (file paths, exit codes, URLs) while dropping bulk content.
fn summarize_tool_result(tool_name: &str, output: &str) -> String {
    match tool_name {
        "read_file" => {
            let path = extract_json_str(output, "filePath").unwrap_or_default();
            let lines = extract_json_num(output, "numLines")
                .or_else(|| extract_json_num(output, "lineCount"))
                .unwrap_or_default();
            let bytes = extract_json_num(output, "bytesRead").unwrap_or_default();
            format!("[read_file: {path}, {lines} lines, {bytes} bytes \u{2014} content processed]")
        }
        "new_file" => {
            let path = extract_json_str(output, "filePath")
                .or_else(|| extract_json_str(output, "path"))
                .unwrap_or_default();
            let bytes = extract_json_num(output, "bytesWritten")
                .or_else(|| extract_json_num(output, "bytes"))
                .unwrap_or_default();
            format!("[new_file: {path}, {bytes} bytes written \u{2014} content processed]")
        }
        "edit_file" => {
            let path = extract_json_str(output, "filePath")
                .or_else(|| extract_json_str(output, "path"))
                .unwrap_or_default();
            let changed = extract_json_num(output, "linesChanged").unwrap_or_default();
            let diff = extract_json_str(output, "diffPath").unwrap_or_default();
            format!("[edit_file: {path}, {changed} lines changed, diff={diff} \u{2014} content processed]")
        }
        "bash" => {
            let exit = extract_json_num(output, "exitCode")
                .or_else(|| extract_json_num(output, "code"))
                .unwrap_or_default();
            // Keep first 200 chars of stdout for context
            let preview = extract_json_str(output, "stdout")
                .or_else(|| extract_json_str(output, "output"))
                .map(|s| {
                    if s.len() > 200 {
                        format!("{}...", &s[..200])
                    } else {
                        s
                    }
                })
                .unwrap_or_default();
            format!("[bash: exit={exit}, output: {preview}]")
        }
        "WebFetch" => {
            let url = extract_json_str(output, "url").unwrap_or_default();
            format!("[WebFetch: {url} \u{2014} content processed]")
        }
        "WebSearch" => {
            let query = extract_json_str(output, "query").unwrap_or_default();
            let provider = extract_json_str(output, "provider").unwrap_or_default();
            let returned = extract_json_num(output, "resultsReturned").unwrap_or_default();
            format!("[WebSearch: \"{query}\" via {provider}, {returned} results \u{2014} results reviewed]")
        }
        "grep_search" => {
            let files = extract_json_num(output, "num_files").unwrap_or_default();
            let lines = extract_json_num(output, "num_lines").unwrap_or_default();
            format!("[grep_search: {files} files, {lines} matches \u{2014} content processed]")
        }
        _ => {
            let chars = output.chars().count();
            format!("[{tool_name}: {chars} chars — content processed]")
        }
    }
}

/// Extract a string value from a JSON object by key.
fn extract_json_str(json: &str, key: &str) -> Option<String> {
    // Fast path: search for "key":"value" pattern without full JSON parse
    let pattern = format!("\"{key}\":");
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    if rest.starts_with('"') {
        let end = rest[1..].find('"')?;
        Some(rest[1..1 + end].to_string())
    } else {
        None
    }
}

/// Extract a numeric value from a JSON object by key.
fn extract_json_num(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":");
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    // Number or null
    if rest.starts_with("null") {
        return Some("null".to_string());
    }
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-' && c != '.')
        .unwrap_or(rest.len());
    if end > 0 {
        Some(rest[..end].to_string())
    } else {
        None
    }
}

/// Estimates token count for a single message.
/// Uses tiktoken `cl100k_base` when available, falls back to `chars/4`.
/// Delegates text-level estimation to [`crate::compact::estimate_text_tokens`]
/// for a single authoritative implementation across the crate.
pub fn estimate_message_tokens(message: &ConversationMessage) -> usize {
    // Use cached value if available (populated by compact.rs).
    if let Some(cached) = message.cached_tokens.get() {
        return *cached;
    }

    let mut tokens = 0;
    // Role overhead
    tokens += match message.role {
        MessageRole::System => 4,
        MessageRole::User => 4,
        MessageRole::Assistant => 3,
        MessageRole::Tool => 5,
    };

    for block in &message.blocks {
        tokens += match block {
            ContentBlock::Text { text } => crate::compact::estimate_text_tokens(text),
            ContentBlock::ToolUse { name, input, .. } => {
                let input_str = input.to_string();
                3 + crate::compact::estimate_text_tokens(name)
                    + crate::compact::estimate_text_tokens(&input_str)
            }
            ContentBlock::ToolResult {
                tool_name, output, ..
            } => {
                3 + crate::compact::estimate_text_tokens(tool_name)
                    + crate::compact::estimate_text_tokens(output)
            }
            ContentBlock::Image { data, .. } => {
                // Base64 is 33% inflated → decode bytes = len * 3/4.
                let bytes = data.len() * 3 / 4;
                bytes / 750 + 20
            }
            ContentBlock::ImageRef { .. } => 100,
            ContentBlock::Thinking { thinking, .. } => {
                crate::compact::estimate_text_tokens(thinking)
            }
        };
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn make_thinking_block(content: &str, sig: Option<&str>) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: content.to_string(),
            signature: sig.map(String::from),
        }
    }

    #[test]
    fn filter_removes_thinking_content() {
        let msg = ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "Hello".to_string(),
                },
                make_thinking_block("Long thinking content...", Some("sig123")),
            ],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        let filtered = filter_for_api(&[msg]);
        assert_eq!(filtered.len(), 1);

        // Thinking block should exist but with empty content
        let thinking_block = filtered[0]
            .blocks
            .iter()
            .find(|b| matches!(b, ContentBlock::Thinking { .. }));
        assert!(thinking_block.is_some());

        if let ContentBlock::Thinking { thinking, signature } = thinking_block.unwrap() {
            assert_eq!(thinking, ""); // Content stripped
            assert_eq!(signature, &Some("sig123".to_string())); // Signature preserved
        }
    }

    #[test]
    fn filter_preserves_non_thinking_blocks() {
        let msg = ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "Answer".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({}),
                },
            ],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        let filtered = filter_for_api(&[msg]);
        assert_eq!(filtered[0].blocks.len(), 2);
    }

    #[test]
    fn filter_replaces_large_toolresult_with_structured_summary() {
        let json_output = r#"{"filePath":"src/session.rs","lineCount":1200,"bytesRead":45000,"content":"use std::..."}"#;
        let long_output = format!("{json_output}{}", "X".repeat(1000));
        let msg = ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tu1".to_string(),
                tool_name: "read_file".to_string(),
                output: long_output,
                is_error: false,
            }],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        // Use preserve_recent=0 to test compression regardless of position
        let filtered = filter_for_api_inner(&[msg], 0);
        if let ContentBlock::ToolResult { output, .. } = &filtered[0].blocks[0] {
            assert!(output.starts_with("[read_file: src/session.rs"));
            assert!(output.contains("1200 lines"));
            assert!(output.contains("45000 bytes"));
            assert!(output.contains("content processed"));
        } else {
            panic!("Expected ToolResult");
        }
    }

    #[test]
    fn filter_replaces_bash_with_exit_code() {
        let json_output = format!(
            r#"{{"stdout":"test result: ok. 42 passed{}","exitCode":0}}"#,
            "X".repeat(600)
        );
        let msg = ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tu3".to_string(),
                tool_name: "bash".to_string(),
                output: json_output,
                is_error: false,
            }],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        // Use preserve_recent=0 to test compression regardless of position
        let filtered = filter_for_api_inner(&[msg], 0);
        if let ContentBlock::ToolResult { output, .. } = &filtered[0].blocks[0] {
            assert!(output.starts_with("[bash: exit=0"));
            assert!(output.contains("test result: ok"));
        } else {
            panic!("Expected ToolResult");
        }
    }

    #[test]
    fn filter_preserves_error_results() {
        let msg = ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tu2".to_string(),
                tool_name: "WebFetch".to_string(),
                output: "Connection refused".to_string(),
                is_error: true,
            }],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        let filtered = filter_for_api(&[msg]);
        if let ContentBlock::ToolResult { output, .. } = &filtered[0].blocks[0] {
            assert_eq!(output, "Connection refused");
        } else {
            panic!("Expected ToolResult");
        }
    }

    #[test]
    fn filter_preserves_recent_tool_results_verbatim() {
        let make_tool_msg = |id: &str, output: &str| ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                tool_name: "read_file".to_string(),
                output: output.to_string(),
                is_error: false,
            }],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        let big_output = format!(
            r#"{{"filePath":"big.rs","lineCount":500,"bytesRead":20000,"content":"{}"}}
"#,
            "X".repeat(1000)
        );

        // Create 8 messages: 2 old + 6 recent (within preserve window)
        let messages: Vec<ConversationMessage> = (0..8)
            .map(|i| make_tool_msg(&format!("tu{i}"), &big_output))
            .collect();

        let filtered = filter_for_api(&messages);

        // Old messages (index 0, 1) should be compressed
        if let ContentBlock::ToolResult { output, .. } = &filtered[0].blocks[0] {
            assert!(
                output.starts_with("[read_file:"),
                "old message should be compressed, got: {output}"
            );
        } else {
            panic!("Expected ToolResult at index 0");
        }

        // Recent messages (index 2-7) should preserve full output
        for i in 2..8 {
            if let ContentBlock::ToolResult { output, .. } = &filtered[i].blocks[0] {
                assert!(
                    output.contains("\"filePath\":\"big.rs\""),
                    "recent message {i} should be preserved verbatim, got: {output}"
                );
            } else {
                panic!("Expected ToolResult at index {i}");
            }
        }
    }

    #[test]
    fn filter_drops_thinking_only_messages() {
        // A message with ONLY a Thinking block should be dropped entirely,
        // because after API conversion strips Thinking blocks it would become
        // an empty message, causing cached_message_values index misalignment.
        let msg = ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![make_thinking_block("some reasoning...", Some("sig_abc"))],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        let filtered = filter_for_api(&[msg]);
        assert!(
            filtered.is_empty(),
            "thinking-only message should be dropped, got {} messages",
            filtered.len()
        );
    }

    #[test]
    fn filter_preserves_assistant_with_text_and_thinking() {
        // A message with Text + Thinking should be preserved (not dropped).
        let msg = ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "Hello".to_string(),
                },
                make_thinking_block("thinking...", Some("sig1")),
            ],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        let filtered = filter_for_api(&[msg]);
        assert_eq!(filtered.len(), 1, "text+thinking message should be kept");
    }

    #[test]
    fn filter_compresses_websearch_results() {
        // given
        let search_output = serde_json::json!({
            "query": "rust async runtime",
            "provider": "bing",
            "totalResults": 1234567,
            "resultsReturned": 10,
            "results": [
                {"title": "Tokio - An asynchronous Rust runtime", "link": "https://tokio.rs", "snippet": "Tokio is an asynchronous runtime for the Rust programming language that provides the building blocks needed for writing network applications.", "source": "tokio.rs", "date": "2024-01-15"},
                {"title": "Async programming in Rust", "link": "https://rust-lang.github.io/async-book/", "snippet": "This book aims to be a thorough guide to asynchronous programming in Rust, covering everything from basic concepts to advanced patterns.", "source": "rust-lang.github.io", "date": "2024-02-20"},
                {"title": "Understanding async/await", "link": "https://example.com/async-await", "snippet": "A deep dive into how async/await works under the hood in Rust, including the state machine transformation and Future trait.", "source": "example.com", "date": "2024-03-10"},
            ]
        }).to_string();

        // Ensure output exceeds TOOLRESULT_MIN_BYTES (500)
        assert!(search_output.len() > 500, "test data too small: {} bytes", search_output.len());

        let msg = ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_search".to_string(),
                tool_name: "WebSearch".to_string(),
                output: search_output,
                is_error: false,
            }],
            usage: None,
            cached_tokens: OnceLock::new(),
            cached_input_message: OnceLock::new(),
        };

        // when - message is old (not in recent window)
        let filtered = filter_for_api_inner(&[msg], 0);

        // then
        if let ContentBlock::ToolResult { output, .. } = &filtered[0].blocks[0] {
            assert!(output.starts_with("[WebSearch:"), "got: {output}");
            assert!(output.contains("rust async runtime"), "got: {output}");
            assert!(output.contains("bing"), "got: {output}");
            assert!(output.contains("10 results"), "got: {output}");
            assert!(output.len() < 200, "summary should be short, got {} chars", output.len());
        } else {
            panic!("Expected ToolResult");
        }
    }
}
