# Server Context Mechanism — Deep Architecture Analysis

> Generated: 2026-06-15  
> Scope: `session.messages` → HTTP POST body 全链路  
> Status: **深度分析报告**

---

## 1. 当前数据流全图

```
Session.messages (Vec<ConversationMessage>)
│
│ ① filter_for_api()  ──── 每条消息 clone 全部 blocks
│    ├─ Thinking: 保留 signature, thinking → ""  (clone signature)
│    ├─ ToolResult (old, >500B, filter tool): summarize → String::new allocation
│    └─ Other: block.clone()  ◄── 深度 clone 每个 String 字段
│
▼
api_messages: Arc<Vec<ConversationMessage>>  ◄── 仅此处 wrap Arc
│
│ ② run_turn() agentic loop  ──── 每轮迭代:
│    ├─ ApiRequest { system_prompt: Arc::clone, messages: Arc::clone }  O(1)
│    └─ Arc::make_mut(&mut api_messages).push(msg)  COW push
│
▼
ApiClient::stream(request: ApiRequest)
│
│ ③ convert_messages()  ──── 再次深度 clone 每条消息
│    ├─ Text { text } → InputContentBlock::Text { text: text.clone() }
│    ├─ ToolUse { id, name, input } → id.clone(), name.clone(),
│    │   serde_json::from_str(input) ◄── 重新 parse JSON string → Value
│    ├─ ToolResult → tool_use_id.clone(), output.clone()
│    ├─ Image → mime_type.clone(), data.clone()
│    ├─ ImageRef → cache lookup → base64_data.clone()
│    └─ Thinking → None (filtered)
│
▼
MessageRequest { messages: Vec<InputMessage>, system: Option<String>, ... }
│
│ ④ system_prompt.to_string()  ◄── Arc<str> → String 深拷贝 (每次请求!)
│
│ ⑤ render_anthropic_body()  ──── 完整序列化 + 后处理
│    ├─ serde_json::to_value(self)  ◄── 全量序列化 MessageRequest → Value
│    ├─ normalize_anthropic_image_blocks()  ◄── 遍历 messages 修改 Value 树
│    ├─ apply_system_prompt_cache_control()  ◄── system_str.to_owned() + 分裂
│    └─ apply_tools_cache_control()  ◄── 修改 tools 最后一个元素
│
▼
body: serde_json::Value
│
│ ⑥ strip_unsupported_beta_body_fields()  ◄── 修改 Value 树
│
│ ⑦ .json(&request_body)  ──── reqwest 内部再次 serde_json::to_vec()
│    ◄── 第二次序列化 Value → bytes 写入 HTTP body
│
▼
HTTP POST /v1/messages
```

### 关键分配热点 (每轮 API 请求)

| 阶段 | 操作 | 分配类型 | 估算开销 |
|------|------|----------|----------|
| ① filter_for_api | 全量 clone 所有 ConversationMessage | N × String clone | O(全部消息) |
| ③ convert_messages | 全量 clone + JSON re-parse | N × String clone + serde parse | O(全部消息) |
| ④ system_prompt | Arc<str> → String | 一次完整 system prompt 拷贝 | ~2-8 KB |
| ⑤ to_value | MessageRequest → Value | 全量 JSON 树分配 | O(全部消息 + tools) |
| ⑤ post-processing | Value 树遍历修改 | 额外 String 分配 | O(system + tools) |
| ⑦ .json() | Value → bytes | 第二次全量序列化 | O(完整 body) |

---

## 2. 残留问题分析

### 2.1 convert_messages() — 仍是全量深拷贝

**位置**: `rusty-claude-cli/src/main.rs:9668` 和 `agents/src/runtime.rs:452`

两处 `convert_messages()` 实现完全相同，对每条消息的每个 String 字段执行 `.clone()`：

