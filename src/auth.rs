//! Qoder 原生设备 OAuth 与持久化 COSY 身份。
use crate::{Region, VENDOR_ID, protocol};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, GuestHost, HttpRequest, PluginError,
    ProviderSnapshot, read_http_body,
};
const MAX_BODY: usize = 256 * 1024;
#[derive(Serialize, Deserialize)]
struct Pending {
    region: String,
    nonce: String,
    verifier: String,
    machine_id: String,
    deadline: i64,
    interval: u32,
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn error(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Auth, message)
}
fn malformed(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::upstream_unknown(), message)
}
pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start { .. } => start(host, provider, region),
        AuthStep::Poll => poll(host, provider, region),
        AuthStep::Refresh => refresh(host, provider, region),
        AuthStep::Revoke => {
            host.write_private_state(b"")?;
            Ok(AuthResponse::Revoked)
        }
        _ => Err(common::unsupported(
            "auth exchange/manual-input",
            VENDOR_ID,
            region.id(),
        )),
    }
}
fn start(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<AuthResponse, PluginError> {
    let machine_id = match provider.credentials.get("machine_id") {
        Some(Value::String(id)) if uuid::Uuid::parse_str(id).is_ok() => id.clone(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    let mut entropy = [0u8; 48];
    getrandom::fill(&mut entropy).map_err(|_| error("secure device login entropy unavailable"))?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(entropy);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let pending = Pending {
        region: region.id().into(),
        nonce: uuid::Uuid::new_v4().to_string(),
        verifier,
        machine_id,
        deadline: now() + 300_000,
        interval: 1,
    };
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("challenge", &challenge)
        .append_pair("challenge_method", "S256")
        .append_pair("nonce", &pending.nonce)
        .append_pair("machine_id", &pending.machine_id)
        .append_pair("client_id", "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb")
        .finish();
    let url = format!("{}/device/selectAccounts?{query}", region.website());
    write_pending(host, &pending)?;
    Ok(AuthResponse::Authorization {
        url: url.clone(),
        state: None,
        user_code: None,
        verification_uri: Some(url),
        interval_seconds: Some(1),
    })
}
fn write_pending(host: &GuestHost, pending: &Pending) -> Result<(), PluginError> {
    let mut root = crate::state::read(host)?;
    root.insert(
        "device_login".into(),
        serde_json::to_value(pending).map_err(|_| error("could not save device login"))?,
    );
    crate::state::write(host, root)
}
fn clear_pending(host: &GuestHost) -> Result<(), PluginError> {
    let mut root = crate::state::read(host)?;
    root.remove("device_login");
    crate::state::write(host, root)
}
fn poll(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<AuthResponse, PluginError> {
    let root = crate::state::read(host)?;
    let pending: Pending = serde_json::from_value(
        root.get("device_login")
            .cloned()
            .ok_or_else(|| error("device login session missing"))?,
    )
    .map_err(|_| error("invalid device login session"))?;
    if pending.region != region.id() {
        return Err(error("device login belongs to another region"));
    }
    if now() >= pending.deadline {
        clear_pending(host)?;
        return Err(error("device login expired"));
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("nonce", &pending.nonce)
        .append_pair("verifier", &pending.verifier)
        .append_pair("challenge_method", "S256")
        .finish();
    let (status, data) = request(
        host,
        "GET",
        format!(
            "{}/api/v1/deviceToken/poll?{query}",
            region.openapi_origin()
        ),
        None,
        Vec::new(),
        true,
    )?;
    if status == 404 {
        return Ok(AuthResponse::Pending {
            retry_after_seconds: Some(pending.interval),
        });
    }
    if !(200..300).contains(&status) {
        return Err(error("device authorization request failed"));
    }
    let Some(_) = data
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    else {
        return Ok(AuthResponse::Pending {
            retry_after_seconds: Some(pending.interval),
        });
    };
    let result = credentials(
        host,
        region,
        &data,
        &pending.machine_id,
        None,
        data.get("user_id").and_then(Value::as_str),
        &provider.options,
    )?;
    clear_pending(host)?;
    Ok(result)
}
fn refresh(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<AuthResponse, PluginError> {
    let previous = identity(provider, region)?;
    let refresh = provider
        .credentials
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error("refresh token missing"))?;
    safe(refresh)?;
    let body =
        serde_json::to_vec(&json!({"refresh_token":refresh,"machine_id":previous.machine_id}))
            .map_err(|_| malformed("cannot encode refresh request"))?;
    let (_, data) = request(
        host,
        "POST",
        format!("{}/api/v1/deviceToken/refresh", region.openapi_origin()),
        None,
        body,
        false,
    )?;
    credentials(
        host,
        region,
        &data,
        &previous.machine_id,
        Some(provider),
        Some(&previous.uid),
        &provider.options,
    )
}
fn credentials(
    host: &GuestHost,
    region: Region,
    data: &Value,
    machine: &str,
    old: Option<&ProviderSnapshot>,
    expected: Option<&str>,
    options: &BTreeMap<String, Value>,
) -> Result<AuthResponse, PluginError> {
    let token = required(
        data,
        if old.is_some() {
            "device_token"
        } else {
            "token"
        },
    )?;
    safe(token)?;
    let (_, profile) = request(
        host,
        "GET",
        format!("{}/api/v1/userinfo", region.openapi_origin()),
        Some(token),
        Vec::new(),
        false,
    )?;
    // 官方 userinfo 的 canonical ID 优先于 uid 别名；两者可能同时存在。
    let uid = ["id", "user_id", "uid"]
        .iter()
        .find_map(|key| {
            profile
                .get(*key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| malformed("userinfo has no account identity"))?;
    safe(uid)?;
    if expected.is_some_and(|expected| expected != uid) {
        return Err(error("OAuth credential changed account identity"));
    }
    let org = profile
        .get("organization_id")
        .or_else(|| profile.get("orgId"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !org.is_empty() {
        safe(org)?;
    }
    let tags: Vec<String> = if org.is_empty() {
        Vec::new()
    } else {
        let escaped: String = url::form_urlencoded::byte_serialize(org.as_bytes()).collect();
        let (_, payload) = request(
            host,
            "GET",
            format!(
                "{}/api/v1/organizations/{escaped}/tags",
                region.openapi_origin()
            ),
            Some(token),
            Vec::new(),
            false,
        )?;
        let array = payload
            .get("tags")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("organization tags missing"))?;
        array
            .iter()
            .map(|v| {
                let tag = v
                    .as_str()
                    .ok_or_else(|| malformed("invalid organization tag"))?;
                safe(tag)?;
                if tag.contains(',') {
                    return Err(error("invalid organization tag"));
                }
                Ok(tag.to_owned())
            })
            .collect::<Result<_, PluginError>>()?
    };
    let provisional = protocol::Identity::from_token(uid, token, machine, org, &tags, false)?;
    let path = format!(
        "/api/v2/config/getDataPolicy?requestId={}&version=2",
        uuid::Uuid::new_v4()
    );
    let mut http = protocol::prepare(region, &provisional, "GET", &path, &[], None, None)?;
    protocol::add_client_headers(&mut http.headers, options, machine, false)?;
    let policy_response = host.http_start(http)?;
    let status = policy_response.status()?;
    let bytes = read_http_body(&policy_response, MAX_BODY)?;
    if !(200..300).contains(&status) {
        return Err(malformed("data policy request failed"));
    }
    let policy = protocol::decode_server_response(&bytes)?;
    let agreed = match policy.pointer("/result/status").and_then(Value::as_str) {
        Some("AGREE" | "NO_RECORD") => true,
        Some("DISAGREE") => false,
        _ => return Err(malformed("data policy status missing or unknown")),
    };
    let identity = protocol::Identity::from_token(uid, token, machine, org, &tags, agreed)?;
    let expires_at_unix_ms = expiry(data, "expires_at", "expires_in")
        .or_else(|| old.and_then(|p| p.credentials.get("expire_time").and_then(Value::as_i64)));
    let refresh_expiry = expiry(data, "refresh_token_expires_at", "refresh_token_expires_in")
        .or_else(|| {
            old.and_then(|p| {
                p.credentials
                    .get("refresh_token_expire_time")
                    .and_then(Value::as_i64)
            })
        });
    let refresh = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| old.and_then(|p| p.credentials.get("refresh_token").and_then(Value::as_str)));
    if let Some(refresh) = refresh {
        safe(refresh)?;
    }
    let mut values = BTreeMap::from([
        ("region".into(), json!(region.id())),
        ("uid".into(), json!(uid)),
        ("subject_id".into(), json!(uid)),
        ("access_token".into(), json!(token)),
        ("machine_id".into(), json!(machine)),
        ("organization_id".into(), json!(org)),
        ("organization_tags".into(), json!(tags)),
        ("data_policy_agreed".into(), json!(agreed)),
        ("login_method".into(), json!("browser")),
        (
            "identity".into(),
            serde_json::to_value(identity).map_err(|_| malformed("cannot save identity"))?,
        ),
    ]);
    if let Some(token) = refresh {
        values.insert("refresh_token".into(), json!(token));
    }
    if let Some(expiry) = expires_at_unix_ms {
        values.insert("expire_time".into(), json!(expiry));
    }
    if let Some(expiry) = refresh_expiry {
        values.insert("refresh_token_expire_time".into(), json!(expiry));
    }
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    })
}
pub(crate) fn identity(
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<protocol::Identity, PluginError> {
    let values = &provider.credentials;
    if values.get("region").and_then(Value::as_str) != Some(region.id()) {
        return Err(error("credential belongs to another region"));
    }
    let identity: protocol::Identity = serde_json::from_value(
        values
            .get("identity")
            .cloned()
            .ok_or_else(|| error("Qoder identity missing"))?,
    )
    .map_err(|_| error("Qoder identity malformed"))?;
    if values.get("uid").and_then(Value::as_str) != Some(identity.uid.as_str())
        || values.get("subject_id").and_then(Value::as_str) != Some(identity.uid.as_str())
        || values.get("machine_id").and_then(Value::as_str) != Some(identity.machine_id.as_str())
        || values.get("organization_id").and_then(Value::as_str)
            != Some(identity.organization_id.as_str())
        || values.get("organization_tags") != Some(&json!(identity.organization_tags))
        || values.get("data_policy_agreed").and_then(Value::as_bool)
            != Some(identity.data_policy_agreed)
    {
        return Err(error("credential account identity is inconsistent"));
    }
    if uuid::Uuid::parse_str(&identity.machine_id).is_err() {
        return Err(error("invalid machine identity"));
    }
    for field in [
        &identity.uid,
        &identity.machine_id,
        &identity.encrypt_user_info,
        &identity.key,
    ] {
        safe(field)?;
    }
    if !identity.organization_id.is_empty() {
        safe(&identity.organization_id)?;
    }
    for tag in &identity.organization_tags {
        safe(tag)?;
        if tag.contains(',') {
            return Err(error("invalid organization tag"));
        }
    }
    let token = values
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| error("OAuth token missing"))?;
    safe(token)?;
    Ok(identity)
}
pub(crate) fn oauth_token(
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<&str, PluginError> {
    identity(provider, region)?;
    provider
        .credentials
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| error("OAuth token missing"))
}
fn safe(value: &str) -> Result<(), PluginError> {
    if value.is_empty() || !value.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        Err(error("credential contains unsafe header characters"))
    } else {
        Ok(())
    }
}
fn required<'a>(data: &'a Value, key: &str) -> Result<&'a str, PluginError> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| malformed("OAuth response missing required field"))
}
fn expiry(data: &Value, absolute: &str, relative: &str) -> Option<i64> {
    data.get(absolute)
        .and_then(|v| {
            if let Some(s) = v.as_str() {
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|t| t.timestamp_millis())
                    .or_else(|| s.parse::<i64>().ok().map(epoch))
            } else {
                v.as_i64().map(epoch)
            }
        })
        .or_else(|| {
            data.get(relative)
                .and_then(Value::as_i64)
                .filter(|s| *s >= 0)
                .and_then(|s| s.checked_mul(1000))
                .and_then(|ms| now().checked_add(ms))
        })
}
fn epoch(value: i64) -> i64 {
    if value > 100_000_000_000 {
        value
    } else {
        value.saturating_mul(1000)
    }
}
fn request(
    host: &GuestHost,
    method: &str,
    url: String,
    token: Option<&str>,
    body: Vec<u8>,
    allow_oauth_error: bool,
) -> Result<(u16, Value), PluginError> {
    let mut headers = vec![
        ("Accept".into(), "application/json".into()),
        (
            "User-Agent".into(),
            format!("qoder/{}", protocol::CLI_VERSION),
        ),
    ];
    if method == "POST" {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    if let Some(token) = token {
        safe(token)?;
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    }
    let response = host.http_start(HttpRequest {
        method: method.into(),
        url,
        headers,
        body,
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let bytes = read_http_body(&response, MAX_BODY)?;
    if allow_oauth_error && status == 404 {
        return Ok((status, Value::Null));
    }
    if !(200..300).contains(&status) {
        let mut error = common::upstream_error(status, &response_headers, &bytes);
        error.message = format!("Qoder OAuth HTTP {status}");
        return Err(error);
    }
    let data = serde_json::from_slice(&bytes).map_err(|_| malformed("invalid OAuth JSON"))?;
    Ok((status, data))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiry_accepts_native_iso_and_relative() {
        assert_eq!(
            expiry(
                &json!({"expires_at":"2026-01-01T00:00:00Z"}),
                "expires_at",
                "expires_in"
            ),
            Some(1767225600000)
        );
        assert_eq!(
            expiry(
                &json!({"expires_at":1767225600}),
                "expires_at",
                "expires_in"
            ),
            Some(1767225600000)
        );
    }
    #[test]
    fn credentials_reject_header_injection() {
        assert!(safe("token\r\nInjected: yes").is_err());
        assert!(safe("synthetic-oauth").is_ok());
    }
}
