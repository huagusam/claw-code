use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use api::{
    convert_messages_cached, convert_messages_inner, max_tokens_for_model, resolve_model_alias,
    ApiError, ContentBlockDelta, InputMessage, MessageRequest, MessageResponse,
    OutputContentBlock, ProviderClient, StreamEvent as ApiStreamEvent, ToolChoice,
    ToolDefinition,
};
use runtime::{
    extract_embedded_tools, load_system_prompt,
    ApiClient, ApiRequest, AssistantEvent, ConfigLoader, ConversationRuntime,
    PermissionMode, PermissionPolicy, ProviderFallbackConfig,
    RuntimeError, Session, ThinkParser, ToolError, ToolExecutor,
};
use serde_json::Value;

use crate::types::AgentJob;

// Global hook for the tools crate to register its real tool executor.
static GLOBAL_TOOL_EXECUTOR: OnceLock<
    Box<dyn Fn(&str, &Value, Option<&PermissionPolicy>) -> Result<String, String> + Send + Sync>,
> = OnceLock::new();

pub fn register_tool_executor(
    f: Box<
        dyn Fn(&str, &Value, Option<&PermissionPolicy>) -> Result<String, String>
            + Send
            + Sync,
    >,
) -> Result<(), String> {
    GLOBAL_TOOL_EXECUTOR
        .set(f)
        .map_err(|_| String::from("tool executor already registered"))
}

struct ProviderEntry {
    model: String,
    client: ProviderClient,
}

/// Tracks `Arc` pointer identity across consecutive `ApiClient::stream()` calls
/// to detect when messages are merely appended (not rebuilt) so we can skip
/// re-converting the full message list.
struct MessageCache {
    /// `Arc::as_ptr` value of the last seen `ApiRequest.messages`.
    last_ptr: usize,
    /// Number of messages from the start that we've already converted.
    last_len: usize,
    /// Accumulated converted `InputMessage`s.
    input_messages: Arc<Vec<InputMessage>>,
    /// Accumulated cached JSON `Value`s for `IncrementalBody`.
    cached_values: Arc<Vec<Option<Value>>>,
}

pub struct ProviderRuntimeClient {
    runtime: tokio::runtime::Runtime,
    chain: Vec<ProviderEntry>,
    allowed_tools: BTreeSet<String>,
    message_cache: Option<MessageCache>,
}

impl ProviderRuntimeClient {
    pub fn new(model: String, allowed_tools: BTreeSet<String>) -> Result<Self, String> {
        let fallback_config = load_provider_fallback_config();
        Self::new_with_fallback_config(model, allowed_tools, &fallback_config)
    }

    pub fn new_with_fallback_config(
        model: String,
        allowed_tools: BTreeSet<String>,
        fallback_config: &ProviderFallbackConfig,
    ) -> Result<Self, String> {
        let primary_model = fallback_config.primary().map_or(model, str::to_string);
        let primary = build_provider_entry(&primary_model)?;
        let mut chain = vec![primary];
        for fallback_model in fallback_config.fallbacks() {
            match build_provider_entry(fallback_model) {
                Ok(entry) => chain.push(entry),
                Err(error) => {
                    eprintln!(
                        "warning: skipping unavailable fallback provider {fallback_model}: {error}"
                    );
                }
            }
        }
        Ok(Self {
            runtime: tokio::runtime::Runtime::new().map_err(|error| error.to_string())?,
            chain,
            allowed_tools,
            message_cache: None,
        })
    }
}

fn build_provider_entry(model: &str) -> Result<ProviderEntry, String> {
    let resolved = resolve_model_alias(model).clone();
    let client = ProviderClient::from_model(&resolved)
        .map_err(|error| error.to_string())?
        .with_incremental_body();
    Ok(ProviderEntry {
        model: resolved,
        client,
    })
}

fn load_provider_fallback_config() -> ProviderFallbackConfig {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| ConfigLoader::default_for(cwd).load().ok())
        .map_or_else(ProviderFallbackConfig::default, |config| {
            config.provider_fallbacks().clone()
        })
}

