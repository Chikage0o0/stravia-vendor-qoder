//! Qoder 原生 RemoteChatAsk 传输；仅官方 SSE finish 事件决定成功终止。
use crate::{PROTOCOL, Region, auth, protocol};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::transform::{ProtocolTransform, StreamDecodeStage};
use stravia_runtime_contract::protocol::ir::{
    AiRequest, ContentBlock, MediaSource, MessageContent, ProtocolExt, Role, ToolChoice,
    ToolResultContentKind,
};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AiErrorKind, AiStreamDelta, ErrorKind, GuestHost, OperationOutput, PluginError,
    ProviderSnapshot, read_http_body,
};

const CHAT_PATH: &str = "/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
    mut request: AiRequest,
) -> Result<OperationOutput, PluginError> {
    let model = provider
        .model
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            common::plugin_error(ErrorKind::Invalid, "Qoder inference requires a model")
        })?;
    if !crate::models::validate(&provider.options).issues.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "invalid Qoder model options",
        ));
    }
    let session = provider
        .operation_metadata
        .get("session_affinity")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(native_session_id)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let model_config = provider
        .model_metadata
        .as_ref()
        .and_then(|m| m.extensions.get("qoder_model_config"));
    let source = model_config
        .and_then(|m| m.get("source"))
        .or_else(|| {
            provider
                .model_metadata
                .as_ref()
                .and_then(|m| m.extensions.get("qoder_source"))
        })
        .and_then(Value::as_str)
        .unwrap_or("system");
    let identity = auth::identity(provider, region)?;
    request.model = model.to_owned();
    request.stream.enabled = true;
    let canonical = project_request(&request)?;
    let body = build_body(canonical, model, source, &session, region, model_config)?;
    let raw = serde_json::to_vec(&body)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "cannot serialize Qoder request"))?;
    let mut http = protocol::prepare(
        region,
        &identity,
        "POST",
        CHAT_PATH,
        &raw,
        Some(model),
        Some(source),
    )?;
    protocol::add_client_headers(
        &mut http.headers,
        &provider.options,
        &identity.machine_id,
        true,
    )?;
    host.emit_started()?;
    let response = host.http_start(http)?;
    let status = response.status()?;
    if status != 200 {
        let headers = response.headers()?;
        let body = read_http_body(&response, 256 * 1024)?;
        let text = std::str::from_utf8(&body).unwrap_or("");
        let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let mut error = classified_error(status, &value, text);
        if error.kind.retry_after().is_none()
            && error
                .kind
                .model_error_kind()
                .is_some_and(|kind| kind.is_retryable())
        {
            let fallback = common::upstream_error(status, &headers, &[]);
            error.kind =
                ErrorKind::upstream(error.kind.model_error_kind(), fallback.kind.retry_after());
        }
        return Err(error);
    }
    let mut accumulator = StreamResponseAccumulator::default();
    consume_stream(
        || response.read_body(),
        |deltas| common::emit_deltas(host, &mut accumulator, deltas),
    )?;
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(OperationOutput::Infer(Box::new(complete)))
}

fn native_session_id(affinity: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"qoder-session-v1\0");
    digest.update(affinity.as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.finalize()[..16]);
    // 保留宿主链根的稳定性，仅转换为 CLI 使用的 UUID 布局；这不是随机熵。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

// finish 已代表事务终结，不再等待上游 EOF 或读取连接上的下一块。
fn consume_stream(
    mut read: impl FnMut() -> Result<Option<Vec<u8>>, PluginError>,
    mut emit: impl FnMut(&[AiStreamDelta]) -> Result<(), PluginError>,
) -> Result<(), PluginError> {
    let mut parser = StreamInterpreter::new()?;
    while !parser.frames.finished {
        let Some(chunk) = read()? else {
            break;
        };
        emit(&parser.push(&chunk)?)?;
    }
    emit(&parser.finish()?)
}

