# Claw Code

> **Fork 说明：** 本项目基于 [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) 深度修改，针对 Windows 平台进行了全面适配和优化，已实现 Windows 上的稳定运行。

## 与原项目的差异

对比基准：[ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) `main` vs 本仓库 `main`（仅 `rust/` 目录）

| 指标 | 数值 |
|------|------|
| 变更文件总数 | **202** |
| 新增文件 | **53** |
| 删除文件 | **76** |
| 修改文件 | **72** |
| 新增行数 | **+27,509** |
| 删除行数 | **-47,844** |

### Crate 结构变化

| 变化 | Crate | 说明 |
|------|-------|------|
| 新增 | `agents` | Sub-agent 生命周期管理（spawn、manifest、状态机） |
| 新增 | `plugin-types` | 插件类型定义（config、lifecycle、MCP 接口） |
| 移除 | `claw-analog` | 原项目模拟服务（本项目不需要） |
| 移除 | `claw-rag-service` | 原项目 RAG 服务（本项目不需要） |

### 各 Crate 变更统计

| Crate | 变更文件数 | 主要改动 |
|-------|----------|----------|
| `runtime` | 50 | 核心引擎重构（session、bash、permissions、MCP、sandbox） |
| `rusty-claude-cli` | 20 | CLI 入口、终端渲染、交互优化、模型选择 |
| `agents` | 15 | 全新 crate：sub-agent spawn/discovery/persist |
| `api` | 15 | LLM HTTP 客户端、SSE 解析、Provider 路由 |
| `plugins` | 12 | 插件加载、marketplace、配置集成 |
| `commands` | 6 | slash commands 重构 |
| `tools` | 6 | 工具执行门面层调整 |
| `plugin-types` | 5 | 全新 crate：插件接口类型 |

### 关键改动

- **新增 Windows 原生支持** — `start.bat`、`start.sh`、MSVC/Clang-CL 编译环境集成
- **新增 `agents` crate** — sub-agent 生命周期管理（Created → Running → Completed/Failed）
- **新增 `plugin-types` crate** — 插件系统类型定义与接口规范
- **运行时重构** — session 持久化、权限系统、MCP 生命周期管理、沙箱检测
- **Mock 服务改为纯库** — 不生成独立 exe，仅供集成测试使用
- **Lint 策略调整** — `pedantic` 从 `allow` 提升为 `warn`，`unsafe_code` 从 `forbid` 改为 `deny`

Windows-native AI coding assistant CLI written in Rust. Binary name: `claw`.

Provides an interactive REPL that communicates with LLM providers (Anthropic, OpenAI-compatible) and exposes tools for file editing, bash execution, sub-agents, MCP servers, plugins, and slash commands.

![Claw Code Terminal](terminal.png)

## Features

- **Interactive REPL** — Markdown rendering + syntax highlighting in terminal
- **Multi-Provider** — Anthropic Messages API, OpenAI-compatible endpoints, local models (LM Studio)
- **Tool System** — File ops, bash execution, grep/glob, LSP, web fetch, image handling
- **Sub-Agents** — Spawn isolated agent workers with lifecycle management
- **MCP Integration** — Full lifecycle management for Model Context Protocol servers
- **Plugins** — Builtin, bundled, and external plugin loading with marketplace support
- **Slash Commands** — `/compact`, `/agents`, `/mcp`, `/plugins`, `/skills`, etc.
- **Permission System** — Rule-based tool execution permissions with sandbox support
- **Session Persistence** — Conversation history and session resume
- **Prompt Cache** — Anthropic prompt caching for efficient API usage

## Architecture

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
```

## Prerequisites

- **Rust** (latest stable)
- **MSVC toolchain** (VS2022 with C++ workload)
- **Clang-CL** (optional, for faster compilation)

## Build

### Option A — Scripted (recommended)

```bat
rust\build.bat
```

This loads the full MSVC environment and runs `cargo build --release`.

### Option B — Manual

```bat
:: Load MSVC environment first
"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat" -arch=x64