impl ApiClient for ProviderRuntimeClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let tools = tool_specs_for_allowed_tools(Some(&self.allowed_tools))
            .into_iter()
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema.clone(),
            })
            .collect::<Vec<_>>();

        let (messages, cached_values) = {
            let msg_ptr = Arc::as_ptr(&request.messages) as usize;
            let msg_len = request.messages.len();

            // Phase 2: incremental conversion cache.
            // Same Arc allocation with only appended messages = reuse cached conversion.
            if let Some(cache) = &self.message_cache {
                if cache.last_ptr == msg_ptr && cache.last_len <= msg_len {
                    // Cache hit — convert only delta messages, extend cache in place.
                    let cache = self.message_cache.as_mut().unwrap();
                    if msg_len > cache.last_len {
                        let (delta_inputs, delta_cached) = convert_messages_inner(
                            &request.messages[cache.last_len..],
                            None,
                            None,
                        );
                        Arc::make_mut(&mut cache.input_messages).extend(delta_inputs);
                        Arc::make_mut(&mut cache.cached_values).extend(delta_cached);
                        cache.last_len = msg_len;
                    }
                    let messages = Arc::clone(&cache.input_messages);
                    let cached_values = Arc::clone(&cache.cached_values);
                    (messages, cached_values)
                } else {
                    // Cache miss — full conversion, populate cache.
                    full_convert_and_cache(
                        &mut self.message_cache,
                        &request,
                        msg_ptr,
                        msg_len,
                    )
                }
            } else {
                // No cache — full conversion, populate cache.
                full_convert_and_cache(
                    &mut self.message_cache,
                    &request,
                    msg_ptr,
                    msg_len,
                )
            }
        };

        let system =
            (!request.system_prompt.is_empty()).then(|| Arc::clone(&request.system_prompt));
        let tool_choice = (!self.allowed_tools.is_empty()).then_some(ToolChoice::Auto);

        let runtime = &self.runtime;
        let chain = &self.chain;
        let mut last_error: Option<ApiError> = None;
        for (index, entry) in chain.iter().enumerate() {
            let message_request = MessageRequest {
                model: entry.model.clone(),
                max_tokens: max_tokens_for_model(&entry.model),
                messages: messages.clone(),
                system: system.clone(),
                tools: (!tools.is_empty()).then(|| tools.clone()),
                tool_choice: tool_choice.clone(),
                stream: true,
                cached_message_values: Arc::clone(&cached_values),
                ..Default::default()
            };

            let attempt = runtime.block_on(stream_with_provider(&entry.client, &message_request));
            match attempt {
                Ok(events) => return Ok(events),
                Err(error) if error.is_retryable() && index + 1 < chain.len() => {
                    eprintln!(
                        "provider {} failed with retryable error, falling back: {error}",
                        entry.model
                    );
                    last_error = Some(error);
                }
                Err(error) => return Err(RuntimeError::new(error.to_string())),
            }
        }

        Err(RuntimeError::new(last_error.map_or_else(
            || String::from("provider chain exhausted with no attempts"),
            |error| error.to_string(),
        )))
    }
}

/// Full conversion pass that populates the message cache.
fn full_convert_and_cache(
    cache: &mut Option<MessageCache>,
    request: &ApiRequest,
    msg_ptr: usize,
    msg_len: usize,
) -> (Arc<Vec<InputMessage>>, Arc<Vec<Option<Value>>>) {
    let image_cache = request.image_cache.as_ref().map(|arc| arc.lock().unwrap());
    let image_store = request.image_store.as_ref();
    let (msgs_arc, vals) = convert_messages_cached(
        &request.messages,
        image_cache.as_deref(),
        image_store,
    );

    let vals_arc = Arc::new(vals);

    *cache = Some(MessageCache {
        last_ptr: msg_ptr,
        last_len: msg_len,
        input_messages: Arc::clone(&msgs_arc),
        cached_values: Arc::clone(&vals_arc),
    });

    (msgs_arc, vals_arc)
}