```rust
// 每个 Text block → text.clone()
ContentBlock::Text { text } => Some(InputContentBlock::Text { text: text.clone() })

// 每个 ToolUse → id.clone() + name.clone() + serde_json::from_str(input)
ContentBlock::ToolUse { id, name, input } => Some(InputContentBlock::ToolUse {
    id: id.clone(),
    name: name.clone(),
    input: serde_json::from_str(input).unwrap_or_else(|_| json!({ "raw": input })),
})
```

**问题**:
1. `ToolUse.input` 存储为 `String`，但每次转换都执行 `serde_json::from_str()` 重新解析为 `Value`。
2. 在 agentic loop 中，同一条消息可能被 convert 多次（每次 tool iteration 都重新 convert 全量）。
3. 两份完全相同的实现（CLI 和 agents）违反 DRY 原则。

**影响量化**: 对于 100 条消息的会话，每轮 iteration 产生 ~100 次 String clone + ~50 次 JSON parse。

### 2.2 filter_for_api() — 全量 clone 后立刻丢弃 Thinking 内容

**位置**: `runtime/src/context.rs:30`

`filter_for_api` 为每条消息创建新的 `Vec<ContentBlock>`，其中：
- `other => other.clone()` 对所有非 Thinking、非大 ToolResult 的 block 执行深拷贝
- `ContentBlock::Thinking { signature: signature.clone() }` 拷贝 signature string

**冗余**: `Arc<Vec<ConversationMessage>>` 包装后仅在第一次创建时有意义。后续 agentic loop 中 `Arc::make_mut` 只 push 新消息，已有消息不再变动。但 `filter_for_api` 在每次 `run_turn()` 开始时**重新执行全量 clone**。

### 2.3 render_anthropic_body() — 全量序列化 + 三次后处理

**位置**: `api/src/types.rs:63`

```rust
pub fn render_anthropic_body(&self) -> Result<Value, serde_json::Error> {
    let mut body = serde_json::to_value(self)?;          // 第一次全量序列化
    Self::normalize_anthropic_image_blocks(&mut body);    // 遍历修改
    Self::apply_system_prompt_cache_control(&mut body);   // system_str.to_owned() + 重建
    Self::apply_tools_cache_control(&mut body);           // 修改 tools
    Ok(body)
}
```

**问题**:
1. `serde_json::to_value(self)` 将整个 `MessageRequest` 序列化为 `Value` 树 — 这是最大的单点开销。
2. `apply_system_prompt_cache_control` 先 `system_str.to_owned()` 拷贝整个 system prompt，再 `find(boundary)` 分割，创建新的 `Value::Array`。
3. `reqwest::RequestBuilder::json(&body)` 会**再次** `serde_json::to_vec()` 序列化整个 `Value` 树。即：**同一数据被序列化两次**。

### 2.4 system_prompt 的 Arc<str> → String 泄漏

**位置**: `main.rs:8009` 和 `agents/runtime.rs:111`

```rust
system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.to_string()),
```

`Arc<str>` 的 O(1) clone 优势在 `convert_messages` 之后的 `MessageRequest` 构造处被 `to_string()` 打破。`MessageRequest.system` 是 `Option<String>`，不是 `Option<Arc<str>>`。

### 2.5 PromptCache 的隐式序列化

**位置**: `api/src/prompt_cache.rs:311-318`

```rust
fn from_request(request: &MessageRequest) -> Self {
    Self {
        model: hash_serializable(&request.model),
        system: hash_serializable(&request.system),
        tools: hash_serializable(&request.tools),
        messages: hash_serializable(&request.messages),  // ← 序列化全部消息为 JSON 再 hash
    }
}
```

每次请求的 hash 计算需要 `serde_json::to_vec()` 序列化全部 messages，这是**第三次**全量序列化。

### 2.6 重复实现

| 函数 | CLI (main.rs) | Agents (runtime.rs) | 差异 |
|------|--------------|---------------------|------|
| `convert_messages` | L9668-9739 | L452-524 | 逻辑完全相同 |
| `push_output_block` | (内联在 consume_stream) | L301-345 | agents 版本独立，CLI 内联 |
| `flush_thinking_block` | (内联在 consume_stream) | L385-401 | 相同逻辑 |

