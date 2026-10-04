use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use base64::{Engine, engine::general_purpose::STANDARD};
use md5::{Digest, Md5};
use rsa::{BigUint, RsaPublicKey, pkcs8::DecodePublicKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, HttpRequest, PluginError, ValidationIssue};
use zeroize::Zeroizing;

use crate::Region;

pub(crate) const CLI_VERSION: &str = "1.1.65";
const MACHINE_OS_VALUES: [&str; 6] = [
    "x86_64_win32",
    "aarch64_win32",
    "x86_64_linux",
    "aarch64_linux",
    "x86_64_darwin",
    "aarch64_darwin",
];

fn client_option<'a>(
    options: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, PluginError> {
    options
        .get(key)
        .map(|value| {
            value
                .as_str()
                .filter(|value| {
                    key != "machine_hostname"
                        || (value.len() <= 4096 && !value.contains(['\r', '\n']))
                })
                .ok_or_else(|| {
                    common::plugin_error(
                        ErrorKind::Invalid,
                        "Qoder client identity option must be a string",
                    )
                })
        })
        .transpose()
}

fn machine_os(options: &BTreeMap<String, Value>) -> Result<&str, PluginError> {
    let value = client_option(options, "machine_os")?.unwrap_or(MACHINE_OS_VALUES[0]);
    if MACHINE_OS_VALUES.contains(&value) {
        Ok(value)
    } else {
        Err(common::plugin_error(
            ErrorKind::Invalid,
            "Invalid Qoder machine OS option",
        ))
    }
}

fn header_safe_hostname(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        && value.as_bytes()[0] != b' '
        && value.as_bytes()[value.len() - 1] != b' '
}

fn hostname_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

fn limit_hostname(value: String) -> String {
    if value.len() <= 96 {
        return value;
    }
    let prefix = value[..87].trim_end_matches(['-', ' ']);
    let hash = hostname_hash(&value);
    if prefix.is_empty() {
        format!("unknown-{hash}")
    } else {
        format!("{prefix}-{hash}")
    }
}

fn normalize_hostname(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    if header_safe_hostname(value) {
        return limit_hostname(value.to_owned());
    }
    // 与 Node domainToASCII 一样使用 WHATWG IDNA；IPv6 不作为域名结果处理。
    let domain_value = if value.contains('\t') {
        std::borrow::Cow::Owned(value.replace('\t', ""))
    } else {
        std::borrow::Cow::Borrowed(value)
    };
    let domain = domain_value
        .split(['/', '\\', '?', '#'])
        .next()
        .unwrap_or(&domain_value);
    if let Ok(host) = url::Host::parse(domain) {
        let ascii = match host {
            url::Host::Domain(domain) => Some(domain),
            url::Host::Ipv4(address) => Some(address.to_string()),
            url::Host::Ipv6(_) => None,
        };
        if let Some(ascii) = ascii.filter(|value| header_safe_hostname(value)) {
            return limit_hostname(ascii);
        }
    }
    let mut normalized = String::with_capacity(value.len());
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii() && ('!'..='~').contains(&character) && character != '-' {
            if separator && !normalized.is_empty() {
                normalized.push('-');
            }
            normalized.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    let hash = hostname_hash(value);
    limit_hostname(if normalized.is_empty() {
        format!("unknown-{hash}")
    } else {
        format!("{normalized}-{hash}")
    })
}

pub(crate) fn validate_client_options(options: &BTreeMap<String, Value>) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if machine_os(options).is_err() {
        issues.push(ValidationIssue {
            field: Some("machine_os".into()),
            code: "invalid_machine_os".into(),
            message: crate::messages::invalid_machine_os(),
        });
    }
    if client_option(options, "machine_hostname").is_err() {
        issues.push(ValidationIssue {
            field: Some("machine_hostname".into()),
            code: "invalid_machine_hostname".into(),
            message: crate::messages::invalid_machine_hostname(),
        });
    }
    issues
}