cd rust
cargo build --release
```

Binary output: `rust\target\release\claw.exe`

### Lint

```bat
cd rust
cargo clippy --workspace --all-targets -- -D warnings
```

### Tests

```bat
cd rust

# All workspace tests
cargo test --workspace

# Single crate
cargo test -p runtime
cargo test -p tools
cargo test -p agents

# Single test by name
cargo test -p tools test_worker_create_with_cwd
```

## Quick Start

### Anthropic API Mode

1. Copy `start.bat` and configure your API key:

```bat
set ANTHROPIC_API_KEY=sk-ant-your-real-key
set ANTHROPIC_BASE_URL=http://127.0.0.1:1234   :: optional proxy
```

2. Run:

```bat
start.bat
```

### Local Model Mode (LM Studio / Ollama)

```bat
run_local_openai.bat
```

Or configure manually:

```bat
set OPENAI_BASE_URL=http://127.0.0.1:1234
set OPENAI_API_KEY=dummy
set ANTHROPIC_MODEL=your-model-name
```

## Configuration

### Environment Variables (`~/.claw/.env`)

Claw auto-loads `~/.claw/.env` on startup. See [`.claw/env.example`](.claw/env.example) for full reference.

```env
# LLM Provider Keys
ANTHROPIC_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
DASHSCOPE_API_KEY=sk-...

# WebSearch (optional — falls back to Bing/Sogou scraping)
SEARCHAPI_API_KEY=your-key

# Network
HTTPS_PROXY=http://127.0.0.1:7890

# Runtime
RUST_LOG=info
DISABLE_TELEMETRY=1
```

### Settings Files (priority low → high)

| File | Level | Purpose |
|------|-------|---------|
| `~/.claw.json` | User (legacy) | Global defaults |
| `~/.claw/settings.json` | User | User-wide MCP/model/permissions |
| `$CWD/.claw.json` | Project | Project defaults |
| `$CWD/.claw/settings.json` | Project | Project core config |
| `$CWD/.claw/settings.local.json` | Local override | Not committed |

User and project settings **deep merge**, with project-level keys overriding user-level.

### Example `settings.json`

```json
{
  "model": "sonnet",
  "env": {
    "ANTHROPIC_API_KEY": "sk-ant-..."
  },
  "mcpServers": {
    "my-server": {
      "command": "uvx",
      "args": ["my-tool"]
    }
  },
  "permissions": {
    "defaultMode": "ask",
    "allow": ["Read", "Glob", "Grep"],
    "deny": ["Bash(rm -rf *)"]
  }
}
```

## Key Environment Variables

| Variable | Description | Example |
|----------|-------------|---------|
| `ANTHROPIC_API_KEY` | Anthropic API key | `sk-ant-...` |
| `OPENAI_API_KEY` | OpenAI-compatible provider | `sk-...` |
| `ANTHROPIC_BASE_URL` | API endpoint override | `http://127.0.0.1:1234` |
| `HTTPS_PROXY` | HTTP proxy | `http://127.0.0.1:7890` |
| `CLAW_WORKSPACE_POLICY` | Workspace trust policy | `allow` |
| `CLAUDE_CODE_SHELL` | Bash shell path | `C:\msys64\mingw64\bin\bash.exe` |
| `RUST_LOG` | Log level | `info` |
| `DISABLE_TELEMETRY` | Disable telemetry | `1` |

## File Conventions

```
.claude/              # Agent definitions, skills, plugins
.claude-plugin/       # Plugin manifests
.claw/                # Claw-specific config
  ├── settings.json   # Project settings
  ├── agents/         # Agent manifests
  ├── plugins/        # Installed plugins
  ├── sessions/       # Session data (gitignored)
  ├── skills/         # Skill definitions
  └── env.example     # Environment variable reference
.claw.json            # Legacy config (to be migrated)
AGENTS.md             # AI agent guidance
```

## License

MIT
