# AGENTS.md

This file provides guidance to Qoder (qoder.com) when working with code in this repository.

## Project Overview

**claw-code** is a Windows-native AI coding assistant CLI written in Rust. The binary is named `claw`. It provides an interactive REPL that communicates with LLM providers (Anthropic, OpenAI-compatible) and exposes tools for file editing, bash execution, sub-agents, MCP servers, plugins, and slash commands.

## Build Commands

All builds require the MSVC toolchain. The workspace root for cargo is `rust/`.

### Prerequisites (every compile session)

Run in `cmd` (not PowerShell) before any cargo invocation:

```bat
"C:\Users\Incredible\openspace\.opencode\CompilePreSet.bat"
```

This loads VS2022 VsDevCmd, MSVC 14.44.35207, Windows Kits 10.0.26100.0, Clang-CL 22.1.2, NASM, Perl.

### Build

```bat
:: Option A — scripted (runs full env setup + cargo build --release)
rust\build.bat

:: Option B — manual in cmd after CompilePreSet.bat
cd rust
cargo build --release
```

Binary output: `rust\target\release\claw.exe`

### Lint

```bat
cd rust
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace lints: `clippy::all` + `clippy::pedantic` warn; `unsafe_code` denied. Several `allow` overrides are declared per-crate where structurally necessary (e.g. `main.rs` allows `dead_code`, `unused_imports`).

### Tests

```bat
cd rust
:: All workspace tests
cargo test --workspace

:: Single crate
cargo test -p runtime
cargo test -p tools
cargo test -p agents

:: Single test by name (substring match)
cargo test -p tools test_worker_create_with_cwd

:: Single test in a specific file (bin crate)
cargo test -p rusty-claude-cli --test cli_flags_and_config_defaults
```

Note: `rusty-claude-cli/src/main.rs` contains inline `#[cfg(test)] mod tests` but currently has a pre-existing compile error (missing `cached_tokens` field) that blocks those unit tests. Integration tests in `rusty-claude-cli/tests/` compile and run independently.

### Run

```bat
:: start.bat sets TARGET_DIR, env vars, then invokes the release binary
start.bat
```

## Workspace Crate Architecture

```
rusty-claude-cli  (binary "claw")
  ├─ tools         (tool execution façade)
  │    ├─ runtime  (core engine)
  │    │    ├─ plugin-types
  │    │    ├─ plugins
  │    │    └─ telemetry
  │    ├─ agents   (sub-agent lifecycle)
  │    ├─ commands (slash commands)
  │    └─ plugins
  ├─ api           (LLM HTTP client: Anthropic + OpenAI-compat, SSE, prompt cache)
  ├─ commands
  ├─ runtime
  └─ compat-harness  (upstream manifest extraction, bootstrap plan)

mock-anthropic-service  (test-only: mock provider for integration tests)
```

### Dependency direction

`rusty-claude-cli` → `tools` → `runtime` → `plugin-types` / `telemetry`
`tools` → `agents` → `runtime`
`commands` → `runtime`, `plugins`, `agents`

### Crate responsibilities