// 直接投影 canonical IR；OpenAI 编码器会重排并行工具历史，不能充当原生 wire 中间层。
fn project_request(request: &AiRequest) -> Result<Value, PluginError> {
    let mut parameters = serde_json::Map::new();
    // 仅投影 CLI 可表达的参数；其他请求选项不参与出站请求，也不使推理失败。
    if let Some(value) = request.generation.temperature {
        parameters.insert("temperature".into(), json!(value));
    }
    if let Some(value) = request.generation.top_p {
        parameters.insert("top_p".into(), json!(value));
    }
    if let Some(value) = request.generation.max_tokens {
        parameters.insert("max_tokens".into(), json!(value));
    }
    if let Some(level) = request.reasoning.level {
        let effort = level.as_str();
        parameters.insert(
            "reasoning_effort".into(),
            json!(if effort == "off" { "none" } else { effort }),
        );
    } else if let Some(effort) = &request.reasoning.effort {
        let effort = serde_json::to_value(effort).map_err(|_| unsupported_request())?;
        if effort.is_string() {
            parameters.insert("reasoning_effort".into(), effort);
        }
    }
    if let Some(budget) = request.reasoning.budget_tokens {
        parameters.insert("reasoning_budget_tokens".into(), json!(budget));
        parameters.insert("enable_thinking".into(), json!(budget > 0));
    } else if request.reasoning.enabled {
        parameters.insert("enable_thinking".into(), json!(true));
    }
    if let Some(choice) = &request.tool_choice {
        let value = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Named { name } => json!({"type":"function","function":{"name":name}}),
            ToolChoice::Raw(value) => value.clone(),
        };
        parameters.insert("tool_choice".into(), value);
    }
    if let Some(ProtocolExt::Anthropic(ext)) = &request.ext
        && let Some(top_k) = ext.top_k
    {
        parameters.insert("top_k".into(), json!(top_k));
    }
    // DlA/Fgi 在拆分工具结果之前选择最后一条历史的断点；全为排除块时不退到前一条。
    let cache_breakpoint = request
        .items
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| matches!(item.role, Role::User | Role::Assistant | Role::Tool))
        .and_then(|(message_index, item)| {
            if item.role == Role::Tool {
                return None;
            }
            let MessageContent::Blocks(blocks) = &item.content else {
                return None;
            };
            blocks
                .iter()
                .rposition(|block| {
                    !matches!(
                        block,
                        ContentBlock::Thinking { .. }
                            | ContentBlock::Reasoning { .. }
                            | ContentBlock::RedactedThinking { .. }
                            | ContentBlock::ToolUse { .. }
                            | ContentBlock::ToolResult { .. }
                    )
                })
                .map(|block_index| (message_index, block_index))
        });
    let mut messages = Vec::with_capacity(request.items.len());
    for (message_index, item) in request.items.iter().enumerate() {
        let role = match item.role {
            Role::System => "system",
            Role::Developer => "developer",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let mut row = json!({"role":role,"content":""});
        let mut text = Vec::new();
        let mut contents = Vec::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        match &item.content {
            MessageContent::Text(value) => {
                row["content"] = json!(value);
            }
            MessageContent::Blocks(blocks) => {
                for (block_index, block) in blocks.iter().enumerate() {
                    match block {
                        ContentBlock::Text {
                            text: value,
                            cache_control,
                        } => {
                            if value.is_empty() {
                                continue;
                            }
                            text.push(value.as_str());
                            let mut part = json!({"type":"text","text":value});
                            if cache_control.is_some()
                                || cache_breakpoint == Some((message_index, block_index))
                            {
                                // canonical 的 ttl/断点优先级不是 CLI wire 字段。
                                part["cache_control"] = json!({"type":"ephemeral"});
                            }
                            contents.push(part);
                        }
                        ContentBlock::Image { source, .. } => {
                            contents.push(remote_image(source)?);
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            let arguments = if let Some(value) = input.as_str() {
                                value.to_owned()
                            } else {
                                serde_json::to_string(input).map_err(|_| unsupported_request())?
                            };
                            calls.push(json!({"id":id,"type":"function","index":calls.len(),"function":{"name":name,"arguments":arguments}}));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            content_kind,
                            is_error,
                            ..
                        } => {
                            let content = if *content_kind == Some(ToolResultContentKind::Json) {
                                json!(content.to_string())
                            } else {
                                remote_result(content)?
                            };
                            let mut result =
                                json!({"role":"tool","tool_call_id":tool_use_id,"content":content});
                            if *is_error == Some(true) {
                                result["is_error"] = json!(true);
                            }
                            results.push(result);
                        }
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            let current = row
                                .get("reasoning_content")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            row["reasoning_content"] = json!(format!("{current}{thinking}"));
                            if let Some(signature) = signature {
                                row["reasoning_content_signature"] = json!(signature);
                            }
                            row["reasoning_item"] = json!({"type":"reasoning","summary":[{"type":"summary_text","text":row["reasoning_content"]}]});
                        }
                        ContentBlock::Reasoning {
                            summary,
                            content,
                            encrypted_content,
                        } => {
                            let mut reasoning = json!({"type":"reasoning"});
                            if !summary.is_empty() {
                                reasoning["summary"] = json!(
                                    summary
                                        .iter()
                                        .map(|text| json!({"type":"summary_text","text":text}))
                                        .collect::<Vec<_>>()
                                );
                            }
                            if !content.is_empty() {
                                reasoning["content"] = json!(
                                    content
                                        .iter()
                                        .map(|text| json!({"type":"reasoning_text","text":text}))
                                        .collect::<Vec<_>>()
                                );
                            }
                            if let Some(encrypted) = encrypted_content {
                                reasoning["encrypted_content"] = json!(encrypted);
                            }
                            row["reasoning_item"] = reasoning;
                        }
                        ContentBlock::RedactedThinking { data } => {
                            row["reasoning_item"] =
                                json!({"type":"reasoning","encrypted_content":data});
                        }
                        _ => return Err(unsupported_request()),
                    }
                }
                row["content"] = if matches!(item.role, Role::System | Role::Developer)
                    || contents.iter().any(|part| part["type"] == "image_url")
                {
                    json!(contents)
                } else {
                    json!(text.join(if item.role == Role::User { "\n" } else { "" }))
                };
                if !contents.is_empty() {
                    row["contents"] = json!(contents);
                }
            }
        }
        if let Some(tool_calls) = &item.tool_calls {
            for call in tool_calls {
                if !calls
                    .iter()
                    .any(|existing| existing["id"].as_str() == Some(call.id.as_str()))
                {
                    calls.push(json!({"id":call.id,"type":"function","index":calls.len(),"function":{"name":call.name,"arguments":call.arguments}}));
                }
            }
        }
        if !calls.is_empty() {
            row["tool_calls"] = json!(calls);
        }
        if let Some(id) = &item.tool_call_id {
            row["tool_call_id"] = json!(id);
        }
        // 官方 FWc 将同一 user 内容中的工具结果先发送，结果之间保持原顺序。
        let has_results = !results.is_empty();
        messages.extend(results);
        if !has_results
            || !contents.is_empty()
            || matches!(&item.content, MessageContent::Text(value) if !value.is_empty())
        {
            messages.push(row);
        }
    }
    if let Some(instructions) = &request.instructions {
        messages.insert(0, json!({"role":"system","content":instructions}));
    }
    let mut tools = Vec::with_capacity(request.tools.as_ref().map_or(0, Vec::len));
    if let Some(specs) = &request.tools {
        for spec in specs {
            tools.push(json!({"type":"function","function":{"name":spec.name,"description":spec.description.as_deref().unwrap_or(""),"parameters":spec.parameters}}));
        }
    }
    parameters.insert("messages".into(), json!(messages));
    parameters.insert("tools".into(), json!(tools));
    Ok(Value::Object(parameters))
}