pub(crate) fn add_client_headers(
    headers: &mut Vec<(String, String)>,
    options: &BTreeMap<String, Value>,
    machine_id: &str,
    inference: bool,
) -> Result<(), PluginError> {
    let os = machine_os(options)?;
    let configured_hostname = client_option(options, "machine_hostname")?.unwrap_or("");
    // 这是配置的客户端身份，不是通过 WASI 读取的真实宿主身份。
    let hostname = if inference {
        let normalized = normalize_hostname(configured_hostname);
        Some(if normalized.is_empty() {
            let suffix: String = machine_id
                .chars()
                .filter(|character| character.is_ascii_hexdigit())
                .take(12)
                .collect();
            format!("stravia-{suffix}")
        } else {
            normalized
        })
    } else {
        None
    };
    headers.retain(|(name, _)| {
        !name.eq_ignore_ascii_case("Cosy-MachineOS")
            && !name.eq_ignore_ascii_case("Cosy-MachineHostname")
    });
    headers.push(("Cosy-MachineOS".into(), os.to_owned()));
    if let Some(hostname) = hostname {
        headers.push(("Cosy-MachineHostname".into(), hostname));
    }
    Ok(())
}
const INFER_PATH: &str = "/api/v2/service/pro/sse/agent_chat_generation";
const STANDARD_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BODY_ALPHABET: &[u8; 64] =
    b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
const BODY_INVERSE: [u8; 256] = {
    let mut inverse = [u8::MAX; 256];
    let mut index = 0;
    while index < BODY_ALPHABET.len() {
        inverse[BODY_ALPHABET[index] as usize] = STANDARD_ALPHABET[index];
        index += 1;
    }
    inverse[b'$' as usize] = b'=';
    inverse
};
static PARSED_PUBLIC_KEY: OnceLock<Result<RsaPublicKey, ()>> = OnceLock::new();
// 来自官方 Qoder CLI 1.1.65 发布包的公开互操作密钥。
const RUNTIME_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc\n4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l\n6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17\nXcW+ML9FoCI6AOvOzwIDAQAB\n-----END PUBLIC KEY-----";

#[derive(Serialize, Deserialize)]
pub(crate) struct Identity {
    pub(crate) uid: String,
    pub(crate) machine_id: String,
    pub(crate) organization_id: String,
    pub(crate) organization_tags: Vec<String>,
    pub(crate) data_policy_agreed: bool,
    pub(crate) encrypt_user_info: String,
    pub(crate) key: String,
}

impl Identity {
    pub(crate) fn from_token(
        uid: &str,
        token: &str,
        machine_id: &str,
        organization_id: &str,
        tags: &[String],
        data_policy_agreed: bool,
    ) -> Result<Self, PluginError> {
        #[derive(Serialize)]
        struct RuntimeInfo<'a> {
            uid: &'a str,
            security_oauth_token: &'a str,
            organization_id: &'a str,
            organization_tags: &'a [String],
            data_policy_agreed: bool,
        }
        let raw = Zeroizing::new(
            serde_json::to_vec(&RuntimeInfo {
                uid,
                security_oauth_token: token,
                organization_id,
                organization_tags: tags,
                data_policy_agreed,
            })
            .map_err(|_| failure("Qoder runtime identity serialization failed"))?,
        );
        let (encrypt_user_info, key) = runtime_fields(&raw, secure_fill)?;
        Ok(Self {
            uid: uid.to_owned(),
            machine_id: machine_id.to_owned(),
            organization_id: organization_id.to_owned(),
            organization_tags: tags.to_vec(),
            data_policy_agreed,
            encrypt_user_info,
            key,
        })
    }
}

fn secure_fill(bytes: &mut [u8]) -> Result<(), PluginError> {
    getrandom::fill(bytes).map_err(|_| failure("Qoder secure randomness unavailable"))
}

fn masked_uuid_bytes(mut random: [u8; 16]) -> [u8; 16] {
    random.reverse();
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    random
}