async fn stream_with_provider(
    client: &ProviderClient,
    message_request: &MessageRequest,
) -> Result<Vec<AssistantEvent>, ApiError> {
    let mut stream = client.stream_message(message_request).await?;
    let mut events = Vec::new();
    let mut pending_tools: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
    let mut saw_stop = false;
    let mut accumulated_thinking = String::new();
    let mut block_is_thinking = false;
    // ThinkParser strips inline `<think>…</think>` tags from text deltas
    // so reasoning models that emit thinking inline (DeepSeek-R1, GLM-Z1,
    // some Qwen variants) don't leak the thinking into the visible
    // content stream.
    let mut think_parser = ThinkParser::new();

    while let Some(event) = stream.next_event().await? {
        match event {
            ApiStreamEvent::MessageStart(start) => {
                for (index, block) in start.message.content.into_iter().enumerate() {
                    push_output_block(block, index as u32, &mut events, &mut pending_tools, true);
                }
            }
            ApiStreamEvent::ContentBlockStart(start) => {
                // Flush any inline <think> reasoning extracted from prior text
                // deltas before starting a new block. The parser state spans
                // block boundaries — a <think> tag that opened in a text
                // delta must be closed (or finalized) when a new block starts.
                let (trailing_visible, trailing_reasoning) = think_parser.finish();
                if !trailing_visible.is_empty() {
                    events.push(AssistantEvent::TextDelta(trailing_visible));
                }
                if !trailing_reasoning.is_empty() {
                    accumulated_thinking.push_str(&trailing_reasoning);
                }
                block_is_thinking = matches!(
                    start.content_block,
                    OutputContentBlock::Thinking { .. }
                );
                if !block_is_thinking {
                    flush_thinking_block(&mut events, &mut accumulated_thinking);
                }
                push_output_block(
                    start.content_block,
                    start.index,
                    &mut events,
                    &mut pending_tools,
                    true,
                );
            }
            ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                ContentBlockDelta::TextDelta { text } => {
                    if !text.is_empty() {
                        // Route text through the ThinkParser to extract any
                        // inline `<think>…</think>` content. Reasoning
                        // extracted from the visible stream is folded into
                        // `accumulated_thinking` and flushed with the
                        // provider-native thinking deltas.
                        let (visible, reasoning) = think_parser.push(&text);
                        if !visible.is_empty() {
                            events.push(AssistantEvent::TextDelta(visible));
                        }
                        if !reasoning.is_empty() {
                            accumulated_thinking.push_str(&reasoning);
                            block_is_thinking = true;
                        }
                    }
                }
                ContentBlockDelta::InputJsonDelta { partial_json } => {
                    if let Some((_, _, input)) = pending_tools.get_mut(&delta.index) {
                        input.push_str(&partial_json);
                    }
                }
                ContentBlockDelta::ThinkingDelta { thinking } => {
                    if !thinking.is_empty() {
                        accumulated_thinking.push_str(&thinking);
                    }
                }
                ContentBlockDelta::SignatureDelta { .. } => {}
            },
            ApiStreamEvent::ContentBlockStop(stop) => {
                // Finalize the parser state at block end so any pending
                // tag-boundary buffer is flushed.
                let (trailing_visible, trailing_reasoning) = think_parser.finish();
                if !trailing_visible.is_empty() {
                    events.push(AssistantEvent::TextDelta(trailing_visible));
                }
                if !trailing_reasoning.is_empty() {
                    accumulated_thinking.push_str(&trailing_reasoning);
                }
                if block_is_thinking || !accumulated_thinking.is_empty() {
                    flush_thinking_block(&mut events, &mut accumulated_thinking);
                    block_is_thinking = false;
                }
                if let Some((id, name, input)) = pending_tools.remove(&stop.index) {
                    let input = serde_json::from_str(&input)
                        .unwrap_or_else(|_| serde_json::json!({ "raw": input }));
                    events.push(AssistantEvent::ToolUse { id, name, input });
                }
            }
            ApiStreamEvent::MessageDelta(delta) => {
                events.push(AssistantEvent::Usage(delta.usage.token_usage()));
            }
            ApiStreamEvent::MessageStop(_) => {
                saw_stop = true;
                // Finalize the parser at message end so any unterminated
                // inline <think> content is flushed as reasoning.
                let (trailing_visible, trailing_reasoning) = think_parser.finish();
                if !trailing_visible.is_empty() {
                    events.push(AssistantEvent::TextDelta(trailing_visible));
                }
                if !trailing_reasoning.is_empty() {
                    accumulated_thinking.push_str(&trailing_reasoning);
                }
                if block_is_thinking || !accumulated_thinking.is_empty() {
                    flush_thinking_block(&mut events, &mut accumulated_thinking);
                    block_is_thinking = false;
                }
                events.push(AssistantEvent::MessageStop);
            }
        }
    }

    push_prompt_cache_record(client, &mut events);

    if !saw_stop
        && events.iter().any(|event| {
            matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                || matches!(event, AssistantEvent::ToolUse { .. })
        })
    {
        events.push(AssistantEvent::MessageStop);
    }

    if events
        .iter()
        .any(|event| matches!(event, AssistantEvent::MessageStop))
    {
        return Ok(events);
    }

    let response = client
        .send_message(&MessageRequest {
            stream: false,
            ..message_request.clone()
        })
        .await?;
    let mut events = response_to_events(response);
    push_prompt_cache_record(client, &mut events);
    Ok(events)
}

fn push_output_block(
    block: OutputContentBlock,
    block_index: u32,
    events: &mut Vec<AssistantEvent>,
    pending_tools: &mut BTreeMap<u32, (String, String, String)>,
    streaming_tool_input: bool,
) {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            pending_tools.insert(block_index, (id, name, initial_input));
        }
        OutputContentBlock::Thinking { thinking, .. } => {
            if !thinking.is_empty() {
                let (clean, tool_calls) = extract_embedded_tools(&thinking);
                for (id, name, input) in tool_calls {
                    events.push(AssistantEvent::ToolUse { id, name, input });
                }
                if !clean.trim().is_empty() {
                    events.push(AssistantEvent::Thinking(clean));
                } else if !thinking.trim().is_empty() {
                    events.push(AssistantEvent::Thinking(thinking));
                }
            }
        }
        OutputContentBlock::RedactedThinking { .. } => {
            events.push(AssistantEvent::Thinking(
                "[Thinking block hidden by provider]".to_string(),
            ));
        }
        OutputContentBlock::Image { .. } => {}
    }
}