---

## 3. Context Window Management 机制

### 3.1 三层过滤管线

```
Session.messages (原始)
     │
     ├── filter_for_api()          ← Layer 1: 内容过滤
     │    ├─ Thinking 内容清零
     │    └─ 旧 ToolResult 压缩为摘要 (>500B, 6 条前)
     │
     ├── preflight check           ← Layer 2: 尺寸守卫
     │    ├─ 本地 byte 估算 (preflight_message_request)
     │    └─ 远程 count_tokens API (best-effort)
     │
     └── maybe_auto_compact()      ← Layer 3: 自动压缩
          ├─ 触发: input_tokens > 100K (默认)
          ├─ 反抖动: last_savings_ratio < 10% → 跳过
          └─ compact_session() → 摘要 + 保留尾部
```

### 3.2 compact_session() 的三维保留策略

```rust
let from_token_budget = find_token_tail_start(post_prefix, preserve_recent_tokens);  // 2000 tokens
let from_message_min = messages.len() - preserve_recent_messages;                    // 4 messages
let from_turn_budget = find_turn_tail_start(post_prefix, preserve_last_n_turns);     // 0 turns (default)
let raw_keep_from = min(token_absolute, turn_absolute, message_min);
// + tool-use/tool-result boundary walkback
```

### 3.3 filter_for_api vs compact_session 交互

- `filter_for_api` 在 `run_turn` 开始时调用一次，产生 `api_messages`
- `maybe_auto_compact` 在 `run_turn` **结束时**调用，直接修改 `self.session`
- **问题**: auto_compact 修改 session 后，下一轮 `run_turn` 的 `filter_for_api` 会基于压缩后的 session 重新构建。当前 turn 中的 `api_messages` 不会感知到 compaction。
- 这意味着 compaction 只在**下一轮用户输入**时生效，当前轮次的 agentic loop 不受影响。

### 3.4 没有主动裁剪机制

- 如果单条消息特别大（例如 `bash` 输出了 500KB），但位于最近 6 条消息内，`filter_for_api` 会原样保留。
- `preflight_message_request` 会拦截超限请求，但此时已是请求发送前最后一步 — 用户已经等待了。
- **缺失**: 没有在 agentic loop 中**渐进式**裁剪的机制。

---

## 4. Prompt Cache 集成分析

### 4.1 PromptCache 的实际功能

`PromptCache` **不是**请求级别的缓存（不缓存 HTTP 请求），而是：

1. **Completion Cache**: 对 `send_message`（非流式）缓存 `MessageResponse`，TTL 30s
2. **Cache Break Detection**: 追踪 `cache_read_input_tokens` 的变化，检测 Anthropic server-side prompt cache 是否失效
3. **统计追踪**: 记录 creation/read tokens 的累计值

### 4.2 PromptCache 是否修改请求？

**不修改**。`PromptCache` 是一个**被动观察者**：
- `lookup_completion()` — 仅在非流式路径使用，流式路径跳过
- `record_response()` / `record_usage()` — 记录统计数据
- `apply_system_prompt_cache_control()` 和 `apply_tools_cache_control()` 在 `types.rs` 中独立实现，不依赖 `PromptCache`

### 4.3 cache_control 标注路径

```
MessageRequest.system: Option<String>
    │
    ▼ serde_json::to_value()
body["system"]: String
    │
    ▼ apply_system_prompt_cache_control()
body["system"]: [
    { "type": "text", "text": "static...", "cache_control": { "type": "ephemeral" } },
    { "type": "text", "text": "dynamic..." }
]
```

`SYSTEM_PROMPT_DYNAMIC_BOUNDARY` 标记将 system prompt 分为静态/动态两部分。静态部分添加 `cache_control: ephemeral`，由 Anthropic 服务端缓存。

### 4.4 工具定义的 cache_control