fn remote_image(source: &MediaSource) -> Result<Value, PluginError> {
    let url = match source {
        MediaSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        MediaSource::Url(url) => url.clone(),
        _ => return Err(unsupported_request()),
    };
    Ok(json!({"type":"image_url","image_url":{"url":url}}))
}

fn remote_result(content: &Value) -> Result<Value, PluginError> {
    let Some(parts) = content.as_array() else {
        return Ok(content
            .as_str()
            .map(|value| json!(value))
            .unwrap_or_else(|| json!(content.to_string())));
    };
    let mut wire = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => wire.push(json!({"type":"text","text":part["text"]})),
            Some("image") => {
                let source: MediaSource = serde_json::from_value(part["source"].clone())
                    .map_err(|_| unsupported_request())?;
                wire.push(remote_image(&source)?);
            }
            Some("image_url") => wire.push(json!({
                "type":"image_url",
                "image_url":{"url":part["image_url"]["url"]}
            })),
            _ => return Err(unsupported_request()),
        }
    }
    if wire.iter().any(|part| part["type"] == "image_url") {
        Ok(json!(wire))
    } else {
        Ok(json!(
            wire.iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<String>()
        ))
    }
}

fn unsupported_request() -> PluginError {
    common::plugin_error(
        ErrorKind::Invalid,
        "Qoder RemoteChatAsk cannot represent this message content",
    )
}