fn response_to_events(response: MessageResponse) -> Vec<AssistantEvent> {
    let mut events = Vec::new();
    let mut pending_tools = BTreeMap::new();

    for (index, block) in response.content.into_iter().enumerate() {
        let index = u32::try_from(index).expect("response block index overflow");
        push_output_block(block, index, &mut events, &mut pending_tools, false);
        if let Some((id, name, input)) = pending_tools.remove(&index) {
            let input = serde_json::from_str(&input)
                .unwrap_or_else(|_| serde_json::json!({ "raw": input }));
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    events
}

fn push_prompt_cache_record(client: &ProviderClient, events: &mut Vec<AssistantEvent>) {
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

fn prompt_cache_record_to_runtime_event(
    record: api::PromptCacheRecord,
) -> Option<runtime::PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(runtime::PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}

fn flush_thinking_block(events: &mut Vec<AssistantEvent>, accumulated_thinking: &mut String) {
    if accumulated_thinking.is_empty() {
        return;
    }
    let text = std::mem::take(accumulated_thinking);
    let (clean, tool_calls) = extract_embedded_tools(&text);

    for (id, name, input) in tool_calls {
        events.push(AssistantEvent::ToolUse { id, name, input });
    }

    if !clean.trim().is_empty() {
        events.push(AssistantEvent::Thinking(clean));
    } else if !text.trim().is_empty() {
        events.push(AssistantEvent::Thinking(text));
    }
}

pub struct SubagentToolExecutor {
    allowed_tools: BTreeSet<String>,
    policy: Option<PermissionPolicy>,
}

impl SubagentToolExecutor {
    pub fn new(allowed_tools: BTreeSet<String>) -> Self {
        Self {
            allowed_tools,
            policy: None,
        }
    }

    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
}

impl ToolExecutor for SubagentToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if !self.allowed_tools.contains(tool_name) {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled for this sub-agent"
            )));
        }
        let value: Value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;

        let exec = GLOBAL_TOOL_EXECUTOR.get().ok_or_else(|| {
            ToolError::new(
                "subagent tool executor not registered; \
                 call agents::runtime::register_tool_executor from the tools crate"
                    .to_string(),
            )
        })?;
        exec(tool_name, &value, self.policy.as_ref()).map_err(ToolError::new)
    }
}

fn tool_specs_for_allowed_tools(
    allowed_tools: Option<&BTreeSet<String>>,
) -> Vec<runtime::tool_registry::ToolSpec> {
    runtime::tool_registry::mvp_tool_specs()
        .into_iter()
        .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
        .collect()
}

// Deleted 2026-06-04 per spec §5.4 (cycle-break Option 2).
// The 18-spec subset was the *permission-relevant* view; until the
// PermissionMode filter criterion is decided (spec §11), callers see all 53.

fn agent_permission_policy() -> PermissionPolicy {
    runtime::tool_registry::mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(PermissionMode::DangerFullAccess),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    )
}

pub fn build_agent_system_prompt(subagent_type: &str) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    use crate::persist::DEFAULT_AGENT_SYSTEM_DATE;
    let mut prompt = load_system_prompt(
        cwd,
        DEFAULT_AGENT_SYSTEM_DATE.to_string(),
        std::env::consts::OS,
        "unknown",
    )
    .map_err(|error| error.to_string())?;
    prompt.push(format!(
        "You are a background sub-agent of type `{subagent_type}`. \
         Work only on the delegated task, use only the tools available to you, \
         do not ask the user questions, and finish with a concise result."
    ));
    Ok(prompt)
}

pub fn resolve_agent_model(model: Option<&str>) -> String {
    use crate::persist::DEFAULT_AGENT_MODEL;
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_AGENT_MODEL)
        .to_string()
}

pub fn build_agent_runtime(
    job: &AgentJob,
) -> Result<ConversationRuntime<ProviderRuntimeClient, SubagentToolExecutor>, String> {
    use crate::persist::DEFAULT_AGENT_MODEL;
    let model = job
        .manifest
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string());
    let allowed_tools = job.allowed_tools.clone();
    let api_client = ProviderRuntimeClient::new(model, allowed_tools.clone())?;
    let permission_policy = agent_permission_policy();
    let tool_executor = SubagentToolExecutor::new(allowed_tools)
        .with_permission_policy(permission_policy.clone());
    Ok(ConversationRuntime::new(
        Session::new(),
        api_client,
        tool_executor,
        permission_policy,
        job.system_prompt.clone(),
    ))
}