```rust
fn apply_tools_cache_control(body: &mut Value) {
    // 仅对 tools 数组的最后一个元素添加 cache_control
    if let Some(last_tool) = tools.last_mut() {
        obj.insert("cache_control", json!({ "type": "ephemeral" }));
    }
}
```

---

## 5. Sub-agent Context 分析

### 5.1 ProviderRuntimeClient::stream()

**位置**: `agents/src/runtime.rs:98-148`

```rust
impl ApiClient for ProviderRuntimeClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let messages = convert_messages(&request.messages, image_cache, image_store);
        let system = (!request.system_prompt.is_empty()).then(|| request.system_prompt.to_string());
        // ...
        for entry in chain {
            let message_request = MessageRequest {
                messages: messages.clone(),   // ← 每个 provider 又 clone 一次
                system: system.clone(),       // ← 每个 provider 又 clone 一次
                tools: (!tools.is_empty()).then(|| tools.clone()),
                // ...
            };
        }
    }
}
```

### 5.2 Sub-agent 与 Main Client 的差异

| 维度 | Main Client (CLI) | Sub-agent (agents) |
|------|-------------------|-------------------|
| `convert_messages` | main.rs:9668 | runtime.rs:452 |
| Provider fallback | 单 provider | ProviderChain 遍历 |
| `messages.clone()` per attempt | 否 (仅一次) | 是 (每个 fallback provider) |
| `tools.clone()` per attempt | 否 | 是 |
| System prompt cache_control | 通过 render_anthropic_body | 取决于 provider variant |
| PromptCache 实例 | 有 | 无 |

**问题**: Sub-agent 在 provider fallback 场景下，每个 provider attempt 都 clone 整个 `messages` 和 `tools`。

---

## 6. 增量序列化方案 (IncrementalContextBuilder)

### 6.1 核心思路

当前每轮 agentic loop 迭代都执行：
1. `convert_messages` → 全量深拷贝
2. `serde_json::to_value` → 全量序列化
3. `reqwest .json()` → 第二次全量序列化

**优化目标**: 维护一个 "base JSON body"，只序列化**增量部分**。

### 6.2 设计方案

```
┌─────────────────────────────────────────────┐
│           IncrementalContextBuilder          │
├─────────────────────────────────────────────┤
│ base_body: Value                             │  ← 包含 model, max_tokens, tools, system
│ cached_messages_json: Vec<Value>             │  ← 每条消息的已序列化 Value
│ session_hash: u64                            │  ← 用于检测 base 是否需要重建
│ messages_offset: usize                       │  ← 已缓存的消息数量
├─────────────────────────────────────────────┤
│ build_request(messages: &[ConvMsg]) -> Value │
│   1. 若 base 无效 → 重建 base_body           │
│   2. 对 messages[offset..] → 序列化并缓存     │
│   3. 拼装最终 body = base + all messages      │
│ invalidate_base()                            │
│ invalidate_from(index: usize)                │
└─────────────────────────────────────────────┘
```

### 6.3 关键约束

1. **JSON 结构**: Anthropic API body 是扁平 object，`messages` 是数组。增量追加天然可行。
2. **cache_control 后处理**: `apply_system_prompt_cache_control` 只影响 `system` 字段，与 `messages` 无关。可以只对 base 执行一次。
3. **Image normalization**: 只影响包含 Image block 的消息。可以在单条消息级别处理。
4. **strip_unsupported_beta_body_fields**: 影响顶层字段，可以在 base 构建时执行。

### 6.4 接口契约 (CCP-style)

