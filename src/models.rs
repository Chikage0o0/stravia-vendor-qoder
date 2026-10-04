use crate::{Region, auth, messages, protocol};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    ConfigValidationResponse, DiscoverRequest, DiscoverResponse, DiscoveredModel, ErrorKind,
    GuestHost, PluginError, ProviderSnapshot, ValidationIssue, read_http_body,
};
fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}
fn malformed(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::upstream_unknown(), message)
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 200 && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}
fn configured_ids(options: &BTreeMap<String, Value>) -> Result<BTreeSet<&str>, ()> {
    let Some(value) = options.get("model_ids") else {
        return Ok(BTreeSet::new());
    };
    let text = value.as_str().ok_or(())?;
    if text.len() > 16384 {
        return Err(());
    }
    let mut ids = BTreeSet::new();
    for id in text.lines().map(str::trim).filter(|s| !s.is_empty()) {
        if !valid_id(id) || !ids.insert(id) {
            return Err(());
        }
    }
    Ok(ids)
}
pub(crate) fn validate(options: &BTreeMap<String, Value>) -> ConfigValidationResponse {
    let mut issues = protocol::validate_client_options(options);
    if options
        .keys()
        .any(|k| !matches!(k.as_str(), "model_ids" | "machine_os" | "machine_hostname"))
    {
        issues.push(ValidationIssue {
            field: None,
            code: "unknown_option".into(),
            message: messages::invalid_options(),
        });
    }
    if configured_ids(options).is_err() {
        issues.push(ValidationIssue {
            field: Some("model_ids".into()),
            code: "invalid_model_ids".into(),
            message: messages::invalid_model_ids(),
        });
    }
    ConfigValidationResponse {
        issues,
        proposed_base_url: None,
    }
}
pub(crate) fn discover(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
    request: DiscoverRequest,
) -> Result<DiscoverResponse, PluginError> {
    if request.cursor.is_some() {
        return Err(invalid("Qoder model discovery is not paginated"));
    }
    if !validate(&provider.options).issues.is_empty() {
        return Err(invalid("invalid Qoder model options"));
    }
    let identity = auth::identity(provider, region)?;
    let ids = configured_ids(&provider.options).map_err(|_| invalid("invalid model IDs"))?;
    let mut result = if let Some(rows) = provider.operation_metadata.get("static_models") {
        let rows = rows
            .as_array()
            .ok_or_else(|| invalid("static_models must be an array"))?;
        let entries = rows
            .iter()
            .map(|row| {
                row.as_str()
                    .map(|id| json!({"key": id}))
                    .ok_or_else(|| invalid("static_models entries must be string model IDs"))
            })
            .collect::<Result<Vec<_>, PluginError>>()?;
        parse_models(&entries, "administrator")?
    } else {
        let mut http = protocol::prepare(
            region,
            &identity,
            "GET",
            "/api/v2/model/list?Encode=1",
            &[],
            None,
            None,
        )?;
        protocol::add_client_headers(
            &mut http.headers,
            &provider.options,
            &identity.machine_id,
            false,
        )?;
        let response = host.http_start(http)?;
        let status = response.status()?;
        let headers = response.headers()?;
        let bytes = read_http_body(&response, 8 * 1024 * 1024)?;
        if !(200..300).contains(&status) {
            let mut error = common::upstream_error(status, &headers, &bytes);
            error.message = format!("Qoder model discovery HTTP {status}");
            return Err(error);
        }
        let manifest = protocol::decode_server_response(&bytes)?;
        let rows = manifest
            .get("assistant")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("Qoder catalog has no assistant scene"))?;
        parse_models(rows, "account-catalog")?
    };
    if !ids.is_empty() {
        result
            .models
            .retain(|model| ids.contains(model.id.as_str()));
    }
    Ok(result)
}
fn parse_models(rows: &[Value], source: &str) -> Result<DiscoverResponse, PluginError> {
    let mut models = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        if row
            .get("enable")
            .is_some_and(|v| v == &json!(false) || v == &json!(0))
        {
            continue;
        }
        let id = row
            .get("key")
            .or_else(|| row.get("model_key"))
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("catalog model key missing"))?;
        if !valid_id(id) || !seen.insert(id) {
            return Err(malformed(
                "catalog contains invalid or duplicate model keys",
            ));
        }
        let model_source = row
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("system");
        let mut config = json!({"key":id,"display_name":row.get("display_name").or_else(||row.get("name")).and_then(Value::as_str).unwrap_or(id),"model":"","format":row.get("format").and_then(Value::as_str).unwrap_or("openai"),"is_vl":row.get("is_vl").and_then(Value::as_bool).unwrap_or(false),"is_reasoning":row.get("is_reasoning").and_then(Value::as_bool).unwrap_or(false),"api_key":"","url":"","source":model_source});
        config["max_input_tokens"] = json!(
            row.get("max_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(200_000)
        );
        if let Some(outer_provider) = row.get("outer_provider").filter(|value| !value.is_null()) {
            if let Some(config) = config.as_object_mut() {
                config.remove("model");
            }
            config["outer_provider"] = outer_provider.clone();
        }
        for field in ["max_input_tokens", "max_output_tokens"] {
            if let Some(value) = row.get(field).and_then(Value::as_u64) {
                config[field] = json!(value);
            }
        }
        let mut metadata = BTreeMap::from([
            ("qoder_catalog_source".into(), json!(source)),
            ("qoder_source".into(), json!(model_source)),
            ("qoder_model_config".into(), config),
        ]);
        if row
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("openai")
            == "openai"
        {
            metadata.insert("tool_call".into(), json!(true));
        }
        if let Some(reasoning) = row.get("is_reasoning").and_then(Value::as_bool) {
            metadata.insert("reasoning".into(), json!(reasoning));
        }
        if let Some(images) = row.get("is_vl").and_then(Value::as_bool) {
            metadata.insert("modalities".into(),json!({"input":if images {vec!["text","image"]} else {vec!["text"]},"output":["text"]}));
        }
        let mut limit = serde_json::Map::new();
        for (field, key) in [
            ("max_input_tokens", "input"),
            ("max_output_tokens", "output"),
        ] {
            if let Some(value) = row.get(field).and_then(Value::as_u64) {
                limit.insert(key.into(), json!(value));
            }
        }
        if let Some(contexts) = row.get("context_config").and_then(Value::as_object) {
            let tokens: BTreeSet<u64> = contexts
                .values()
                .filter_map(|v| v.get("token_count").and_then(Value::as_u64))
                .filter(|v| *v > 0)
                .collect();
            if let Some(max) = tokens.last() {
                limit.insert("context".into(), json!(max));
            }
        }
        if !limit.is_empty() {
            metadata.insert("limit".into(), Value::Object(limit));
        }
        let (efforts, default) = reasoning(row);
        if !efforts.is_empty() {
            metadata.insert(
                "reasoning_options".into(),
                json!([{"type":"effort","values":efforts}]),
            );
        }
        if let Some(default) = default {
            metadata.insert("reasoning_default_effort".into(), json!(default));
        }
        models.push(DiscoveredModel {
            id: id.into(),
            display_name: row
                .get("display_name")
                .or_else(|| row.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(id)
                .into(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata,
        });
    }
    Ok(DiscoverResponse {
        models,
        next_cursor: None,
    })
}
fn truth(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool) == Some(true)
}
fn reasoning(row: &Value) -> (Vec<String>, Option<String>) {
    let thinking = row
        .get("thinking_config")
        .or_else(|| row.get("thinkingConfig"));
    let enabled = thinking.and_then(|t| t.get("enabled").or_else(|| t.get("enabledConfig")));
    let names = [
        "efforts",
        "reasoning_efforts",
        "reasoningEfforts",
        "reasoning_effort_levels",
        "reasoningEffortLevels",
        "effort_level",
        "effortLevel",
        "effort_levels",
        "effortLevels",
        "supported_efforts",
        "supportedEfforts",
        "supported_effort_levels",
        "supportedEffortLevels",
        "levels",
    ];
    let effort = [Some(row), thinking, enabled]
        .into_iter()
        .flatten()
        .find_map(|scope| names.iter().find_map(|name| scope.get(*name)));
    let accepted = |s: &str| matches!(s, "none" | "low" | "medium" | "high" | "xhigh" | "max");
    let mut values = Vec::<String>::new();
    let mut default = None;
    if let Some(effort) = effort {
        match effort {
            Value::Array(array) => values.extend(
                array
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| accepted(s))
                    .map(str::to_owned),
            ),
            Value::String(text) => values.extend(
                text.split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| accepted(s))
                    .map(str::to_owned),
            ),
            Value::Object(map) => {
                for (name, config) in map {
                    if accepted(name) {
                        values.push(name.clone());
                        if truth(config.get("is_default").or_else(|| config.get("isDefault"))) {
                            default = Some(name.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if thinking
        .and_then(|t| t.get("disabled"))
        .is_some_and(|v| !v.is_null() && v != &json!(false))
        && !values.iter().any(|v| v == "none")
    {
        values.insert(0, "none".into());
    }
    let mut seen = BTreeSet::new();
    values.retain(|v| seen.insert(v.clone()));
    (values, default)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_and_nonassistant_models_cannot_be_created_from_ids() {
        let result = parse_models(
            &[
                json!({"key":"enabled","is_vl":true,"max_input_tokens":100}),
                json!({"key":"disabled","enable":0}),
            ],
            "account-catalog",
        )
        .unwrap();
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["enabled"]
        );
        assert_eq!(result.models[0].metadata["limit"]["input"], json!(100));
    }
    #[test]
    fn duplicate_ids_and_removed_options_are_invalid() {
        assert!(
            !validate(&BTreeMap::from([("model_ids".into(), json!("a\na"))]))
                .issues
                .is_empty()
        );
        assert!(
            !validate(&BTreeMap::from([(
                "auto_paid_on_rate_limit".into(),
                json!(true)
            )]))
            .issues
            .is_empty()
        );
    }
    #[test]
    fn official_thinking_map_controls_off_and_default() {
        let row = json!({"thinking_config":{"enabled":{"efforts":{"low":{"is_default":false},"medium":{"is_default":true},"high":{"is_default":false}}},"disabled":{"is_default":false}}});
        let (values, default) = reasoning(&row);
        assert!(values.contains(&"none".into()));
        assert_eq!(default.as_deref(), Some("medium"));
        let (values, _) = reasoning(
            &json!({"thinking_config":{"enabled":{"efforts":["low","high"]},"disabled":false}}),
        );
        assert_eq!(values, vec!["low", "high"]);
    }
}