fn build_body(
    canonical: Value,
    model: &str,
    source: &str,
    session: &str,
    region: Region,
    metadata: Option<&Value>,
) -> Result<Value, PluginError> {
    if session.trim().is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Qoder inference requires a session",
        ));
    }
    let Value::Object(mut parameters) = canonical else {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "invalid canonical Qoder request",
        ));
    };
    let mut messages = parameters.remove("messages").unwrap_or_else(|| json!([]));
    if !messages.is_array() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "invalid Qoder message history",
        ));
    }
    let tools = parameters.remove("tools").unwrap_or_else(|| json!([]));
    let mut system = parameters.remove("system").unwrap_or_else(|| json!([]));
    if let Some(rows) = messages.as_array_mut() {
        // S8e 单独接收 system，并在历史开头插入一条同内容消息。
        // canonical 中其他不同的 system/developer 内容也必须保留。
        let mut blocks = match &system {
            Value::Array(blocks) => blocks.clone(),
            Value::String(text) => vec![json!({"type":"text","text":text})],
            Value::Null => Vec::new(),
            _ => {
                return Err(common::plugin_error(
                    ErrorKind::Invalid,
                    "invalid Qoder system prompt",
                ));
            }
        };
        rows.retain(|row| {
            if !matches!(
                row.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            ) {
                return true;
            }
            let content = row.get("content").unwrap_or(&Value::Null);
            let additions = match content {
                Value::String(text) => vec![json!({"type":"text","text":text})],
                Value::Array(parts) => parts.clone(),
                Value::Null => Vec::new(),
                value => vec![value.clone()],
            };
            blocks.extend(additions);
            false
        });
        system = Value::Array(blocks);
        for row in rows.iter_mut() {
            project_remote_message(row)?;
        }
        if system.as_array().is_some_and(|blocks| !blocks.is_empty()) {
            rows.insert(0, json!({"role":"system","content":system}));
        }
    }
    if parameters.get("preserve_thinking").and_then(Value::as_bool) == Some(false)
        && let Some(rows) = messages.as_array_mut()
    {
        for row in rows {
            if let Some(row) = row.as_object_mut() {
                row.remove("reasoning_content");
                row.remove("reasoning_content_signature");
                row.remove("reasoning_item");
            }
        }
    }
    // yci/S8e 只定义这些生成参数；未知字段在编码和签名前静默丢弃。
    parameters.retain(|key, _| {
        matches!(
            key.as_str(),
            "temperature"
                | "top_p"
                | "top_k"
                | "max_tokens"
                | "reasoning_effort"
                | "enable_thinking"
                | "reasoning_budget_tokens"
                | "preserve_thinking"
                | "context_length"
                | "tool_choice"
        )
    });
    // Zel 逐字段构造配置，不把目录条目或其他 metadata 原样发送。
    let catalog = metadata.and_then(Value::as_object);
    if metadata.is_some() && catalog.is_none() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "invalid Qoder model configuration",
        ));
    }
    let field = |name: &str, fallback: Value| {
        catalog
            .and_then(|row| row.get(name))
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or(fallback)
    };
    let mut config = json!({
        "key":model,
        "display_name":field("display_name", json!(model)),
        "format":field("format", json!("openai")),
        "is_vl":field("is_vl", json!(false)),
        "is_reasoning":field("is_reasoning", json!(false)),
        "api_key":"",
        "url":"",
        "source":field("source", json!(source)),
        "max_input_tokens":field("max_input_tokens", json!(200000))
    });
    let object = config.as_object_mut().ok_or_else(|| {
        common::plugin_error(ErrorKind::Invalid, "invalid Qoder model configuration")
    })?;
    if let Some(provider) = catalog
        .and_then(|row| row.get("outer_provider"))
        .filter(|value| !value.is_null() && *value != &json!(false) && *value != &json!(""))
    {
        object.insert("outer_provider".into(), provider.clone());
    } else {
        object.insert("model".into(), json!(""));
    }
    if let Some(adapter) = catalog
        .and_then(|row| row.get("custom_provider_adapter"))
        .filter(|value| !value.is_null() && *value != &json!(false) && *value != &json!(""))
    {
        object.insert("custom_provider_adapter".into(), adapter.clone());
    }
    if !parameters.contains_key("max_tokens") {
        let limit = catalog
            .and_then(|row| row.get("max_output_tokens"))
            .and_then(Value::as_u64)
            .filter(|limit| *limit > 0 && *limit <= 9_007_199_254_740_991)
            .unwrap_or(32_000);
        parameters.insert("max_tokens".into(), json!(limit));
    } else {
        let limit = parameters
            .get("max_tokens")
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
            })
            .filter(|limit| *limit > 0 && *limit <= 9_007_199_254_740_991)
            .unwrap_or(32_000);
        parameters.insert("max_tokens".into(), json!(limit));
    }
    if let Some(effort) = parameters.get("reasoning_effort").and_then(Value::as_str) {
        let enabled = effort != "none";
        parameters.insert("enable_thinking".into(), json!(enabled));
        if !enabled {
            parameters.remove("reasoning_budget_tokens");
        }
    }
    let reasoning = object.get("is_reasoning").and_then(Value::as_bool) == Some(true)
        && parameters.get("enable_thinking").and_then(Value::as_bool) != Some(false);
    object.insert("is_reasoning".into(), json!(reasoning));
    let prompt = messages
        .as_array()
        .and_then(|messages| {
            messages.iter().rev().find(|m| {
                m.get("role").and_then(Value::as_str) == Some("user")
                    && m.get("content").is_some_and(Value::is_string)
            })
        })
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let id = uuid::Uuid::new_v4().to_string();
    let request_set_id = uuid::Uuid::new_v4().to_string();
    let mut name_end = 0;
    let mut name_units = 0;
    for (offset, character) in prompt.char_indices() {
        name_units += character.len_utf16();
        if name_units > 10 {
            break;
        }
        name_end = offset + character.len_utf8();
    }
    // 正常 CLI 在首轮推理前创建 AgentLifecycle 并进入 start；Qwen 路由依赖此上下文。
    // 标题遵守十个 UTF-16 单元上限，但不生成被截断的半个代理对。
    Ok(json!({
        "request_id": id, "request_set_id": request_set_id, "session_id": session, "chat_record_id": id,
        "stream": true, "chat_task": "FREE_INPUT", "chat_context": {"text":prompt,"features":[],"extra":{"context":[],"modelConfig":{"key":model,"is_reasoning":reasoning},"originalContent":prompt},"chatPrompt":"","imageUrls":null}, "is_reply": true,
        "is_retry": false, "source": 1, "version": "3", "agent_id": "agent_common",
        "task_id": "common", "session_type": if region == Region::Cn {"qoderclicn"} else {"qodercli"}, "aliyun_user_type": "",
        "model_config": config,
        "business": {
            "product": "cli", "version": protocol::CLI_VERSION, "type": "agent",
            "id": uuid::Uuid::new_v4().to_string(), "name": &prompt[..name_end],
            "begin_at": chrono::Utc::now().timestamp_millis(), "stage": "start"
        },
        "system": system, "messages": messages, "tools": tools,
        "parameters": parameters
    }))
}