```rust
/// 构建并维护增量序列化的 API 请求 body。
///
/// 不变式:
///   - base_body 包含 model, max_tokens, system, tools, tool_choice, stream
///   - cached_messages_json[i] 是 messages[i] 的 JSON Value 表示
///   - build() 的输出等价于 serde_json::to_value(MessageRequest{...})
///     + post-processing，但只在 delta 部分执行序列化
trait IncrementalBody {
    /// 首次构建或 base 失效时调用。序列化 system + tools + 元数据。
    fn rebuild_base(&mut self, system: &str, tools: &[ToolDefinition], model: &str, max_tokens: u32);

    /// 追加一条已过滤的消息。仅序列化该消息。
    fn append_message(&mut self, msg: &ConversationMessage, image_cache: Option<&HashMap<String, String>>);

    /// 回滚到指定 offset (用于 compaction 后重建)。
    fn truncate_to(&mut self, offset: usize);

    /// 组装最终请求 body。将 base_body 与 cached_messages_json 合并。
    /// 返回的 Value 可以直接传给 reqwest::json()。
    fn build(&self) -> Value;

    /// 标记 base 需要重建 (tools 或 system 变更)。
    fn invalidate_base(&mut self);
}
```

### 6.5 消息级序列化缓存

```rust
/// 单条消息的序列化缓存。
///
/// 契约:
///   - value 是该消息的完整 JSON Value (含 role + content)
///   - hash 是消息内容的 FNV hash，用于验证缓存有效性
///   - 当消息内容不变时，value 可以安全复用
struct CachedMessageJson {
    value: Value,
    content_hash: u64,
}
```

### 6.6 性能预估

| 场景 | 当前开销 | 增量方案开销 | 加速比 |
|------|----------|------------|--------|
| 100 条消息，第 5 轮 iteration | 100 msg clone + serialize | 2 msg serialize (delta) | ~50x |
| system prompt 不变 | 每次 to_owned() + to_value | 0 (base 复用) | ∞ |
| tools 不变 | 每次 to_value | 0 (base 复用) | ∞ |
| compaction 后 | 全量重建 | truncate_to + 重建 base | ~2x |

---

## 7. 构建序列 (Implementation Priority)

### Phase 1: 消除重复实现 (低风险, 高收益)

1. **提取 `convert_messages` 到 `runtime` crate**
   - 当前位置: `main.rs` 和 `agents/runtime.rs` 各一份
   - 目标: `runtime/src/convert.rs` 或 `api/src/convert.rs`
   - 依赖: 无

2. **引入 `Arc<str>` 到 `MessageRequest.system`**
   - 将 `system: Option<String>` 改为 `system: Option<Arc<str>>`
   - 消除 `system_prompt.to_string()` 深拷贝
   - 依赖: `MessageRequest` 的 Serialize impl 需要适配

### Phase 2: 增量消息转换 (中风险, 高收益)

3. **`convert_messages` 增量转换**
   - 维护 `Vec<CachedInputMessage>` 缓存
   - 新增消息只 convert delta 部分
   - 在 `run_turn` 的 agentic loop 中复用
   - 依赖: Phase 1 完成

4. **`filter_for_api` 增量过滤**
   - 对已过滤的消息维护缓存
   - 新消息只过滤 delta
   - 依赖: Phase 1 完成

### Phase 3: 增量序列化 (高风险, 极高收益)

5. **`IncrementalContextBuilder` 实现**
   - 维护 base_body + cached_messages_json
   - 在 `AnthropicRuntimeClient` 中集成
   - 依赖: Phase 2 完成

6. **消除双重序列化**
   - 将 `render_anthropic_body()` 的输出直接作为 `Vec<u8>` 而非 `Value`
   - 或直接构建 JSON bytes 而非 `Value` 树
   - 依赖: Phase 3 step 5 完成

### Phase 4: 高级优化 (高风险, 需要验证)

7. **消息级 JSON 缓存与 hash**
   - 在 `ConversationMessage` 上添加 `cached_json: OnceLock<Value>`
   - 依赖: 需要验证内存开销

8. **streaming JSON writer**
   - 用 `serde_json::Serializer` 直接写入 `Vec<u8>` buffer
   - 避免中间 `Value` 树分配
   - 依赖: Phase 3 完成

---

## 8. 风险评估