fn uuid(random: [u8; 16]) -> String {
    let bytes = masked_uuid_bytes(random);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

fn runtime_fields(
    raw: &[u8],
    mut fill: impl FnMut(&mut [u8]) -> Result<(), PluginError>,
) -> Result<(String, String), PluginError> {
    let mut random = Zeroizing::new([0; 16]);
    fill(random.as_mut())?;
    let masked = Zeroizing::new(masked_uuid_bytes(*random));
    let mut key = Zeroizing::new([0u8; 16]);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in masked[..8].iter().enumerate() {
        key[2 * index] = HEX[(byte >> 4) as usize];
        key[2 * index + 1] = HEX[(byte & 15) as usize];
    }
    let cipher = cbc::Encryptor::<aes::Aes128>::new((&*key).into(), (&*key).into());
    let encrypted = Zeroizing::new(cipher.encrypt_padded_vec_mut::<Pkcs7>(raw));
    let encrypt_user_info = STANDARD.encode(&*encrypted);

    // 显式构造 PKCS#1 v1.5 填充，使 WASI Preview 2 上的熵错误仍通过 Result 返回；
    // 公钥运算由 RustCrypto RSA 提供，固定公钥只解析一次。
    let mut block = Zeroizing::new([0u8; 128]);
    block[1] = 2;
    fill(&mut block[2..111])?;
    for byte in &mut block[2..111] {
        while *byte == 0 {
            fill(std::slice::from_mut(byte))?;
        }
    }
    block[112..].copy_from_slice(&*key);
    let public_key = PARSED_PUBLIC_KEY
        .get_or_init(|| RsaPublicKey::from_public_key_pem(RUNTIME_PUBLIC_KEY).map_err(|_| ()))
        .as_ref()
        .map_err(|_| failure("Qoder runtime public key is invalid"))?;
    let message = Zeroizing::new(BigUint::from_bytes_be(&*block));
    let ciphertext = rsa::hazmat::rsa_encrypt(public_key, &message)
        .map_err(|_| failure("Qoder runtime key encryption failed"))?
        .to_bytes_be();
    if ciphertext.len() > 128 {
        return Err(failure("Qoder runtime RSA ciphertext length is invalid"));
    }
    let mut padded = [0u8; 128];
    padded[128 - ciphertext.len()..].copy_from_slice(&ciphertext);
    Ok((encrypt_user_info, STANDARD.encode(padded)))
}

pub(crate) fn encode_body(raw: &[u8]) -> String {
    let mut encoded = STANDARD.encode(raw).into_bytes();
    for byte in &mut encoded {
        *byte = match *byte {
            b'A'..=b'Z' => BODY_ALPHABET[(*byte - b'A') as usize],
            b'a'..=b'z' => BODY_ALPHABET[(*byte - b'a' + 26) as usize],
            b'0'..=b'9' => BODY_ALPHABET[(*byte - b'0' + 52) as usize],
            b'+' => BODY_ALPHABET[62],
            b'/' => BODY_ALPHABET[63],
            b'=' => b'$',
            _ => unreachable!("Base64 alphabet"),
        };
    }
    swap_outer_thirds(&mut encoded);
    // 两套字母表均为 ASCII，原位映射不改变 UTF-8 有效性。
    String::from_utf8(encoded).expect("Qoder body alphabet is ASCII")
}

fn swap_outer_thirds(bytes: &mut [u8]) {
    let third = bytes.len() / 3;
    let last = bytes.len() - third;
    for index in 0..third {
        bytes.swap(index, last + index);
    }
}

pub(crate) fn decode_body(encoded: &[u8]) -> Result<Vec<u8>, PluginError> {
    let mut bytes = encoded.to_vec();
    swap_outer_thirds(&mut bytes);
    for byte in &mut bytes {
        let decoded = BODY_INVERSE[*byte as usize];
        if decoded == u8::MAX {
            return Err(failure(
                "Qoder encoded response contains an invalid character",
            ));
        }
        *byte = decoded;
    }
    STANDARD
        .decode(bytes)
        .map_err(|_| failure("Qoder encoded response has invalid Base64"))
}

pub(crate) fn decode_server_response(raw: &[u8]) -> Result<Value, PluginError> {
    match decode_body(raw) {
        Ok(decoded) => serde_json::from_slice(&decoded)
            .map_err(|_| failure("Qoder decoded upstream response is not valid JSON")),
        Err(_) => serde_json::from_slice(raw)
            .map_err(|_| failure("Qoder upstream response is not valid encoded or plain JSON")),
    }
}

pub(crate) fn prepare(
    region: Region,
    identity: &Identity,
    method: &str,
    path: &str,
    raw_body: &[u8],
    model_key: Option<&str>,
    model_source: Option<&str>,
) -> Result<HttpRequest, PluginError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| failure("Qoder signing clock precedes Unix epoch"))?
        .as_secs();
    let mut random = [0u8; 16];
    secure_fill(&mut random)?;
    prepare_at(
        region,
        identity,
        method,
        path,
        raw_body,
        model_key,
        model_source,
        now,
        random,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_at(
    region: Region,
    identity: &Identity,
    method: &str,
    path: &str,
    raw_body: &[u8],
    model_key: Option<&str>,
    model_source: Option<&str>,
    now: u64,
    random: [u8; 16],
) -> Result<HttpRequest, PluginError> {
    let signed_path = path.split('?').next().unwrap_or(path);
    if !signed_path.starts_with("/api/v2/") || path.contains(['\r', '\n', '#']) {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Invalid Qoder signed request path",
        ));
    }
    if method != "GET" && method != "POST" {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Unsupported Qoder signed request method",
        ));
    }
    if method == "GET" && !raw_body.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Qoder signed GET must not have a body",
        ));
    }
    let inference = signed_path == INFER_PATH;
    if inference && method != "POST" {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Qoder inference requires POST",
        ));
    }
    let encoded_body = if method == "GET" {
        String::new()
    } else {
        encode_body(raw_body)
    };
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload<'a> {
        version: &'a str,
        request_id: String,
        info: &'a str,
        cosy_version: &'a str,
        ide_version: &'a str,
    }
    let payload = STANDARD.encode(
        serde_json::to_vec(&Payload {
            version: "v1",
            request_id: uuid(random),
            info: &identity.encrypt_user_info,
            cosy_version: CLI_VERSION,
            ide_version: "",
        })
        .map_err(|_| failure("Qoder signing payload serialization failed"))?,
    );
    let date = now.to_string();
    let mut digest = Md5::new();
    digest.update(payload.as_bytes());
    for part in [
        identity.key.as_str(),
        date.as_str(),
        encoded_body.as_str(),
        signed_path,
    ] {
        digest.update(b"\n");
        digest.update(part.as_bytes());
    }
    let authorization = format!("Bearer COSY.{payload}.{:x}", digest.finalize());
    let mut headers = vec![
        (
            "Accept".into(),
            if inference {
                "text/event-stream"
            } else {
                "application/json"
            }
            .into(),
        ),
        ("Authorization".into(), authorization),
        ("Content-Type".into(), "application/json".into()),
        ("Cosy-Business-Product".into(), "cli".into()),
        ("Cosy-Business-Type".into(), "agent".into()),
        ("Cosy-ClientType".into(), "5".into()),
        (
            "Cosy-Data-Policy".into(),
            if identity.data_policy_agreed {
                "agree"
            } else {
                "disagree"
            }
            .into(),
        ),
        ("Cosy-Date".into(), date),
        ("Cosy-Key".into(), identity.key.clone()),
        ("Cosy-MachineId".into(), identity.machine_id.clone()),
        ("Cosy-Scene".into(), "assistant".into()),
        ("Cosy-User".into(), identity.uid.clone()),
        ("Cosy-Version".into(), CLI_VERSION.into()),
        ("Login-Version".into(), "v2".into()),
    ];
    if !identity.organization_id.is_empty() {
        headers.push((
            "Cosy-Organization-Id".into(),
            identity.organization_id.clone(),
        ));
    }
    if !identity.organization_tags.is_empty() {
        headers.push((
            "Cosy-Organization-Tags".into(),
            identity.organization_tags.join(","),
        ));
    }
    if inference {
        headers.push(("Cache-Control".into(), "no-cache".into()));
        headers.push(("Connection".into(), "keep-alive".into()));
        // 官方 1.1.65 JS 仅在真实 UMID 可用时保留机器令牌及类型；
        // 原生 HTTP 不伪造 UMID，因此省略 WASM 默认生成的这两个头。
        if let Some(key) = model_key.filter(|key| !key.is_empty()) {
            headers.push(("X-Model-Key".into(), key.to_owned()));
            headers.push((
                "X-Model-Source".into(),
                model_source.unwrap_or("").to_owned(),
            ));
        }
    } else {
        headers.push(("Accept-Encoding".into(), "identity".into()));
        headers.push(("Cosy-ClientIp".into(), identity.machine_id.clone()));
        headers.push(("Cosy-MachineToken".into(), identity.machine_id.clone()));
        headers.push(("Cosy-MachineType".into(), "5".into()));
    }
    let origin = if signed_path == "/api/v2/config/getDataPolicy" {
        region.center_origin()
    } else {
        region.infer_origin()
    };
    Ok(HttpRequest {
        method: method.to_owned(),
        url: format!("{origin}/algo{path}"),
        headers,
        body: encoded_body.into_bytes(),
    })
}