fn project_remote_message(message: &mut Value) -> Result<(), PluginError> {
    let row = message
        .as_object_mut()
        .ok_or_else(|| common::plugin_error(ErrorKind::Invalid, "invalid Qoder message"))?;
    let is_user = row.get("role").and_then(Value::as_str) == Some("user");
    let is_assistant = row.get("role").and_then(Value::as_str) == Some("assistant");
    match row.get("role").and_then(Value::as_str) {
        Some("user" | "assistant") => {
            if let Some(content) = row.get("content") {
                let contents = match content {
                    Value::String(text) if is_assistant && text.is_empty() => Vec::new(),
                    Value::String(text) => vec![json!({"type":"text","text":text})],
                    Value::Array(parts) => parts.clone(),
                    Value::Null => Vec::new(),
                    _ => {
                        return Err(common::plugin_error(
                            ErrorKind::Invalid,
                            "invalid Qoder message content",
                        ));
                    }
                };
                if contents
                    .iter()
                    .all(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    && row.get("content").is_some_and(Value::is_array)
                {
                    let text = contents
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(if is_user { "\n" } else { "" });
                    row.insert("content".into(), json!(text));
                }
                if is_user || !contents.is_empty() {
                    row.entry("contents".to_owned())
                        .or_insert(Value::Array(contents));
                }
            }
            if let Some(calls) = row.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for (index, call) in calls.iter_mut().enumerate() {
                    if let Some(call) = call.as_object_mut() {
                        call.insert("index".into(), json!(index));
                    }
                }
            }
        }
        Some("tool") => {
            // vWc 在兼容 OpenAI 的 content 之外保留多模态 contents。
            if let Some(Value::Array(parts)) = row.get("content").cloned() {
                if parts
                    .iter()
                    .all(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                {
                    let text = parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("");
                    row.insert("content".into(), json!(text));
                } else {
                    row.entry("contents".to_owned())
                        .or_insert(Value::Array(parts));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn stream_error() -> PluginError {
    common::model_error(AiErrorKind::ServerError, "Qoder upstream stream failed")
}

struct StreamInterpreter {
    frames: SseFrames,
    decoder: StreamDecodeStage,
}
impl StreamInterpreter {
    fn new() -> Result<Self, PluginError> {
        Ok(Self {
            frames: SseFrames::default(),
            decoder: ProtocolTransform::global()
                .decode_stream(common::endpoint(PROTOCOL)?)
                .map_err(common::map_response_transform_error)?,
        })
    }
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<AiStreamDelta>, PluginError> {
        let normalized = self.frames.feed(bytes)?;
        let mut deltas = self
            .decoder
            .decode_chunk(&normalized)
            .map_err(common::map_response_transform_error)?;
        redact_errors(&mut deltas);
        Ok(deltas)
    }
    fn finish(&mut self) -> Result<Vec<AiStreamDelta>, PluginError> {
        if !self.frames.finished || !self.frames.pending.is_empty() || !self.frames.data.is_empty()
        {
            return Err(common::model_error(
                AiErrorKind::UnexpectedEof,
                "Qoder stream ended without a complete finish event",
            ));
        }
        let mut deltas = self
            .decoder
            .finish()
            .map_err(common::map_response_transform_error)?;
        redact_errors(&mut deltas);
        Ok(deltas)
    }
}
fn redact_errors(deltas: &mut [AiStreamDelta]) {
    for delta in deltas {
        if let AiStreamDelta::StreamError { error } = delta {
            error.message = "Qoder upstream stream failed".into();
            error.raw = None;
        }
    }
}

#[derive(Default)]
struct SseFrames {
    pending: Vec<u8>,
    data: Vec<u8>,
    event: Vec<u8>,
    size: usize,
    finished: bool,
}
impl SseFrames {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>, PluginError> {
        let mut out = Vec::new();
        for &byte in bytes {
            if self.finished {
                break;
            }
            self.size += 1;
            if self.size > MAX_EVENT_BYTES {
                return Err(common::plugin_error(
                    ErrorKind::ResourceExhausted,
                    "Qoder SSE event exceeds guest limit",
                ));
            }
            if byte == b'\n' {
                let mut line = std::mem::take(&mut self.pending);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    self.dispatch(&mut out)?;
                    self.size = 0;
                } else if let Some(data) = line.strip_prefix(b"data:") {
                    if !self.data.is_empty() {
                        self.data.push(b'\n');
                    }
                    self.data
                        .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
                } else if let Some(event) = line.strip_prefix(b"event:") {
                    self.event = event.strip_prefix(b" ").unwrap_or(event).to_vec();
                }
            } else {
                self.pending.push(byte);
            }
        }
        Ok(out)
    }
    fn dispatch(&mut self, out: &mut Vec<u8>) -> Result<(), PluginError> {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        if event == b"error" {
            let value = serde_json::from_slice(&data).unwrap_or(Value::Null);
            return Err(classified_error(
                500,
                &value,
                std::str::from_utf8(&data).unwrap_or(""),
            ));
        }
        if event == b"finish" {
            if !data.is_empty() && !marker(&data) {
                let value: Value = serde_json::from_slice(&data).map_err(|_| stream_error())?;
                reject_business_error(&value)?;
                if value
                    .get("statusCodeValue")
                    .is_some_and(|status| status.as_u64() != Some(200))
                {
                    return Err(envelope_error(&value));
                }
            }
            self.finished = true;
            out.extend_from_slice(b"data: [DONE]\n\n");
            return Ok(());
        }
        if data.is_empty() || marker(&data) {
            return Ok(());
        }
        let envelope: Value = serde_json::from_slice(&data).map_err(|_| stream_error())?;
        if envelope.get("statusCodeValue").and_then(Value::as_u64) != Some(200) {
            return Err(envelope_error(&envelope));
        }
        if envelope.get("code").is_some()
            || envelope.get("error").is_some()
            || envelope.get("queue").is_some()
        {
            reject_business_error(&envelope)?;
        }
        let body = envelope
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(stream_error)?;
        if marker(body.as_bytes()) {
            return Ok(());
        }
        let inner: Value = serde_json::from_str(body).map_err(|_| stream_error())?;
        reject_business_error(&inner)?;
        if !inner.get("choices").is_some_and(Value::is_array)
            && !inner.get("usage").is_some_and(Value::is_object)
        {
            return Err(stream_error());
        }
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\n\n");
        Ok(())
    }
}
fn marker(bytes: &[u8]) -> bool {
    bytes == b"[DONE]"
        || bytes == b"[NOT_EXCEED_QUOTA]"
        || bytes.starts_with(b"[EXCEED_QUOTA]")
        || bytes.starts_with(b"[NOTIFICATIONS]")
}
fn reject_business_error(value: &Value) -> Result<(), PluginError> {
    let mut facts = ErrorFacts::default();
    facts.inspect(value, 0);
    if facts.failed || facts.queued || facts.code.is_some_and(|code| code != 0) {
        return Err(classified_error(500, value, ""));
    }
    Ok(())
}

fn envelope_error(value: &Value) -> PluginError {
    let status = value
        .get("statusCodeValue")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .unwrap_or(500);
    let text = value.get("body").and_then(Value::as_str).unwrap_or("");
    let body = serde_json::from_str(text).unwrap_or(Value::Null);
    classified_error(status, &body, text)
}

#[derive(Default)]
struct ErrorFacts {
    code: Option<i64>,
    queued: bool,
    failed: bool,
    retry_ms: Option<u64>,
    visited: usize,
}
impl ErrorFacts {
    // 仅遍历官方 error/data/body/message/cause 容器；限制嵌套及记录数量。
    fn inspect(&mut self, value: &Value, depth: usize) {
        if depth > 8 || self.visited >= 64 {
            return;
        }
        self.visited += 1;
        if let Some(text) = value.as_str() {
            if let Ok(nested) = serde_json::from_str::<Value>(text) {
                self.inspect(&nested, depth + 1);
            }
            return;
        }
        let Some(record) = value.as_object() else {
            return;
        };
        self.failed |= record.get("code").is_some_and(|code| {
            code.as_i64()
                .or_else(|| code.as_str().and_then(|s| s.parse().ok()))
                != Some(0)
        });
        if let Some(code) = record.get("code").and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }) && (self.code.is_none()
            || matches!(code, 105 | 103 | 10605) && !matches!(self.code, Some(105 | 103 | 10605)))
        {
            self.code = Some(code);
        }
        self.failed |= record.get("error").is_some_and(|error| !error.is_null());
        self.queued |= record.get("isQueued").and_then(Value::as_bool) == Some(true)
            || record
                .get("queue")
                .and_then(|q| q.get("isQueued"))
                .and_then(Value::as_bool)
                == Some(true);
        if self.retry_ms.is_none() {
            self.retry_ms = record
                .get("retry_after_ms")
                .or_else(|| record.get("retryAfterMs"))
                .and_then(Value::as_u64)
                .or_else(|| record.get("retryAfterSeconds").and_then(seconds_to_ms))
                .or_else(|| {
                    record
                        .get("queue")
                        .and_then(|q| q.get("retryAfterSeconds"))
                        .and_then(seconds_to_ms)
                });
        }
        for field in ["error", "data", "body", "message", "cause"] {
            if let Some(nested) = record.get(field) {
                self.inspect(nested, depth + 1);
            }
        }
    }
}
fn seconds_to_ms(value: &Value) -> Option<u64> {
    let seconds = value.as_f64()?;
    if !seconds.is_finite() || seconds < 0.0 || seconds > u64::MAX as f64 / 1000.0 {
        return None;
    }
    Some((seconds * 1000.0).round() as u64)
}
fn classified_error(status: u16, value: &Value, text: &str) -> PluginError {
    let mut facts = ErrorFacts::default();
    facts.inspect(value, 0);
    let mut kind = common::upstream_error(status, &[], &[])
        .kind
        .model_error_kind()
        .unwrap_or(AiErrorKind::ServerError);
    let mut message = "Qoder upstream request failed";
    match facts.code {
        Some(105) => {
            kind = AiErrorKind::AuthenticationError;
            message = "Qoder authentication expired";
        }
        Some(103) => {
            kind = AiErrorKind::InvalidRequest;
            message = "Qoder duplicate request rejected";
        }
        Some(10605) => {
            kind = AiErrorKind::RateLimitError;
            message = "Qoder model request queued";
        }
        _ if facts.queued => {
            kind = AiErrorKind::RateLimitError;
            message = "Qoder model request queued";
        }
        None if contains_login_marker(text) => {
            kind = AiErrorKind::AuthenticationError;
            message = "Qoder authentication expired";
        }
        _ => {}
    }
    let retry = if kind.is_retryable() {
        facts.retry_ms.map(Duration::from_millis)
    } else {
        None
    };
    PluginError {
        kind: ErrorKind::upstream(Some(kind), retry),
        message: message.into(),
        upstream_status: Some(status),
    }
}
fn contains_login_marker(text: &str) -> bool {
    [b"login expired".as_slice(), b"login timeout".as_slice()]
        .into_iter()
        .any(|marker| {
            text.as_bytes()
                .windows(marker.len())
                .any(|window| window.eq_ignore_ascii_case(marker))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn envelope(inner: Value) -> Vec<u8> {
        format!(
            "data: {}\n\n",
            json!({"statusCodeValue":200,"body":inner.to_string()})
        )
        .into_bytes()
    }
    #[test]
    fn unicode_and_tool_usage_survive_single_byte_chunks() {
        let inner = json!({"id":"response-1","model":"model","choices":[{"index":0,"delta":{"content":"你好","tool_calls":[{"index":0,"id":"call1","type":"function","function":{"name":"天气","arguments":"{\"城市\":"}}]},"finish_reason":null}]});
        let tail = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"北京\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
        let mut parser = StreamInterpreter::new().unwrap();
        let mut accumulator = StreamResponseAccumulator::default();
        let mut wire = envelope(inner);
        wire.extend(envelope(tail));
        wire.extend_from_slice(b"event: finish\n\n");
        let mut text = String::new();
        for byte in wire {
            let deltas = parser.push(&[byte]).unwrap();
            for delta in &deltas {
                if let AiStreamDelta::TextDelta(part) = delta {
                    text.push_str(part);
                }
            }
            accumulator.apply_all(&deltas);
        }
        accumulator.apply_all(&parser.finish().unwrap());
        assert_eq!(text, "你好");
        let tool = accumulator.tool_calls().next().unwrap();
        assert_eq!(tool.name, "天气");
        assert_eq!(tool.arguments, "{\"城市\":\"北京\"}");
        assert_eq!(accumulator.usage.total_tokens, 10);
        assert_eq!(accumulator.usage.prompt_tokens, 7);
    }
    #[test]
    fn inner_done_is_not_completion() {
        let mut parser = StreamInterpreter::new().unwrap();
        parser
            .push(&envelope(json!({"choices":[],"usage":{"total_tokens":1}})))
            .unwrap();
        parser
            .push(
                format!(
                    "data: {}\n\n",
                    json!({"statusCodeValue":200,"body":"[DONE]"})
                )
                .as_bytes(),
            )
            .unwrap();
        assert!(parser.finish().is_err());
    }
    #[test]
    fn finish_completes_without_eof_or_reading_another_chunk() {
        let mut chunk = envelope(
            json!({"id":"response","model":"model","choices":[{"index":0,"delta":{"content":"完成"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}),
        );
        chunk.extend_from_slice(
            b"event: finish\r\n\r\n: heartbeat\n\ndata: invalid-after-finish\n\n",
        );
        let mut reads = 0;
        let mut accumulator = StreamResponseAccumulator::default();
        consume_stream(
            || {
                reads += 1;
                assert_eq!(reads, 1, "finish 后不应再读取可能阻塞的下一块");
                Ok(Some(std::mem::take(&mut chunk)))
            },
            |deltas| {
                accumulator.apply_all(deltas);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(accumulator.usage.total_tokens, 3);
        assert_eq!(accumulator.stop_reason.as_deref(), Some("stop"));
    }
    #[test]
    fn envelope_and_business_errors_fail_closed() {
        for data in [
            json!({"statusCodeValue":401,"body":"secret"}),
            json!({"statusCodeValue":200,"body":"{\"code\":103}"}),
        ] {
            let mut frames = SseFrames::default();
            let error = frames
                .feed(format!("data: {data}\n\n").as_bytes())
                .unwrap_err();
            assert!(!error.message.contains("secret"));
        }
    }
    #[test]
    fn upstream_business_errors_keep_safe_consumer_classification() {
        for (code, kind, retry) in [
            ("105", AiErrorKind::AuthenticationError, None),
            (
                "10605",
                AiErrorKind::RateLimitError,
                Some(Duration::from_millis(1500)),
            ),
            ("103", AiErrorKind::InvalidRequest, None),
        ] {
            let body = json!({"code":code,"message":"private-token-secret","retryAfterMs":1500});
            let wire = json!({"statusCodeValue":500,"body":body.to_string()});
            let error = SseFrames::default()
                .feed(format!("data: {wire}\n\n").as_bytes())
                .unwrap_err();
            assert_eq!(error.kind.model_error_kind(), Some(kind));
            assert_eq!(error.kind.retry_after(), retry);
            assert!(!error.message.contains("private-token-secret"));
        }
        let body = json!({"data":{"isQueued":true,"retryAfterSeconds":1.5}});
        let error = SseFrames::default().feed(&envelope(body)).unwrap_err();
        assert_eq!(
            error.kind.model_error_kind(),
            Some(AiErrorKind::RateLimitError)
        );
        assert_eq!(error.kind.retry_after(), Some(Duration::from_millis(1500)));
    }
    #[test]
    fn reasoning_effort_controls_thinking_and_budget() {
        let config = json!({"key":"remote","source":"system","is_reasoning":true});
        let off = build_body(
            json!({"messages":[],"reasoning_effort":"none","reasoning_budget_tokens":100}),
            "remote",
            "system",
            "host-session",
            Region::Global,
            Some(&config),
        )
        .unwrap();
        assert_eq!(
            off.pointer("/parameters/enable_thinking"),
            Some(&json!(false))
        );
        assert!(off.pointer("/parameters/reasoning_budget_tokens").is_none());
        assert_eq!(
            off.pointer("/model_config/is_reasoning"),
            Some(&json!(false))
        );
        let on = build_body(json!({"messages":[],"reasoning_effort":"high","enable_thinking":false,"reasoning_budget_tokens":100}), "remote", "system", "host-session", Region::Global, Some(&config)).unwrap();
        assert_eq!(
            on.pointer("/parameters/enable_thinking"),
            Some(&json!(true))
        );
        assert_eq!(on.pointer("/model_config/is_reasoning"), Some(&json!(true)));
        assert_eq!(
            on.pointer("/parameters/reasoning_budget_tokens"),
            Some(&json!(100))
        );
    }

    #[test]
    fn tool_result_parts_keep_payload_without_extra_transport_fields() {
        let result = remote_result(&json!([
            {"type":"text","text":"result","cache_control":{"type":"ephemeral"},"extra":"ignored"},
            {"type":"image_url","image_url":{"url":"https://example.invalid/result.png","detail":"high","extra":"ignored"},"extra":"ignored"}
        ]))
        .unwrap();
        assert_eq!(
            result,
            json!([
                {"type":"text","text":"result"},
                {"type":"image_url","image_url":{"url":"https://example.invalid/result.png"}}
            ])
        );
    }

    #[test]
    fn unknown_generation_fields_do_not_override_native_request_context() {
        let body = build_body(
            json!({
                "messages":[{"role":"user","content":"hello"}],
                "temperature":0.25,"max_tokens":1234,
                "seed":42,"stop":["ignored"],"response_format":{"type":"json_object"},
                "session_id":"client-session","business":{"id":"client-business"},
                "prompt_cache_key":"client-cache"
            }),
            "remote",
            "system",
            "host-session",
            Region::Global,
            None,
        )
        .unwrap();
        assert_eq!(
            body["parameters"],
            json!({"temperature":0.25,"max_tokens":1234})
        );
        assert_eq!(body["session_id"], "host-session");
        assert_ne!(body["business"]["id"], "client-business");
    }
}