| Crate | Role |
|---|---|
| `rusty-claude-cli` | CLI entry point, REPL loop, terminal rendering (markdown + syntax highlighting), slash command dispatch, model selection, session resume. `main.rs` is ~14k lines and contains the entire interactive loop. |
| `runtime` | Core engine: session persistence (`session.rs`), conversation loop (`conversation.rs`), config loading (`config.rs`), permission evaluation (`permissions.rs`), bash execution (`bash.rs` + `bash_validation.rs`), file operations (`file_ops.rs`), MCP lifecycle (`mcp_stdio.rs`, `mcp_lifecycle_hardened.rs`), worker boot state machine (`worker_boot.rs`), hooks (`hooks.rs`), trust resolution, sandbox detection, prompt assembly. |
| `tools` | Unified tool execution façade. Wraps runtime file_ops, bash, agents, MCP tool bridge, LSP, grep/glob, web fetch, image handling. Owns `GlobalToolRegistry` and per-session tool definitions. |
| `api` | HTTP client for Anthropic Messages API and OpenAI-compatible providers. SSE stream parsing. Prompt cache management. Provider detection (`detect_provider_kind`). |
| `agents` | Sub-agent lifecycle: `Created → Running → {Completed, Failed}`. Crash-safe manifest writes (tmp + fsync + atomic rename). Agent discovery from `.claw/agents/` directories. |
| `commands` | Slash command registry (`/compact`, `/agents`, `/mcp`, `/plugins`, `/skills`, etc.). Command handlers delegate to runtime/tools subsystems. |
| `plugins` | Plugin loading (builtin, bundled, external), marketplace, Claude settings integration, frontmatter parsing. |
| `compat-harness` | Extracts upstream manifest from `.claude/` directory structure. Builds `BootstrapPlan` for session initialization. |
| `telemetry` | Session tracing, analytics events, JSONL telemetry sink. Re-exported through `api`. |

## Key Architectural Patterns

### Config discovery

`ConfigSource` enum: `User`, `Plugin`, `Project`, `Local`.
Config loader (`config.rs::discover()`) walks ancestors from cwd upward, stopping at `user_home_dir()`. Uses `Path::canonicalize()` on both cwd and home to defeat Windows 8.3 short-name aliasing. Falls back to textual path on canonicalize failure. `.local.json` files map to `ConfigSource::Local`.

### Permission system

`permissions.rs` implements rule-based permission evaluation for tool execution. `PermissionRuleMatcher::ToolNamePrefix` handles prefix-based tool name matching. F-04 pattern: silent prefix promotion (no warning emitted for well-defined prefix rules).

### Worker state machine

`worker_boot.rs` defines `WorkerStatus`: `Spawning → TrustRequired → ToolPermissionRequired → ReadyForPrompt → Running → {Finished, Failed}`. `WorkerRegistry` manages multiple concurrent workers with trust-gate detection and prompt-misdelivery recovery.

### MCP integration

Full lifecycle management in `mcp_lifecycle_hardened.rs` with degraded-mode reporting. Transport via stdio (`mcp_stdio.rs`) using JSON-RPC. Tool bridging (`mcp_tool_bridge.rs`) exposes MCP tools through `McpToolRegistry` consumed by the tools crate.

### Global registries (tools crate)

`tools/src/lib.rs` defines process-wide `OnceLock` singletons: `global_lsp_registry()`, `global_mcp_registry()`, `global_team_registry()`, `global_cron_registry()`, `global_task_registry()`. These are shared across tool invocations within a session.

### Test isolation

`runtime::test_env_lock()` provides a process-wide mutex for tests that mutate environment variables. Tests that create filesystem fixtures must use `std::env::temp_dir()` with unique subdirectories — never hardcoded `/tmp/X` or `/no/X` paths (which pollute `C:\tmp` and `C:\no` on Windows).

## Environment Variables (runtime)

Set by `start.bat` before launching `claw.exe`:

| Variable | Purpose |
|---|---|
| `TARGET_DIR` | Session storage: `.claw\sessions` |
| `ANTHROPIC_BASE_URL` | API endpoint override (local proxy) |
| `CLAW_WORKSPACE_POLICY` | `allow` — bypass workspace trust prompt |
| `CLAUDE_CODE_SHELL` | Shell path (used in startup scripts; hardcoded in binary) |
| `RUST_LOG` | `info` level logging |
| `DISABLE_TELEMETRY` | Disable telemetry |

## File Conventions

- Settings: `.claw/settings.json` (current), `.claw.json` (legacy, to be migrated)
- Agent manifests: `.claw/agents/agent-*.json` + `.md`
- Plugin manifests: `.claude-plugin/plugin.json`
- Session data: `.claw/sessions/`
- Skills: `.claw/skills/`