### 8.1 Phase 1 风险

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| `Arc<str>` Serialize 不兼容 | 低 | serde 已支持 Arc<str> | 添加 unit test |
| 提取 `convert_messages` 引入循环依赖 | 中 | runtime 不依赖 api | 放在 api crate |

### 8.2 Phase 2 风险

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| 增量转换遗漏消息变更 | 中 | 发送错误上下文给 LLM | 内容 hash 校验 |
| filter_for_api 位置语义变化 | 低 | 旧消息未被正确压缩 | PRESERVE_RECENT_MESSAGES 基于绝对索引 |

### 8.3 Phase 3 风险

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| JSON 结构假设被破坏 | 中 | Anthropic API 400 | 回归测试 + schema 验证 |
| cache_control 后处理顺序依赖 | 低 | 缓存失效 | 后处理只在 base 上执行 |
| Image block normalization 与增量不兼容 | 中 | 图片发送失败 | Image 消息总是重新序列化 |
| reqwest `.json()` 的 Value 输入要求 | 低 | 编译失败 | 可用 `.body(bytes)` 替代 |

### 8.4 Phase 4 风险

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| OnceLock<Value> 增加内存 footprint | 高 | 100 条消息 × ~2KB JSON = ~200KB | 可接受 |
| streaming JSON writer 引入复杂性 | 高 | 维护成本增加 | 仅在 Phase 3 验证后实施 |

---

## 附录: 关键数据结构速查

```
Session.messages: Vec<ConversationMessage>
  ├─ role: MessageRole (System | User | Assistant | Tool)
  ├─ blocks: Vec<ContentBlock>
  │    ├─ Text { text: String }
  │    ├─ ToolUse { id: String, name: String, input: String }  ← input 是 JSON string
  │    ├─ ToolResult { tool_use_id: String, tool_name: String, output: String, is_error: bool }
  │    ├─ Image { mime_type: String, data: String, filename: Option<String> }
  │    ├─ ImageRef { mime_type: String, hash_hex: String, filename: Option<String> }
  │    └─ Thinking { thinking: String, signature: Option<String> }
  ├─ usage: Option<TokenUsage>
  └─ cached_tokens: OnceLock<usize>

ApiRequest (runtime crate):
  ├─ system_prompt: Arc<str>
  ├─ messages: Arc<Vec<ConversationMessage>>
  ├─ image_cache: Option<Arc<Mutex<HashMap<String, String>>>>
  └─ image_store: Option<ImageStore>

MessageRequest (api crate):
  ├─ model: String
  ├─ max_tokens: u32
  ├─ messages: Vec<InputMessage>
  │    ├─ role: String
  │    └─ content: Vec<InputContentBlock>
  │         ├─ Text { text: String }
  │         ├─ ToolUse { id: String, name: String, input: Value }  ← 注意: Value, 不是 String
  │         ├─ ToolResult { tool_use_id: String, content: Vec<ToolResultContentBlock>, is_error: bool }
  │         └─ Image { media_type: String, data: String }
  ├─ system: Option<String>
  ├─ tools: Option<Vec<ToolDefinition>>
  ├─ tool_choice: Option<ToolChoice>
  ├─ stream: bool
  ├─ temperature/top_p/frequency_penalty/presence_penalty/stop: Option<...>
  ├─ reasoning_effort: Option<String>
  └─ thinking: Option<ThinkingConfig>
```

---

## 总结

当前架构的核心瓶颈是 **每轮 agentic loop 迭代都执行全量深拷贝 + 全量序列化**。`Arc<str>` 和 `Arc<Vec>` 优化解决了 `ApiRequest` 的 clone 开销，但 `convert_messages` → `MessageRequest` → `render_anthropic_body` → `reqwest .json()` 链路中仍存在 **3 次全量序列化** 和 **N 次 String clone**。

增量序列化方案可以将 agentic loop 中的 per-iteration 开销从 O(全部消息) 降至 O(delta 消息)，在典型场景下预期提升 10-50x。
