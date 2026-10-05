use crate::{Region, auth};
use rust_decimal::Decimal;
use serde_json::Value;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceResponse, ErrorKind, GuestHost, HttpRequest,
    PluginError, ProviderSnapshot, read_http_body,
};
fn malformed(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::upstream_unknown(), message)
}
fn field<'a>(value: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    value
        .get(snake)
        .or_else(|| value.get(camel))
        .filter(|v| !v.is_null())
}
pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<AllowanceResponse, PluginError> {
    let token = auth::oauth_token(provider, region)?;
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url: format!("{}/api/v2/quota/usage", region.openapi_origin()),
        headers: vec![
            ("Accept".into(), "application/json".into()),
            ("Authorization".into(), format!("Bearer {token}")),
        ],
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let bytes = read_http_body(&response, 2 * 1024 * 1024)?;
    if !(200..300).contains(&status) {
        let mut error = common::upstream_error(status, &headers, &bytes);
        error.message = format!("Qoder quota HTTP {status}");
        return Err(error);
    }
    let payload: Value =
        serde_json::from_slice(&bytes).map_err(|_| malformed("invalid quota JSON"))?;
    let expected = provider
        .credentials
        .get("uid")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("account identity missing"))?;
    parse(&payload, expected, region)
}
fn parse(
    payload: &Value,
    expected: &str,
    region: Region,
) -> Result<AllowanceResponse, PluginError> {
    if field(payload, "user_id", "userId").and_then(Value::as_str) != Some(expected) {
        return Err(malformed("quota account identity mismatch"));
    }
    let user_type = field(payload, "user_type", "userType")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| malformed("quota user type missing"))?;
    let mut allowances = Vec::new();
    for (snake, camel, key, label_cn, label_global) in [
        (
            "user_quota",
            "userQuota",
            "qoder_user_quota",
            "个人额度",
            "User quota",
        ),
        (
            "add_on_quota",
            "addOnQuota",
            "qoder_add_on_quota",
            "附加额度",
            "Add-on quota",
        ),
        (
            "org_resource_package",
            "orgResourcePackage",
            "qoder_org_quota",
            "组织共享额度",
            "Organization shared quota",
        ),
    ] {
        let row = field(payload, snake, camel).or_else(|| {
            if snake == "org_resource_package" {
                field(payload, "shared_quota", "sharedQuota")
            } else {
                None
            }
        });
        if let Some(row) = row {
            allowances.push(item(
                row,
                key,
                match region {
                    Region::Cn => label_cn,
                    Region::Global => label_global,
                },
                field(payload, "expires_at", "expiresAt"),
                region,
            )?);
        }
    }
    if let Some(packages) = field(
        payload,
        "dedicated_resource_packages",
        "dedicatedResourcePackages",
    ) {
        let packages = packages
            .as_array()
            .ok_or_else(|| malformed("invalid dedicated quota packages"))?;
        let mut seen = std::collections::BTreeSet::new();
        for row in packages {
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| malformed("dedicated quota ID missing"))?;
            if !seen.insert(id) {
                return Err(malformed("duplicate dedicated quota ID"));
            }
            allowances.push(item(
                row,
                &format!("qoder_dedicated_{id}"),
                row.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(match region {
                        Region::Cn => "专属额度",
                        Region::Global => "Dedicated quota",
                    }),
                None,
                region,
            )?);
        }
    }
    if allowances.is_empty() {
        return Err(malformed("quota response has no available quota data"));
    }
    Ok(AllowanceResponse {
        allowances,
        models: Vec::new(),
        plan_label: Some(user_type.into()),
    })
}
fn number(value: Option<&Value>) -> Result<Option<Decimal>, PluginError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(malformed("invalid quota amount")),
    };
    let number = Decimal::from_str_exact(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .map_err(|_| malformed("invalid quota amount"))?;
    if number < Decimal::ZERO {
        return Err(malformed("negative quota amount"));
    }
    Ok(Some(number))
}
fn item(
    row: &Value,
    key: &str,
    label: &str,
    expiry: Option<&Value>,
    region: Region,
) -> Result<AllowanceItem, PluginError> {
    if !row.is_object() {
        return Err(malformed("quota entry is not an object"));
    }
    let total = number(row.get("total").or_else(|| row.get("cap")))?;
    let used = number(row.get("used"))?;
    let remaining = number(row.get("remaining"))?;
    let percentage = number(row.get("percentage"))?;
    if total.is_none() && used.is_none() && remaining.is_none() && percentage.is_none() {
        return Err(malformed("quota entry has no amounts or percentage"));
    }
    let unit = row.get("unit").and_then(Value::as_str).unwrap_or("units");
    let amount = |value: Decimal| AllowanceAmount {
        value: value.normalize().to_string(),
        unit: unit.into(),
        currency: None,
    };
    let percentage = percentage
        .map(|p| {
            if p <= Decimal::ONE {
                p * Decimal::ONE_HUNDRED
            } else {
                p
            }
        })
        .or_else(|| match (used, total) {
            (Some(u), Some(t)) if t > Decimal::ZERO => u
                .checked_div(t)
                .and_then(|v| v.checked_mul(Decimal::ONE_HUNDRED)),
            _ => None,
        });
    let expires = field(row, "expires_at", "expiresAt")
        .or(expiry)
        .and_then(epoch);
    let condition = row
        .get("available")
        .and_then(Value::as_bool)
        .map(|available| {
            if available {
                "available"
            } else {
                "unavailable"
            }
            .into()
        })
        .or_else(|| row.get("status").and_then(Value::as_str).map(str::to_owned));
    Ok(AllowanceItem {
        key: key.into(),
        // 宿主摘要和明细共用 label；总量放标题，数值字段保留原始额度语义。
        label: match (region, total) {
            (Region::Cn, Some(total)) => format!("{label} · 总计 {}", total.normalize()),
            (Region::Cn, None) => format!("{label} · 总计 —"),
            (Region::Global, Some(total)) => format!("{label} · Total {}", total.normalize()),
            (Region::Global, None) => format!("{label} · Total —"),
        },
        kind: "balance".into(),
        used: used.map(amount),
        remaining: remaining.map(amount),
        limit: total.map(amount),
        used_percent: percentage.map(|p| p.round_dp(2).normalize().to_string()),
        window_seconds: None,
        resets_at_unix_ms: expires,
        condition,
    })
}
fn epoch(value: &Value) -> Option<i64> {
    if let Some(s) = value.as_str() {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp_millis())
            .or_else(|| s.parse::<i64>().ok().map(milliseconds))
    } else {
        value.as_i64().map(milliseconds)
    }
}
fn milliseconds(value: i64) -> i64 {
    if value > 100_000_000_000 {
        value
    } else {
        value.saturating_mul(1000)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn quota_unknown_is_not_zero() {
        assert!(
            parse(
                &json!({"user_id":"u","user_type":"free","user_quota":{"unit":"credits"}}),
                "u",
                Region::Cn
            )
            .is_err()
        );
    }
    #[test]
    fn quota_preserves_packages_and_fraction_percent() {
        let result = parse(&json!({"user_id":"u","user_type":"pro","user_quota":{"remaining":"12.5","percentage":0.25,"unit":"credits"},"dedicated_resource_packages":[{"id":"p","remaining":7,"expires_at":"2026-01-01T00:00:00Z","available":false}]}),"u",Region::Cn).unwrap();
        assert_eq!(
            result.allowances[0].remaining.as_ref().unwrap().value,
            "12.5"
        );
        assert_eq!(result.allowances[0].used_percent.as_deref(), Some("25"));
        assert_eq!(result.allowances[1].resets_at_unix_ms, Some(1767225600000));
        assert_eq!(
            result.allowances[1].condition.as_deref(),
            Some("unavailable")
        );
    }
}