fn failure(message: &'static str) -> PluginError {
    common::plugin_error(ErrorKind::upstream_unknown(), message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_hostnames_follow_official_cli_normalization() {
        // 全合成输入经官方 kAi/AAn/bAi 与 Node domainToASCII 离线执行。
        for (configured, expected) in [
            ("é.com", "xn--9ca.com"),
            ("é.com/path", "xn--9ca.com"),
            ("é.com:80", ".com:80-3681cf20"),
            ("é\t.com", "xn--9ca.com"),
            ("猫 😀", "unknown-6fb7de86"),
            ("  configured-host  ", "configured-host"),
        ] {
            let options = BTreeMap::from([
                ("machine_os".into(), serde_json::json!("aarch64_linux")),
                ("machine_hostname".into(), serde_json::json!(configured)),
            ]);
            let mut headers = vec![("Authorization".into(), "synthetic-signature".into())];
            add_client_headers(&mut headers, &options, "synthetic-machine", true).unwrap();
            assert!(
                headers
                    .iter()
                    .any(|(name, value)| name == "Cosy-MachineHostname" && value == expected)
            );
            assert!(
                headers
                    .iter()
                    .any(|(name, value)| name == "Cosy-MachineOS" && value == "aarch64_linux")
            );
            assert_eq!(headers[0].1, "synthetic-signature");
        }
        let options =
            BTreeMap::from([("machine_hostname".into(), serde_json::json!("a".repeat(97)))]);
        let mut headers = Vec::new();
        add_client_headers(&mut headers, &options, "synthetic-machine", true).unwrap();
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "Cosy-MachineHostname"
                    && value == &format!("{}-2a2c60fe", "a".repeat(87)))
        );
    }

    #[test]
    fn client_identity_rejects_header_injection_and_invalid_platforms() {
        for options in [
            BTreeMap::from([(
                "machine_hostname".into(),
                serde_json::json!("safe\r\nInjected: yes"),
            )]),
            BTreeMap::from([(
                "machine_os".into(),
                serde_json::json!("unsupported-platform"),
            )]),
            BTreeMap::from([("machine_hostname".into(), serde_json::json!(false))]),
            BTreeMap::from([(
                "machine_hostname".into(),
                serde_json::json!("a".repeat(4097)),
            )]),
        ] {
            assert!(!validate_client_options(&options).is_empty());
            assert!(
                add_client_headers(&mut Vec::new(), &options, "synthetic-machine", true).is_err()
            );
        }
    }

    fn identity() -> Identity {
        Identity {
            uid: "synthetic".into(),
            machine_id: "00000000-0000-4000-8000-000000000000".into(),
            organization_id: String::new(),
            organization_tags: Vec::new(),
            data_policy_agreed: false,
            encrypt_user_info: "synthetic-info".into(),
            key: "synthetic-key".into(),
        }
    }

    fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    // 以下向量来自官方 1.1.65 认证 WASM，仅使用合成身份及确定性熵。
    #[test]
    fn official_runtime_aes_and_rsa_vector() {
        let raw = br#"{"uid":"synthetic","security_oauth_token":"synthetic-token","organization_id":"","organization_tags":[],"data_policy_agreed":false}"#;
        let (encrypted, key) = runtime_fields(raw, |bytes| {
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = (index % 255 + 1) as u8;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            encrypted,
            "A2MBsulMvdx5p3X6li/3grgWPRhkKQ4jyXSxhazxnyUe6u87QphZ5pPgiXjlrmRU2e+Z2Cbr/pfKvG+MzhuDUup9S/9Wv+p7ATZjAjBvhfb/O8JSeod1WyNSwByiFbLem4OO/bhuBO2dtlyLnfGzCKkGIUOFUVPCWCOGTYYC2GN3uAWPDtNwGKACqbJkHBOV"
        );
        assert_eq!(
            key,
            "it6UfJgxM9leZzqV0iX7CjPAMGABJvhB2PBiDFoeltL9vimnsA5sVjF5QWPNo3QP9OB6WeBFYkjeL21TjTngLJL/GEthtr6+FKiqzKt6eR0Ypt4mfUSpNMJr4W60YZIveYj4bKQ+pu5mgF9pYQBfgR/62WXONOaSeiUV6z8C3zQ="
        );
    }

    #[test]
    fn official_request_signatures_strip_query_and_sign_encoded_body() {
        let entropy = std::array::from_fn(|index| (index + 1) as u8);
        let model = prepare_at(
            Region::Global,
            &identity(),
            "GET",
            "/api/v2/model/list?Encode=1",
            &[],
            None,
            None,
            1_700_000_000,
            entropy,
        )
        .unwrap();
        let infer = prepare_at(
            Region::Global,
            &identity(),
            "POST",
            &format!("{INFER_PATH}?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"),
            b"{}",
            Some("auto"),
            Some("system"),
            1_700_000_000,
            entropy,
        )
        .unwrap();
        let payload = "eyJ2ZXJzaW9uIjoidjEiLCJyZXF1ZXN0SWQiOiIxMDBmMGUwZC0wYzBiLTRhMDktODgwNy0wNjA1MDQwMzAyMDEiLCJpbmZvIjoic3ludGhldGljLWluZm8iLCJjb3N5VmVyc2lvbiI6IjEuMS42NSIsImlkZVZlcnNpb24iOiIifQ==";
        assert_eq!(
            header(&model, "Authorization"),
            Some(format!("Bearer COSY.{payload}.d73d0e9e9fa885e3047029f0f9187792").as_str())
        );
        assert_eq!(
            header(&infer, "Authorization"),
            Some(format!("Bearer COSY.{payload}.05926d9f3987a4e7c7b13dfb5091594d").as_str())
        );
        assert_eq!(infer.body, b"$kwm");
        assert_eq!(
            model.url,
            "https://api2.qoder.sh/algo/api/v2/model/list?Encode=1"
        );
        assert!(header(&infer, "Cosy-MachineToken").is_none());
        assert!(header(&infer, "Cosy-MachineType").is_none());
    }

    #[test]
    fn response_codec_preserves_json_and_rejects_corruption() {
        let raw = br#"{"assistant":[{"key":"synthetic","enable":true}]}"#;
        assert_eq!(decode_body(encode_body(raw).as_bytes()).unwrap(), raw);
        let expected: Value = serde_json::from_slice(raw).unwrap();
        assert_eq!(decode_server_response(raw).unwrap(), expected);
        assert_eq!(
            decode_server_response(encode_body(raw).as_bytes()).unwrap(),
            expected
        );
        assert!(decode_body(b"$invalid$").is_err());
        assert!(decode_server_response(b"not JSON or a Qoder response").is_err());
        assert!(decode_server_response(encode_body(b"not JSON").as_bytes()).is_err());
    }

    #[test]
    fn failed_entropy_does_not_publish_runtime_fields() {
        let mut calls = 0;
        let result = runtime_fields(b"{}", |bytes| {
            calls += 1;
            if calls == 1 {
                bytes.fill(1);
                Ok(())
            } else {
                Err(failure("synthetic entropy failure"))
            }
        });
        assert!(result.is_err());
    }
}
