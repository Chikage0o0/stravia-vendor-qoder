//! Qoder 原生 RemoteChatAsk 传输；仅官方 SSE finish 事件决定成功终止。
use crate::{PROTOCOL, Region, auth, protocol};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{borrow::Cow, time::Duration};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::transform::{ProtocolTransform, StreamDecodeStage};
use stravia_runtime_contract::protocol::ir::{
    AiRequest, CacheTtl, ContentBlock, MediaSource, MessageContent, Role, ToolChoice,
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
    let supports_vision = model_config
        .and_then(|config| config.get("is_vl"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let canonical = project_request(&request, supports_vision)?;
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
    let response = protocol::start_http(host, http)?;
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
fn project_request(request: &AiRequest, supports_vision: bool) -> Result<Value, PluginError> {
    let mut parameters = serde_json::Map::new();
    // 仅投影 CLI 可表达的参数；其他请求选项不参与出站请求，也不使推理失败。
    if let Some(value) = request.generation.temperature.filter(|value| *value >= 0.0) {
        parameters.insert("temperature".into(), json!(value));
    }
    if let Some(value) = request
        .generation
        .top_p
        .filter(|value| (0.0..=1.0).contains(value))
    {
        parameters.insert("top_p".into(), json!(value));
    }
    if let Some(value) = request.generation.max_tokens.filter(|value| *value > 0) {
        parameters.insert("max_tokens".into(), json!(value));
    }
    // is_enabled 的缺省 false 不是显式禁用；未提供控制时保留 CLI 目录默认。
    let explicit_mode = matches!(
        request.reasoning.mode.as_deref(),
        Some("disabled" | "enabled" | "auto" | "adaptive")
    );
    let thinking_enabled = (explicit_mode
        || request.reasoning.level.is_some()
        || request.reasoning.budget_tokens.is_some())
    .then(|| request.reasoning.is_enabled());
    // 仅 level 的正向激活须等目录接受该 effort，不能为被丢弃的档位新增参数。
    if let Some(enabled) = thinking_enabled
        && (explicit_mode
            || !enabled
            || request
                .reasoning
                .budget_tokens
                .is_some_and(|budget| budget > 0))
    {
        parameters.insert("enable_thinking".into(), json!(enabled));
    }
    if let Some(level) = request.reasoning.level {
        let effort = level.as_str();
        if !matches!(effort, "minimal" | "off") && thinking_enabled != Some(false) {
            parameters.insert("qoder_reasoning_effort".into(), json!(effort));
        }
    }
    if thinking_enabled == Some(false) {
        parameters.insert("reasoning_effort".into(), json!("none"));
    } else if let Some(budget) = request.reasoning.budget_tokens.filter(|budget| *budget > 0) {
        parameters.insert("reasoning_budget_tokens".into(), json!(budget));
        // htt/J8e 从显式预算派生原生 effort；只有调用方显式选择的 level 优先。
        parameters.entry("reasoning_effort").or_insert_with(|| {
            json!(match budget {
                0..=1024 => "low",
                1025..=8192 => "medium",
                8193..=24576 => "high",
                24577..=49152 => "xhigh",
                _ => "max",
            })
        });
    }
    if let Some(choice) = &request.tool_choice {
        let value = match choice {
            ToolChoice::Auto => Some(json!("auto")),
            ToolChoice::None => Some(json!("none")),
            ToolChoice::Required => Some(json!("required")),
            ToolChoice::Named { name } => Some(json!({"type":"function","function":{"name":name}})),
            ToolChoice::Allowed { .. } | ToolChoice::Native(_) => None,
        };
        if let Some(value) = value {
            parameters.insert("tool_choice".into(), value);
        }
    }
    if let Some(top_k) = request.parameters.top_k.filter(|value| *value > 0) {
        parameters.insert("top_k".into(), json!(top_k));
    }
    let cache_tail = request
        .items
        .iter()
        .rposition(|item| matches!(item.role, Role::User | Role::Assistant | Role::Tool));
    let mut messages = Vec::with_capacity(request.items.len());
    let mut message_index = 0;
    // Responses 的 reasoning/message/function_call 是内容项，不是独立的 assistant 轮。
    // 先恢复一轮的内容，再按官方 UWc 投影，避免发出孤立思考和拆开的并行调用。
    for group in request.items.chunk_by(|left, right| {
        left.role == right.role && matches!(left.role, Role::Assistant | Role::User)
    }) {
        let first = &group[0];
        // Fgi 在一条原生消息内回找缓存块，不能跨 user/tool 边界回退。
        let cache_breakpoint = (cache_tail == Some(message_index + group.len() - 1)
            && first.role != Role::Tool)
            .then(|| {
                group
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(offset, item)| match &item.content {
                        MessageContent::Text(text) if !text.is_empty() => {
                            Some((message_index + offset, None))
                        }
                        MessageContent::Blocks(blocks) => blocks
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
                            .map(|block_index| (message_index + offset, Some(block_index))),
                        _ => None,
                    })
            })
            .flatten();
        let role = match first.role {
            Role::System => "system",
            Role::Developer => "developer",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let mut row = json!({"role":role,"content":""});
        let mut text = Vec::new();
        let mut contents = Vec::<Value>::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        let mut reasoning_text = String::new();
        for (offset, item) in group.iter().enumerate() {
            if first.role == Role::User && offset > 0 {
                let ends_with_text = match &group[offset - 1].content {
                    MessageContent::Text(text) => !text.is_empty(),
                    MessageContent::Blocks(blocks) => {
                        matches!(blocks.last(), Some(ContentBlock::Text { .. }))
                    }
                };
                let starts_with_text = match &item.content {
                    MessageContent::Text(text) => !text.is_empty(),
                    MessageContent::Blocks(blocks) => {
                        matches!(blocks.first(), Some(ContentBlock::Text { .. }))
                    }
                };
                if ends_with_text
                    && starts_with_text
                    && let Some(part) = contents.last_mut()
                    && let Some(Value::String(text)) = part.get_mut("text")
                {
                    // yco/DVl 在两个连续 text 边界多插入一个换行，hVl 再以换行连接。
                    text.push('\n');
                }
            }
            match &item.content {
                MessageContent::Text(value) => {
                    text.push(value.as_str());
                    // 正常 bg/wVl/SVl 在 XQA 前把字符串恢复为 text 块。
                    if matches!(first.role, Role::User | Role::Assistant) && !value.is_empty() {
                        let mut part = json!({"type":"text","text":value});
                        if cache_breakpoint == Some((message_index + offset, None)) {
                            part["cache_control"] = json!({"type":"ephemeral"});
                        }
                        contents.push(part);
                    }
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
                                if cache_breakpoint
                                    == Some((message_index + offset, Some(block_index)))
                                {
                                    // HYi 的自动断点覆盖先前 TTL；更早的显式标记保留 1h。
                                    part["cache_control"] = json!({"type":"ephemeral"});
                                } else if let Some(cache) = cache_control {
                                    part["cache_control"] = if cache.ttl == CacheTtl::Ephemeral1h {
                                        json!({"type":"ephemeral","ttl":"1h"})
                                    } else {
                                        json!({"type":"ephemeral"})
                                    };
                                }
                                contents.push(part);
                            }
                            ContentBlock::Image { source, .. } => {
                                if first.role == Role::Assistant {
                                    // IVl 只投影 text/thinking/redacted_thinking/tool_use。
                                    continue;
                                }
                                // vVl 在 HYi 前降级不支持的格式；普通 image 的缓存字段
                                // 则在 hVl/wco 重建 image_url 时丢弃。
                                if let Some(part) = remote_image(
                                    source,
                                    supports_vision,
                                    cache_breakpoint
                                        == Some((message_index + offset, Some(block_index))),
                                )? {
                                    contents.push(part);
                                }
                            }
                            ContentBlock::ToolUse {
                                id, name, input, ..
                            } => {
                                let arguments = if input.is_null() {
                                    "{}".to_owned()
                                } else if let Some(value) = input.as_str() {
                                    value.to_owned()
                                } else {
                                    serde_json::to_string(input)
                                        .map_err(|_| unsupported_request())?
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
                                let content = if *content_kind == Some(ToolResultContentKind::Json)
                                {
                                    json!(content.to_string())
                                } else {
                                    remote_result(
                                        content,
                                        supports_vision,
                                        *is_error == Some(true),
                                    )?
                                };
                                let mut result = json!({"role":"tool","tool_call_id":tool_use_id,"content":content});
                                if *is_error == Some(true) {
                                    result["is_error"] = json!(true);
                                }
                                results.push(result);
                            }
                            ContentBlock::Thinking {
                                thinking,
                                signature,
                            } => {
                                reasoning_text.push_str(thinking);
                                if let Some(signature) =
                                    signature.as_deref().filter(|value| !value.is_empty())
                                {
                                    row["reasoning_content_signature"] = json!(signature);
                                }
                            }
                            ContentBlock::Reasoning {
                                summary,
                                content,
                                encrypted_content,
                            } => {
                                let mut reasoning = json!({"type":"reasoning"});
                                for (index, part) in summary.iter().chain(content).enumerate() {
                                    if index > 0 {
                                        reasoning_text.push('\n');
                                    }
                                    reasoning_text.push_str(part);
                                }
                                if !summary.is_empty() {
                                    reasoning["summary"] = json!(
                                        summary
                                            .iter()
                                            .map(|text| json!({"type":"summary_text","text":text}))
                                            .collect::<Vec<_>>()
                                    );
                                }
                                if !content.is_empty() {
                                    reasoning["content"] =
                                        json!(
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
            if let Some(id) = &item.tool_call_id {
                row["tool_call_id"] = json!(id);
            }
        }
        row["content"] = if (matches!(first.role, Role::System | Role::Developer)
            && matches!(&first.content, MessageContent::Blocks(_)))
            || contents.iter().any(|part| part["type"] == "image_url")
        {
            json!(contents)
        } else if !contents.is_empty() {
            let mut joined = String::new();
            let mut first_text = true;
            for value in contents
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
            {
                if !first_text && first.role == Role::User {
                    joined.push('\n');
                }
                joined.push_str(value);
                first_text = false;
            }
            json!(joined)
        } else {
            json!(text.join(if first.role == Role::User { "\n" } else { "" }))
        };
        if !contents.is_empty() {
            row["contents"] = json!(contents);
        }
        if !reasoning_text.is_empty() {
            if row.get("reasoning_item").is_none() {
                row["reasoning_item"] = json!({"type":"reasoning","summary":[{"type":"summary_text","text":reasoning_text}]});
            }
            row["reasoning_content"] = Value::String(reasoning_text);
        }
        if !calls.is_empty() {
            row["tool_calls"] = json!(calls);
        }
        // 官方 FWc 将同一 user 内容中的工具结果先发送，结果之间保持原顺序。
        let has_results = !results.is_empty();
        messages.extend(results);
        if first.role != Role::User
            && (!has_results || !contents.is_empty() || text.iter().any(|value| !value.is_empty()))
            || first.role == Role::User && !contents.is_empty()
        {
            messages.push(row);
        }
        message_index += group.len();
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

fn remote_image(
    source: &MediaSource,
    supports_vision: bool,
    cache_breakpoint: bool,
) -> Result<Option<Value>, PluginError> {
    let normalize_type = |media_type: &str| {
        let mut kind = media_type.trim().to_ascii_lowercase();
        if !kind.contains('/') {
            kind.insert_str(0, "image/");
        }
        if kind == "image/jpg" {
            kind = "image/jpeg".into();
        }
        kind
    };
    let supported = |media_type: &str| {
        matches!(
            media_type,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
    };
    let (url, unsupported) = match source {
        MediaSource::Base64 { media_type, data } => {
            let media_type = normalize_type(media_type);
            if supported(&media_type) {
                if data.is_empty() {
                    return Ok(None);
                }
                (Cow::Owned(format!("data:{media_type};base64,{data}")), None)
            } else {
                (Cow::Borrowed(""), Some(media_type))
            }
        }
        MediaSource::Url(url) => {
            if url.is_empty() {
                return Ok(None);
            }
            let data_type = url
                .get(..5)
                .filter(|prefix| prefix.eq_ignore_ascii_case("data:"))
                .and_then(|_| url[5..].split([';', ',']).next())
                .filter(|kind| kind.to_ascii_lowercase().starts_with("image/"))
                .map(normalize_type)
                .filter(|kind| !supported(kind));
            let unsupported = data_type.or_else(|| {
                let svg = url::Url::parse(url)
                    .map(|url| url.path().to_ascii_lowercase().ends_with(".svg"))
                    .unwrap_or_else(|_| {
                        url.split(['?', '#'])
                            .next()
                            .is_some_and(|path| path.to_ascii_lowercase().ends_with(".svg"))
                    });
                svg.then(|| "image/svg+xml".into())
            });
            (Cow::Borrowed(url.as_str()), unsupported)
        }
        _ => return Err(unsupported_request()),
    };
    if let Some(media_type) = unsupported {
        let mut part = json!({"type":"text","text":format!(
            "[Unsupported image omitted: {media_type}. Supported formats are PNG, JPEG, GIF, and WebP.]"
        )});
        if cache_breakpoint {
            part["cache_control"] = json!({"type":"ephemeral"});
        }
        return Ok(Some(part));
    }
    if !supports_vision {
        return Ok(Some(
            json!({"type":"text","text":"[Image omitted: the current model does not support image input. Ask the user to switch to a vision-capable model if the image content is needed.]"}),
        ));
    }
    Ok(Some(json!({"type":"image_url","image_url":{"url":url}})))
}

fn remote_result(
    content: &Value,
    supports_vision: bool,
    is_error: bool,
) -> Result<Value, PluginError> {
    if is_error
        && let Some(parts) = content.as_array()
        && parts
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) != Some("text"))
    {
        // bg/MVl 对混合类型的错误结果只留下文本，并在块之间插入两个换行。
        let mut joined = String::new();
        let mut first = true;
        for part in parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        {
            if !first {
                joined.push_str("\n\n");
            }
            joined.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
            first = false;
        }
        return Ok(json!(joined));
    }
    let Some(parts) = content.as_array() else {
        return Ok(json!(match content {
            Value::Null | Value::Bool(false) => String::new(),
            Value::Number(number) if number.as_f64() == Some(0.0) => String::new(),
            Value::String(text) => text.clone(),
            Value::Object(_) => "[object Object]".into(),
            value => value.to_string(),
        }));
    };
    let mut wire = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
                {
                    wire.push(json!({"type":"text","text":part["text"]}));
                }
            }
            Some("image") => {
                let source: MediaSource = serde_json::from_value(part["source"].clone())
                    .map_err(|_| unsupported_request())?;
                if let Some(part) = remote_image(&source, supports_vision, false)? {
                    wire.push(part);
                }
            }
            Some("image_url") => {
                let url = part["image_url"]["url"]
                    .as_str()
                    .ok_or_else(unsupported_request)?;
                if let Some(part) =
                    remote_image(&MediaSource::Url(url.into()), supports_vision, false)?
                {
                    wire.push(part);
                }
            }
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

fn native_max_tokens(value: Option<&Value>) -> u64 {
    let number = value.and_then(|value| {
        if let Some(value) = value.as_str() {
            let value = value.trim();
            if value.is_empty() {
                return None;
            }
            let radix = [
                ("0x", 16),
                ("0X", 16),
                ("0o", 8),
                ("0O", 8),
                ("0b", 2),
                ("0B", 2),
            ]
            .into_iter()
            .find_map(|(prefix, radix)| value.strip_prefix(prefix).map(|digits| (digits, radix)));
            if let Some((digits, radix)) = radix {
                return u64::from_str_radix(digits, radix)
                    .ok()
                    .map(|value| value as f64);
            }
            value.parse::<f64>().ok()
        } else {
            value.as_f64()
        }
    });
    number
        .filter(|value| value.fract() == 0.0 && *value > 0.0 && *value <= 9_007_199_254_740_991.0)
        .map(|value| value as u64)
        .unwrap_or(32_000)
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
    let requested_effort = parameters
        .remove("qoder_reasoning_effort")
        .and_then(|value| value.as_str().map(str::to_owned));
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
        .filter(|value| {
            !value.is_null()
                && value.as_bool() != Some(false)
                && value.as_str() != Some("")
                && value.as_f64() != Some(0.0)
        })
    {
        object.insert("outer_provider".into(), provider.clone());
    } else {
        object.insert("model".into(), json!(""));
    }
    if let Some(adapter) = catalog
        .and_then(|row| row.get("custom_provider_adapter"))
        .filter(|value| {
            !value.is_null()
                && value.as_bool() != Some(false)
                && value.as_str() != Some("")
                && value.as_f64() != Some(0.0)
        })
    {
        object.insert("custom_provider_adapter".into(), adapter.clone());
    }
    let limit = native_max_tokens(
        parameters
            .get("max_tokens")
            .or_else(|| catalog.and_then(|row| row.get("max_output_tokens"))),
    );
    parameters.insert("max_tokens".into(), json!(limit));
    let selected_effort = requested_effort.or_else(|| {
        (!parameters.contains_key("reasoning_effort")
            && parameters.get("enable_thinking").and_then(Value::as_bool) != Some(false))
        .then(|| {
            catalog
                .and_then(|row| row.get("default_effort"))
                .and_then(Value::as_str)
                .map(|effort| effort.trim().to_ascii_lowercase())
                .map(|effort| match effort.as_str() {
                    "off" | "disabled" => "none".into(),
                    _ => effort,
                })
                .filter(|effort| {
                    matches!(
                        effort.as_str(),
                        "none" | "low" | "medium" | "high" | "xhigh" | "max"
                    )
                })
        })
        .flatten()
    });
    if let Some(effort) = selected_effort {
        let supports_disabled = catalog
            .and_then(|row| row.get("supports_disabled"))
            .and_then(Value::as_bool)
            == Some(true);
        let efforts = catalog.and_then(|row| row.get("efforts"));
        let has_selector = efforts.is_some_and(Value::is_array) || supports_disabled;
        let has_outer_provider = catalog.is_some_and(|row| row.contains_key("outer_provider"));
        let supported_effort = efforts.is_some_and(|values| match values {
            Value::Array(values) => values
                .iter()
                .any(|value| value.as_str() == Some(effort.as_str())),
            Value::Object(values) => values.contains_key(&effort),
            Value::String(values) => values
                .split(|c: char| c == ',' || c.is_whitespace())
                .any(|value| value == effort),
            _ => false,
        }) || effort == "none" && supports_disabled;
        if !has_outer_provider && !has_selector || supported_effort {
            parameters.insert("reasoning_effort".into(), json!(effort));
        }
    }
    // 原生 effort 只在未提供激活控制时决定开关，不能推翻 SDK 的 mode/level/budget 优先级。
    let enabled = parameters
        .get("enable_thinking")
        .and_then(Value::as_bool)
        .or_else(|| {
            parameters
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .map(|effort| effort != "none")
        });
    if let Some(enabled) = enabled {
        parameters.insert("enable_thinking".into(), json!(enabled));
        if !enabled {
            parameters.remove("reasoning_budget_tokens");
            if parameters.get("reasoning_effort").and_then(Value::as_str) != Some("none") {
                parameters.insert("reasoning_effort".into(), json!("none"));
            }
        } else if parameters.get("reasoning_effort").and_then(Value::as_str) == Some("none") {
            parameters.remove("reasoning_effort");
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
    fn normal_strings_and_empty_users_follow_cli_preprocessing() {
        let endpoint = common::endpoint("anthropic").unwrap();
        let transform = ProtocolTransform::global()
            .bind(endpoint, endpoint)
            .unwrap();
        let input = |content: Value| {
            transform
                .decode_request(json!({
                    "model":"synthetic-model","max_tokens":1,
                    "messages":[{"role":"user","content":content}]
                }))
                .unwrap()
        };
        let body = project_request(&input(json!("hello")), true).unwrap();
        assert_eq!(
            body["messages"],
            json!([{
                "role":"user","content":"hello",
                "contents":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]
            }])
        );
        assert_eq!(
            project_request(&input(json!("")), true).unwrap()["messages"],
            json!([])
        );
    }

    #[test]
    fn images_follow_preconversion_and_postconversion_cache_timing() {
        let png = MediaSource::Url("https://example.invalid/image.png".into());
        assert_eq!(
            remote_image(&png, true, true).unwrap(),
            Some(json!({
                "type":"image_url","image_url":{"url":"https://example.invalid/image.png"}
            }))
        );
        let no_vision = remote_image(&png, false, true).unwrap().unwrap();
        assert!(no_vision.get("cache_control").is_none());
        let svg = MediaSource::Url("https://example.invalid/image.svg?synthetic=1".into());
        let omitted = remote_image(&svg, true, true).unwrap().unwrap();
        assert_eq!(omitted["type"], "text");
        assert_eq!(omitted["cache_control"], json!({"type":"ephemeral"}));
        let jpg = MediaSource::Base64 {
            media_type: "JPG".into(),
            data: "c3ludGhldGlj".into(),
        };
        assert_eq!(
            remote_image(&jpg, true, false).unwrap().unwrap()["image_url"]["url"],
            "data:image/jpeg;base64,c3ludGhldGlj"
        );
        assert_eq!(
            remote_result(&json!(false), true, false).unwrap(),
            json!("")
        );
        assert_eq!(remote_result(&json!(0), true, false).unwrap(), json!(""));
    }

    #[test]
    fn native_token_limits_and_catalog_effort_follow_cli_rules() {
        for value in [json!("1e3"), json!("0x3e8"), json!(1000.0)] {
            assert_eq!(native_max_tokens(Some(&value)), 1000);
        }
        for value in [json!(0), json!(-1), json!(1.5), json!(""), json!(true)] {
            assert_eq!(native_max_tokens(Some(&value)), 32_000);
        }
        let config =
            json!({"is_reasoning":true,"default_effort":"medium","efforts":["low","medium"]});
        let body = build_body(
            json!({"messages":[]}),
            "remote",
            "system",
            "synthetic-session",
            Region::Global,
            Some(&config),
        )
        .unwrap();
        assert_eq!(
            body["parameters"],
            json!({"max_tokens":32000,"reasoning_effort":"medium","enable_thinking":true})
        );
        let rejected = build_body(
            json!({"messages":[],"qoder_reasoning_effort":"high"}),
            "remote",
            "system",
            "synthetic-session",
            Region::Global,
            Some(&config),
        )
        .unwrap();
        assert_eq!(rejected["parameters"], json!({"max_tokens":32000}));
        assert!(
            rejected["parameters"]
                .get("qoder_reasoning_effort")
                .is_none()
        );
    }

    #[test]
    fn unsupported_effort_does_not_add_an_activation_override() {
        let mut request = AiRequest::new("remote", vec![]);
        request.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::High);
        let config = json!({"is_reasoning":true,"efforts":["low"]});
        let body = build_body(
            project_request(&request, false).unwrap(),
            "remote",
            "system",
            "synthetic-session",
            Region::Global,
            Some(&config),
        )
        .unwrap();
        assert_eq!(body["parameters"], json!({"max_tokens":32000}));
        assert_eq!(body["model_config"]["is_reasoning"], json!(true));
    }

    #[test]
    fn thinking_signatures_preserve_only_nonempty_proofs() {
        for signature in ["", "synthetic-signature"] {
            let mut request = AiRequest::new("remote", vec![]);
            request.items = serde_json::from_value(json!([{"role":"assistant","content":[{
                "type":"thinking","thinking":"Consider the alternatives.","signature":signature
            }]}]))
            .unwrap();
            let body = build_body(
                project_request(&request, false).unwrap(),
                "remote",
                "system",
                "synthetic-session",
                Region::Global,
                None,
            )
            .unwrap();
            assert_eq!(
                body["messages"][0]["reasoning_content"],
                "Consider the alternatives."
            );
            let expected_signature = (!signature.is_empty()).then(|| json!(signature));
            assert_eq!(
                body["messages"][0].get("reasoning_content_signature"),
                expected_signature.as_ref()
            );
        }
    }

    #[test]
    fn disabled_mode_overrides_positive_reasoning_budget() {
        let mut request = AiRequest::new("remote", vec![]);
        request.reasoning.mode = Some("disabled".into());
        request.reasoning.budget_tokens = Some(1024);
        let config = json!({"is_reasoning":true,"default_effort":"medium"});
        let body = build_body(
            project_request(&request, false).unwrap(),
            "remote",
            "system",
            "host-session",
            Region::Global,
            Some(&config),
        )
        .unwrap();
        assert_eq!(
            body["parameters"],
            json!({"max_tokens":32000,"enable_thinking":false,"reasoning_effort":"none"})
        );
        assert_eq!(body["model_config"]["is_reasoning"], json!(false));
    }

    #[test]
    fn disabled_mode_overrides_supported_high_effort() {
        let mut request = AiRequest::new("remote", vec![]);
        request.reasoning.mode = Some("disabled".into());
        request.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::High);
        let config =
            json!({"is_reasoning":true,"default_effort":"medium","efforts":["medium","high"]});
        let body = build_body(
            project_request(&request, false).unwrap(),
            "remote",
            "system",
            "host-session",
            Region::Global,
            Some(&config),
        )
        .unwrap();
        assert_eq!(
            body["parameters"],
            json!({"max_tokens":32000,"enable_thinking":false,"reasoning_effort":"none"})
        );
        assert_eq!(body["model_config"]["is_reasoning"], json!(false));
    }

    #[test]
    fn enabled_modes_override_off_level_and_zero_budget() {
        for mode in ["enabled", "auto", "adaptive"] {
            let mut request = AiRequest::new("remote", vec![]);
            request.reasoning.mode = Some(mode.into());
            request.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::Off);
            request.reasoning.budget_tokens = Some(0);
            let config = json!({"is_reasoning":true,"default_effort":"medium","efforts":["none","medium"],"supports_disabled":true});
            let body = build_body(
                project_request(&request, false).unwrap(),
                "remote",
                "system",
                "host-session",
                Region::Global,
                Some(&config),
            )
            .unwrap();
            assert_eq!(
                body["parameters"],
                json!({"max_tokens":32000,"enable_thinking":true,"reasoning_effort":"medium"})
            );
            assert_eq!(body["model_config"]["is_reasoning"], json!(true));
        }
    }

    #[test]
    fn unspecified_reasoning_keeps_catalog_default_effort() {
        for mode in [None, Some("unknown".into())] {
            let mut request = AiRequest::new("remote", vec![]);
            request.reasoning.mode = mode;
            let config =
                json!({"is_reasoning":true,"default_effort":"medium","efforts":["medium"]});
            let body = build_body(
                project_request(&request, false).unwrap(),
                "remote",
                "system",
                "host-session",
                Region::Global,
                Some(&config),
            )
            .unwrap();
            assert_eq!(
                body["parameters"],
                json!({"max_tokens":32000,"enable_thinking":true,"reasoning_effort":"medium"})
            );
            assert_eq!(body["model_config"]["is_reasoning"], json!(true));
        }
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
        let on = build_body(
            json!({"messages":[],"reasoning_effort":"high","reasoning_budget_tokens":100}),
            "remote",
            "system",
            "host-session",
            Region::Global,
            Some(&config),
        )
        .unwrap();
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
        ]), true, false)
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
