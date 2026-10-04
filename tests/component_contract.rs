use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use md5::{Digest, Md5};
use parking_lot::Mutex;
use serde_json::{Value, json};
use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::protocol::ir::AiErrorKind;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_common::common;
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LogLevel,
    OperationScope, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::{
    AllowanceRequest, AuthRequest, AuthResponse, AuthStep, DiscoverRequest, ErrorKind,
    OperationInput, OperationOutput, ProviderSnapshot,
};

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    chunks: Mutex<VecDeque<Vec<u8>>>,
}

impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            chunks: Mutex::new(VecDeque::from([serde_json::to_vec(&body).unwrap()])),
        }
    }
    fn status(status: u16, body: Value) -> Self {
        Self {
            status,
            ..Self::json(body)
        }
    }
    fn encoded(body: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            chunks: Mutex::new(VecDeque::from([reference_encode(
                &serde_json::to_vec(&body).unwrap(),
            )])),
        }
    }
    fn stream(body: &str, chunk_size: usize) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            chunks: Mutex::new(
                body.as_bytes()
                    .chunks(chunk_size)
                    .map(<[u8]>::to_vec)
                    .collect(),
            ),
        }
    }
}

#[async_trait]
impl HostHttpResponse for Reply {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.status)
    }
    async fn headers(&self) -> Result<Vec<(String, String)>, HostFailure> {
        Ok(self.headers.clone())
    }
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.chunks.lock().pop_front())
    }
}

struct LocalUpstream {
    origins: Vec<String>,
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<HttpRequest>>,
    state: Mutex<Option<Vec<u8>>>,
    events: Mutex<Vec<RuntimeEvent>>,
}

impl LocalUpstream {
    fn new(origins: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            origins: origins.iter().map(|origin| (*origin).into()).collect(),
            replies: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            state: Mutex::new(None),
            events: Mutex::new(Vec::new()),
        })
    }
    fn reply(&self, value: Reply) {
        self.replies.lock().push_back(value);
    }
    fn scope(self: &Arc<Self>) -> OperationScope {
        OperationScope::new(
            self.clone(),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(30)),
            1,
        )
    }
}

#[async_trait]
impl HostServices for LocalUpstream {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let url = url::Url::parse(&request.url).unwrap();
        assert!(
            self.origins.contains(&url.origin().ascii_serialization()),
            "HTTP destination must be declared and approved"
        );
        self.requests.lock().push(request);
        Ok(Arc::new(
            self.replies
                .lock()
                .pop_front()
                .expect("unexpected HTTP request or retry"),
        ))
    }
    async fn ws_connect(
        &self,
        _: String,
        _: Vec<(String, String)>,
        _: Vec<String>,
        _: Option<String>,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Unsupported,
            "WebSocket is not allowed in this contract",
        ))
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.state.lock().clone())
    }
    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure> {
        *self.state.lock() = Some(bytes);
        Ok(())
    }
    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure> {
        self.events.lock().push(event);
        Ok(())
    }
    fn log(&self, _: LogLevel, _: &str) {}
    fn generation_is_current(&self, generation: u64) -> bool {
        generation == 1
    }
}

#[derive(Clone, Copy)]
struct RegionFixture {
    channel: &'static str,
    infer: &'static str,
    openapi: &'static str,
    center: &'static str,
    web: &'static str,
}

const REGIONS: [RegionFixture; 2] = [
    RegionFixture {
        channel: "cn",
        infer: "https://gateway.qoder.com.cn",
        openapi: "https://openapi.qoder.com.cn",
        center: "https://gateway.qoder.com.cn",
        web: "https://qoder.cn",
    },
    RegionFixture {
        channel: "global",
        infer: "https://api2.qoder.sh",
        openapi: "https://openapi.qoder.sh",
        center: "https://center.qoder.sh",
        web: "https://qoder.com",
    },
];

fn snapshot(region: RegionFixture) -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "qoder".into(),
        channel: region.channel.into(),
        base_url: region.infer.into(),
        protocol: "openai-compatible".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::new(),
        model: Some("synthetic-model".into()),
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::from([(
            "session_affinity".into(),
            json!("synthetic-session"),
        )]),
    }
}

fn artifact() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/wasm32-wasip2/release/stravia_vendor_qoder.wasm"),
    )
    .expect("build the release wasm32-wasip2 component first")
}

fn auth(provider: &ProviderSnapshot, step: AuthStep) -> OperationInput {
    OperationInput::Auth {
        provider: provider.clone(),
        request: AuthRequest { step },
    }
}

fn profile_replies(local: &LocalUpstream, token: &str, refresh: &str) {
    local.reply(Reply::json(json!({"token":token,"user_id":"synthetic-user","refresh_token":refresh,"expires_in":3600,"refresh_token_expires_in":7200})));
    account_replies(local);
}

fn account_replies(local: &LocalUpstream) {
    local.reply(Reply::json(json!({"id":"synthetic-user","uid":"secondary-profile-uid","name":"Synthetic User","organization_id":"synthetic-org","organization_name":"Synthetic Organization"})));
    local.reply(Reply::json(json!({"tags":["synthetic-tag"]})));
    local.reply(Reply::encoded(json!({"result":{"status":"AGREE"}})));
}

fn credentials(output: OperationOutput) -> BTreeMap<String, Value> {
    let OperationOutput::Auth(AuthResponse::Credentials { values, .. }) = output else {
        panic!("expected stored credentials")
    };
    values
}

// Independent wire reference, derived from the official 1.1.65 synthetic oracle.
// It intentionally does not call the vendor's protocol module or reuse its tables.
const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
const SUBSTITUTION: &[u8] = b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!$";

fn exchange_thirds(bytes: &mut [u8]) {
    let third = bytes.len() / 3;
    let end = bytes.len() - third;
    for i in 0..third {
        bytes.swap(i, end + i);
    }
}

fn reference_encode(raw: &[u8]) -> Vec<u8> {
    let mut bytes = STANDARD
        .encode(raw)
        .bytes()
        .map(|byte| SUBSTITUTION[B64.iter().position(|candidate| *candidate == byte).unwrap()])
        .collect::<Vec<_>>();
    exchange_thirds(&mut bytes);
    bytes
}

fn reference_decode(encoded: &[u8]) -> Value {
    let mut bytes = encoded.to_vec();
    exchange_thirds(&mut bytes);
    let base64 = bytes
        .into_iter()
        .map(|byte| {
            B64[SUBSTITUTION
                .iter()
                .position(|candidate| *candidate == byte)
                .expect("invalid encoded request byte")]
        })
        .collect::<Vec<_>>();
    serde_json::from_slice(&STANDARD.decode(base64).expect("invalid request base64"))
        .expect("invalid request JSON")
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> &'a str {
    &request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .unwrap_or_else(|| panic!("missing {name}"))
        .1
}

fn verify_signed(request: &HttpRequest, origin: &str, path: &str, uid: &str) -> Option<Value> {
    let url = url::Url::parse(&request.url).unwrap();
    assert_eq!(url.origin().ascii_serialization(), origin);
    assert_eq!(url.path(), format!("/algo{path}"));
    let authorization = header(request, "Authorization")
        .strip_prefix("Bearer COSY.")
        .expect("native COSY authorization");
    let (payload, signature) = authorization.rsplit_once('.').unwrap();
    let payload_json: Value = serde_json::from_slice(&STANDARD.decode(payload).unwrap()).unwrap();
    assert_eq!(payload_json["cosyVersion"], "1.1.65");
    assert!(
        payload_json["info"]
            .as_str()
            .is_some_and(|info| STANDARD.decode(info).is_ok())
    );
    assert_eq!(
        STANDARD.decode(header(request, "Cosy-Key")).unwrap().len(),
        128
    );
    let canonical = format!(
        "{payload}\n{}\n{}\n{}\n{path}",
        header(request, "Cosy-Key"),
        header(request, "Cosy-Date"),
        std::str::from_utf8(&request.body).unwrap()
    );
    assert_eq!(
        signature,
        format!("{:x}", Md5::digest(canonical.as_bytes())),
        "signature must cover the actual encoded body and query-free protocol path"
    );
    assert_eq!(header(request, "Cosy-User"), uid);
    if request.method == "GET" {
        assert!(
            request.body.is_empty(),
            "GET signature covers an empty body, not encoded JSON"
        );
        None
    } else {
        Some(reference_decode(&request.body))
    }
}

fn request() -> stravia_vendor_sdk::AiRequest {
    let endpoint = common::endpoint("openai-compatible").unwrap();
    ProtocolTransform::global().bind(endpoint, endpoint).unwrap().decode_request(json!({
        "model":"client-route-name", "stream":true,
        "messages":[{"role":"user","content":"你好"}],
        "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]
    })).unwrap()
}

fn response_wire(output: OperationOutput) -> Value {
    let OperationOutput::Infer(response) = output else {
        panic!("expected inference output")
    };
    let endpoint = common::endpoint("openai-compatible").unwrap();
    ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap()
        .encode_response(&response)
        .unwrap()
}

fn envelope(body: Value) -> String {
    format!(
        "data: {}\r\n\r\n",
        json!({"headers":{},"body":body.to_string(),"statusCodeValue":200,"statusCode":"OK"})
    )
}

fn stream(finish: bool) -> String {
    let frames = [
        json!({"id":"synthetic-chat","model":"synthetic-model","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"想"},"finish_reason":null}]}),
        json!({"id":"synthetic-chat","model":"synthetic-model","choices":[{"index":0,"delta":{"content":"你好"},"finish_reason":null}]}),
        json!({"id":"synthetic-chat","model":"synthetic-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_weather","type":"function","function":{"name":"weather","arguments":"{\"city\":"}}]},"finish_reason":null}]}),
        json!({"id":"synthetic-chat","model":"synthetic-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"杭州\"}"}}]},"finish_reason":null}]}),
        json!({"id":"synthetic-chat","model":"synthetic-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"id":"synthetic-chat","model":"synthetic-model","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}),
    ];
    let mut body = String::from(": heartbeat\r\n\r\n");
    for frame in frames {
        body.push_str(&envelope(frame));
    }
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"headers":{},"body":"[DONE]","statusCodeValue":200,"statusCode":"OK"})
    ));
    if finish {
        body.push_str("event: finish\r\ndata: {}\r\n\r\n");
    }
    body
}

fn catalog() -> Value {
    json!({"assistant":[
        {"key":"synthetic-model","display_name":"Synthetic Reasoning Vision","enable":true,"is_reasoning":true,"is_vl":true,"max_input_tokens":32000,"max_output_tokens":4096,"source":"system","format":"openai","price_factor":0.5},
        {"key":"disabled-model","display_name":"Disabled","enable":false},
        {"key":"other-model","display_name":"Other","enable":true}
    ],"byok_enterprise":[{"key":"byok-only","display_name":"BYOK only"}]})
}

// Reusable smoke scenario (no real-account network): build the release component,
// then cargo test --test component_contract -- --ignored --nocapture.
// Both tests use LocalUpstream only: device flow/profile/rotation/isolation/revoke,
// signed catalog and quota, then byte-fragmented canonical SSE success/failure.
#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn regional_device_catalog_and_quota_contract() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&artifact()).await.unwrap();
    assert_eq!(plugin.descriptor().vendor_id, "qoder");
    assert_eq!(
        plugin.descriptor().providers[0]
            .channels
            .iter()
            .map(|channel| channel.id.as_str())
            .collect::<Vec<_>>(),
        ["cn", "global"]
    );
    for region in REGIONS {
        let local = LocalUpstream::new(&[region.infer, region.openapi, region.center]);
        let mut provider = snapshot(region);
        let start = runtime
            .execute(
                &plugin,
                region.channel,
                auth(
                    &provider,
                    AuthStep::Start {
                        redirect_uri: String::new(),
                        state: "synthetic-host-state".into(),
                    },
                ),
                local.scope(),
            )
            .await
            .unwrap();
        let OperationOutput::Auth(AuthResponse::Authorization { url, .. }) = start else {
            panic!("expected device browser authorization")
        };
        let browser = url::Url::parse(&url).unwrap();
        assert_eq!(browser.origin().ascii_serialization(), region.web);
        assert_eq!(browser.path(), "/device/selectAccounts");
        let query = browser.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(
            query.get("challenge_method").map(|value| value.as_ref()),
            Some("S256")
        );
        assert!(!query["challenge"].is_empty());
        assert!(!query["nonce"].is_empty());
        assert!(
            local.requests.lock().is_empty(),
            "device start must not transmit credentials"
        );

        local.reply(Reply::status(404, json!({})));
        let pending = runtime
            .execute(
                &plugin,
                region.channel,
                auth(&provider, AuthStep::Poll),
                local.scope(),
            )
            .await
            .unwrap();
        assert!(matches!(
            pending,
            OperationOutput::Auth(AuthResponse::Pending { .. })
        ));
        local.reply(Reply::json(json!({})));
        let awaiting_token = runtime
            .execute(
                &plugin,
                region.channel,
                auth(&provider, AuthStep::Poll),
                local.scope(),
            )
            .await
            .unwrap();
        assert!(matches!(
            awaiting_token,
            OperationOutput::Auth(AuthResponse::Pending { .. })
        ));
        profile_replies(&local, "synthetic-oauth-one", "synthetic-refresh-one");
        provider.credentials = credentials(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    auth(&provider, AuthStep::Poll),
                    local.scope(),
                )
                .await
                .unwrap(),
        );
        assert_eq!(provider.credentials["region"], region.channel);
        assert_eq!(provider.credentials["uid"], "synthetic-user");
        assert_eq!(provider.credentials["access_token"], "synthetic-oauth-one");
        assert_eq!(
            provider.credentials["organization_tags"],
            json!(["synthetic-tag"])
        );
        assert_eq!(provider.credentials["data_policy_agreed"], true);
        assert_eq!(provider.credentials["identity"]["uid"], "synthetic-user");
        assert!(
            provider.credentials["identity"]["encrypt_user_info"]
                .as_str()
                .is_some_and(|value| STANDARD.decode(value).is_ok())
        );
        {
            let requests = local.requests.lock();
            let poll = &requests[0];
            assert_eq!(
                url::Url::parse(&poll.url)
                    .unwrap()
                    .origin()
                    .ascii_serialization(),
                region.openapi
            );
            assert_eq!(
                url::Url::parse(&poll.url).unwrap().path(),
                "/api/v1/deviceToken/poll"
            );
            let poll_url = url::Url::parse(&poll.url).unwrap();
            let poll_query = poll_url.query_pairs().collect::<BTreeMap<_, _>>();
            assert_eq!(poll_query["nonce"], query["nonce"]);
            assert_eq!(
                URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(poll_query["verifier"].as_bytes())),
                query["challenge"]
            );
            let policy = requests.last().unwrap();
            verify_signed(
                policy,
                region.center,
                "/api/v2/config/getDataPolicy",
                "synthetic-user",
            );
            assert_eq!(
                header(policy, "Cosy-Data-Policy"),
                "disagree",
                "a policy lookup must not predeclare account consent"
            );
        }

        local.reply(Reply::json(json!({"device_token":"synthetic-oauth-two","refresh_token":"synthetic-refresh-two","expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-02-01T00:00:00Z"})));
        account_replies(&local);
        provider.credentials = credentials(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    auth(&provider, AuthStep::Refresh),
                    local.scope(),
                )
                .await
                .unwrap(),
        );
        assert_eq!(provider.credentials["access_token"], "synthetic-oauth-two");
        assert_eq!(
            provider.credentials["refresh_token"],
            "synthetic-refresh-two"
        );
        let refresh = local
            .requests
            .lock()
            .iter()
            .find(|request| request.url.ends_with("/api/v1/deviceToken/refresh"))
            .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
            .unwrap();
        assert_eq!(refresh["refresh_token"], "synthetic-refresh-one");

        let before_rejected_refresh = local.requests.lock().len();
        local.reply(Reply::json(json!({"device_token":"synthetic-other-account-token","refresh_token":"synthetic-other-account-refresh"})));
        local.reply(Reply::json(
            json!({"id":"another-account","uid":"synthetic-user"}),
        ));
        let changed_account = runtime
            .execute(
                &plugin,
                region.channel,
                auth(&provider, AuthStep::Refresh),
                local.scope(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            changed_account,
            stravia_vendor_runtime::RuntimeError::Plugin {
                kind: ErrorKind::Auth,
                ..
            }
        ));
        assert_eq!(
            local.requests.lock().len(),
            before_rejected_refresh + 2,
            "a different canonical account must be rejected before tags or policy queries"
        );

        provider
            .options
            .insert("model_ids".into(), json!("synthetic-model"));
        local.reply(Reply::encoded(catalog()));
        let discovered = runtime
            .execute(
                &plugin,
                region.channel,
                OperationInput::Discover {
                    provider: provider.clone(),
                    request: DiscoverRequest::default(),
                },
                local.scope(),
            )
            .await
            .unwrap();
        let OperationOutput::Discover(discovered) = discovered else {
            panic!("expected models")
        };
        assert_eq!(
            discovered
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["synthetic-model"]
        );
        let model = &discovered.models[0];
        assert_eq!(model.display_name, "Synthetic Reasoning Vision");
        assert_eq!(model.metadata["reasoning"], true);
        assert_eq!(model.metadata["limit"]["input"], 32000);
        assert_eq!(model.metadata["limit"]["output"], 4096);
        verify_signed(
            local.requests.lock().last().unwrap(),
            region.infer,
            "/api/v2/model/list",
            "synthetic-user",
        );

        local.reply(Reply::json(json!({"user_id":"synthetic-user","user_type":"PRO","user_quota":{"total":100,"used":25,"remaining":75,"unit":"credits"}})));
        let quota = runtime
            .execute(
                &plugin,
                region.channel,
                OperationInput::Allowance {
                    provider: provider.clone(),
                    request: AllowanceRequest::default(),
                },
                local.scope(),
            )
            .await
            .unwrap();
        let OperationOutput::Allowance(quota) = quota else {
            panic!("expected quota")
        };
        let allowance = quota
            .allowances
            .iter()
            .find(|row| {
                row.limit
                    .as_ref()
                    .is_some_and(|amount| amount.value == "100")
            })
            .expect("real user quota total");
        assert_eq!(allowance.used.as_ref().unwrap().value, "25");
        assert_eq!(allowance.remaining.as_ref().unwrap().value, "75");
        assert_eq!(allowance.limit.as_ref().unwrap().unit, "credits");
        {
            let requests = local.requests.lock();
            let quota_request = requests.last().unwrap();
            assert_eq!(
                quota_request.url,
                format!("{}/api/v2/quota/usage", region.openapi)
            );
            assert_eq!(
                header(quota_request, "Authorization"),
                "Bearer synthetic-oauth-two"
            );
        }
        local.reply(Reply::json(json!({"user_id":"synthetic-user","user_type":"PRO","user_quota":{"used":9,"unit":"credits"}})));
        let partial = runtime
            .execute(
                &plugin,
                region.channel,
                OperationInput::Allowance {
                    provider: provider.clone(),
                    request: AllowanceRequest::default(),
                },
                local.scope(),
            )
            .await
            .unwrap();
        let OperationOutput::Allowance(partial) = partial else {
            panic!("expected partial quota")
        };
        assert_eq!(partial.allowances[0].used.as_ref().unwrap().value, "9");
        assert!(
            partial.allowances[0].limit.is_none() && partial.allowances[0].remaining.is_none(),
            "missing total and remainder remain unknown, not zero"
        );
        local.reply(Reply::json(json!({})));
        assert!(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    OperationInput::Allowance {
                        provider: provider.clone(),
                        request: AllowanceRequest::default()
                    },
                    local.scope()
                )
                .await
                .is_err(),
            "unknown quota must not become a fabricated zero balance"
        );

        let before = local.requests.lock().len();
        let mut crossed = provider.clone();
        crossed.credentials.insert(
            "region".into(),
            json!(if region.channel == "cn" {
                "global"
            } else {
                "cn"
            }),
        );
        assert!(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    OperationInput::Discover {
                        provider: crossed,
                        request: DiscoverRequest::default()
                    },
                    local.scope()
                )
                .await
                .is_err()
        );
        assert_eq!(
            local.requests.lock().len(),
            before,
            "cross-region credentials must fail before outbound HTTP"
        );
        let revoked = runtime
            .execute(
                &plugin,
                region.channel,
                auth(&provider, AuthStep::Revoke),
                local.scope(),
            )
            .await
            .unwrap();
        assert!(matches!(
            revoked,
            OperationOutput::Auth(AuthResponse::Revoked)
        ));
        assert_eq!(
            local.requests.lock().len(),
            before,
            "local revoke must not invent an upstream API"
        );
        // Apply the SDK's Revoked response as the host does.
        provider.credentials.clear();
        assert!(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    OperationInput::Discover {
                        provider: provider.clone(),
                        request: DiscoverRequest::default()
                    },
                    local.scope()
                )
                .await
                .is_err(),
            "a revoked account cannot make authenticated requests"
        );
        assert_eq!(local.requests.lock().len(), before);
        runtime
            .execute(
                &plugin,
                region.channel,
                auth(
                    &provider,
                    AuthStep::Start {
                        redirect_uri: String::new(),
                        state: "synthetic-expiring-state".into(),
                    },
                ),
                local.scope(),
            )
            .await
            .unwrap();
        let mut expired: Value =
            serde_json::from_slice(local.state.lock().as_ref().unwrap()).unwrap();
        expired["device_login"]["deadline"] = json!(0);
        *local.state.lock() = Some(serde_json::to_vec(&expired).unwrap());
        assert!(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    auth(&provider, AuthStep::Poll),
                    local.scope()
                )
                .await
                .is_err(),
            "expired device flow must not poll upstream"
        );
        assert_eq!(local.requests.lock().len(), before);
        runtime
            .execute(
                &plugin,
                region.channel,
                auth(
                    &provider,
                    AuthStep::Start {
                        redirect_uri: String::new(),
                        state: "synthetic-denied-state".into(),
                    },
                ),
                local.scope(),
            )
            .await
            .unwrap();
        local.reply(Reply::status(
            401,
            json!({"error":"synthetic-secret-oauth"}),
        ));
        let denied = runtime
            .execute(
                &plugin,
                region.channel,
                auth(&provider, AuthStep::Poll),
                local.scope(),
            )
            .await
            .unwrap_err();
        assert!(!format!("{denied:?}").contains("synthetic-secret-oauth"));
        assert!(local.replies.lock().is_empty());
        println!(
            "{}: native device login, PKCE, pending, profile, rotation, signed catalog, quota, isolation and revoke",
            region.channel
        );
    }
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn official_cli_parallel_history_contract() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&artifact()).await.unwrap();
    let endpoint = common::endpoint("anthropic").unwrap();
    // 预期来自官方 1.1.65 的 S8e/Zel/FWc/UWc 离线合成执行，不调用插件构造器。
    let expected: Value = serde_json::from_str(include_str!(
        "fixtures/qoder_cli_1_1_65_parallel_tools.json"
    ))
    .unwrap();
    for region in REGIONS {
        let local = LocalUpstream::new(&[region.infer, region.openapi, region.center]);
        let mut provider = snapshot(region);
        provider.model = Some("qfmodel".into());
        if region.channel == "cn" {
            provider
                .options
                .insert("machine_os".into(), json!("aarch64_linux"));
            provider
                .options
                .insert("machine_hostname".into(), json!("é.com/path"));
        }
        provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            id: Some("qfmodel".into()),
            family: None,
            selector: None,
            capabilities: Default::default(),
            extensions: BTreeMap::from([(
                "qoder_model_config".into(),
                json!({
                    "key":"qfmodel","display_name":"Synthetic Flash","source":"system",
                    "format":"openai","is_vl":true,"is_reasoning":false,
                    "max_input_tokens":200000,"max_output_tokens":16384,
                    "unknown_catalog_field":"must-not-reach-the-upstream"
                }),
            )]),
        });
        runtime
            .execute(
                &plugin,
                region.channel,
                auth(
                    &provider,
                    AuthStep::Start {
                        redirect_uri: String::new(),
                        state: "synthetic-official-state".into(),
                    },
                ),
                local.scope(),
            )
            .await
            .unwrap();
        profile_replies(
            &local,
            "synthetic-official-token",
            "synthetic-official-refresh",
        );
        provider.credentials = credentials(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    auth(&provider, AuthStep::Poll),
                    local.scope(),
                )
                .await
                .unwrap(),
        );
        let mut input = ProtocolTransform::global().bind(endpoint, endpoint).unwrap().decode_request(json!({
            "model":"client-route","stream":true,"max_tokens":16384,"system":"synthetic system",
            "messages":[
                {"role":"user","content":"synthetic question"},
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_a","name":"echo","input":{"value":"alpha"}},
                    {"type":"tool_use","id":"call_b","name":"echo","input":{"value":"beta"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","content":"beta","tool_use_id":"call_b"},
                    {"type":"tool_result","content":"alpha","tool_use_id":"call_a","is_error":true},
                    {"type":"text","text":"synthetic continuation"}
                ]}
            ],
            "tools":[{"name":"echo","description":"Synthetic echo","input_schema":{"type":"object","properties":{"value":{"type":"string"}}}}]
        })).unwrap();
        input.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium);
        // VendorRuntime 接收 canonical IR；入口 codec 已摊平结果，不能用丢失错误标记的 wire 解码器验证此契约。
        use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent, Role};
        input.items.retain(|item| item.role != Role::Tool);
        input.items.last_mut().unwrap().content = MessageContent::Blocks(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_b".into(),
                content: json!("beta"),
                content_kind: None,
                is_error: None,
                cache_control: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_a".into(),
                content: json!("alpha"),
                content_kind: None,
                is_error: Some(true),
                cache_control: None,
            },
            ContentBlock::Text {
                text: "synthetic continuation".into(),
                cache_control: None,
            },
        ]);
        let completed = envelope(
            json!({"id":"synthetic-official-response","model":"qfmodel","choices":[{"index":0,"delta":{"content":"accepted"},"finish_reason":"stop"}]}),
        ) + "event: finish\ndata: {}\n\n";
        local.reply(Reply::stream(&completed, 7));
        let started_at = chrono::Utc::now().timestamp_millis();
        let result = runtime
            .execute(
                &plugin,
                region.channel,
                OperationInput::Infer {
                    provider: provider.clone(),
                    request: input,
                },
                local.scope(),
            )
            .await
            .unwrap();
        assert_eq!(
            response_wire(result)["choices"][0]["message"]["content"],
            "accepted"
        );
        let requests = local.requests.lock();
        let request = requests.last().unwrap();
        let actual = verify_signed(
            request,
            region.infer,
            "/api/v2/service/pro/sse/agent_chat_generation",
            "synthetic-user",
        )
        .unwrap();
        let mut oracle = expected.clone();
        let business = &actual["business"];
        assert!(
            business["begin_at"].as_i64().is_some_and(
                |value| value >= started_at && value <= chrono::Utc::now().timestamp_millis()
            ),
            "the business start must belong to this inference operation"
        );
        let business_id = uuid::Uuid::parse_str(business["id"].as_str().unwrap()).unwrap();
        assert_eq!(business_id.get_version_num(), 4);
        assert_ne!(business["id"], actual["request_id"]);
        oracle["business"]["id"] = business["id"].clone();
        oracle["business"]["begin_at"] = business["begin_at"].clone();
        for key in ["request_id", "request_set_id", "chat_record_id"] {
            oracle[key] = actual[key].clone();
        }
        oracle["session_type"] = json!(if region.channel == "cn" {
            "qoderclicn"
        } else {
            "qodercli"
        });
        assert_eq!(
            actual, oracle,
            "parallel calls and reversed/error tool results must match the official CLI"
        );
        assert_eq!(
            header(request, "Cosy-MachineOS"),
            if region.channel == "cn" {
                "aarch64_linux"
            } else {
                "x86_64_win32"
            }
        );
        let expected_hostname = if region.channel == "cn" {
            "xn--9ca.com".to_owned()
        } else {
            format!(
                "stravia-{}",
                provider.credentials["machine_id"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .filter(|c| c.is_ascii_hexdigit())
                    .take(12)
                    .collect::<String>()
            )
        };
        assert_eq!(header(request, "Cosy-MachineHostname"), expected_hostname);
        let policy = requests
            .iter()
            .find(|request| request.url.contains("/api/v2/config/getDataPolicy?"))
            .unwrap();
        assert_eq!(
            header(policy, "Cosy-MachineOS"),
            header(request, "Cosy-MachineOS")
        );
        assert!(
            !policy
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Cosy-MachineHostname")),
            "machine hostname belongs only to the inference transport"
        );
        println!(
            "{}: official CLI body oracle and native client identity headers accepted",
            region.channel
        );
    }
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn native_enveloped_stream_completion_and_failure_contract() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&artifact()).await.unwrap();
    for region in REGIONS {
        let local = LocalUpstream::new(&[region.infer, region.openapi, region.center]);
        let mut provider = snapshot(region);
        runtime
            .execute(
                &plugin,
                region.channel,
                auth(
                    &provider,
                    AuthStep::Start {
                        redirect_uri: String::new(),
                        state: "synthetic-stream-state".into(),
                    },
                ),
                local.scope(),
            )
            .await
            .unwrap();
        profile_replies(&local, "synthetic-secret-oauth", "synthetic-secret-refresh");
        provider.credentials = credentials(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    auth(&provider, AuthStep::Poll),
                    local.scope(),
                )
                .await
                .unwrap(),
        );
        local.reply(Reply::encoded(catalog()));
        let discovered = runtime
            .execute(
                &plugin,
                region.channel,
                OperationInput::Discover {
                    provider: provider.clone(),
                    request: DiscoverRequest::default(),
                },
                local.scope(),
            )
            .await
            .unwrap();
        let OperationOutput::Discover(discovered) = discovered else {
            panic!("expected models")
        };
        assert_eq!(
            discovered
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["synthetic-model", "other-model"],
            "disabled and BYOK-only entries must not be advertised as usable native models"
        );
        let model = discovered
            .models
            .iter()
            .find(|model| model.id == "synthetic-model")
            .unwrap();
        provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            id: Some(model.id.clone()),
            family: model.family.clone(),
            selector: model.selector.clone(),
            capabilities: model.capabilities.clone(),
            extensions: model.metadata.clone(),
        });
        for chunk_size in [1, 7] {
            local.events.lock().clear();
            local.reply(Reply::stream(&stream(true), chunk_size));
            let wire = response_wire(
                runtime
                    .execute(
                        &plugin,
                        region.channel,
                        OperationInput::Infer {
                            provider: provider.clone(),
                            request: request(),
                        },
                        local.scope(),
                    )
                    .await
                    .unwrap(),
            );
            assert_eq!(wire["choices"][0]["message"]["content"], "你好");
            assert_eq!(wire["choices"][0]["message"]["reasoning_content"], "想");
            assert_eq!(
                wire["choices"][0]["message"]["tool_calls"][0]["id"],
                "call_weather"
            );
            assert_eq!(
                wire["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
                "weather"
            );
            assert_eq!(
                wire["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
                "{\"city\":\"杭州\"}"
            );
            assert_eq!(wire["choices"][0]["finish_reason"], "tool_calls");
            assert_eq!(wire["usage"]["prompt_tokens"], 11);
            assert_eq!(wire["usage"]["completion_tokens"], 7);
            assert_eq!(wire["usage"]["total_tokens"], 18);
            assert_eq!(
                local
                    .events
                    .lock()
                    .iter()
                    .filter(|event| matches!(event, RuntimeEvent::Completed))
                    .count(),
                1
            );
            let requests = local.requests.lock();
            let body = verify_signed(
                requests.last().unwrap(),
                region.infer,
                "/api/v2/service/pro/sse/agent_chat_generation",
                "synthetic-user",
            )
            .unwrap();
            assert_eq!(body["model_config"]["key"], "synthetic-model");
            assert_eq!(body["session_id"], "synthetic-session");
            let user = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "user")
                .unwrap();
            assert_eq!(user["content"], "你好");
            assert_eq!(body["tools"][0]["function"]["name"], "weather");
        }
        // A valid inner finish_reason and inner [DONE] are not authoritative completion.
        local.events.lock().clear();
        local.reply(Reply::stream(&stream(false), 3));
        assert!(
            runtime
                .execute(
                    &plugin,
                    region.channel,
                    OperationInput::Infer {
                        provider: provider.clone(),
                        request: request()
                    },
                    local.scope()
                )
                .await
                .is_err()
        );
        assert!(
            !local
                .events
                .lock()
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Completed))
        );
        // Cover both a failed outer status and a business error inside an HTTP-200 envelope.
        for status in [401, 200] {
            local.events.lock().clear();
            let error_body = format!(
                "data: {}\n\nevent: finish\ndata: {{}}\n\n",
                json!({"headers":{},"body":json!({"error":{"message":"synthetic-secret-oauth synthetic-secret-refresh","code":"unauthorized"}}).to_string(),"statusCodeValue":status,"statusCode":if status == 200 { "OK" } else { "UNAUTHORIZED" }})
            );
            local.reply(Reply::stream(&error_body, 2));
            let error = runtime
                .execute(
                    &plugin,
                    region.channel,
                    OperationInput::Infer {
                        provider: provider.clone(),
                        request: request(),
                    },
                    local.scope(),
                )
                .await
                .unwrap_err();
            let diagnostic = format!(
                "{error:?} {}",
                error.diagnostic_message().unwrap_or_default()
            );
            assert!(!diagnostic.contains("synthetic-secret-oauth"));
            assert!(!diagnostic.contains("synthetic-secret-refresh"));
            assert!(
                !local
                    .events
                    .lock()
                    .iter()
                    .any(|event| matches!(event, RuntimeEvent::Completed)),
                "in-band error cannot be completed successfully by a later finish event"
            );
        }
        for code in [105, 10605, 103] {
            local.events.lock().clear();
            let business_error = format!(
                "data: {}\n\nevent: finish\ndata: {{}}\n\n",
                json!({"headers":{},"body":json!({"code":code.to_string(),"retryAfterMs":1500,"message":"synthetic-secret-oauth"}).to_string(),"statusCodeValue":500,"statusCode":"ERROR"})
            );
            local.reply(Reply::stream(&business_error, 2));
            let error = runtime
                .execute(
                    &plugin,
                    region.channel,
                    OperationInput::Infer {
                        provider: provider.clone(),
                        request: request(),
                    },
                    local.scope(),
                )
                .await
                .unwrap_err();
            match code {
                105 => assert_eq!(
                    error.model_error_kind(),
                    Some(AiErrorKind::AuthenticationError)
                ),
                10605 => {
                    assert_eq!(error.model_error_kind(), Some(AiErrorKind::RateLimitError));
                    assert_eq!(error.retry_after(), Some(Duration::from_millis(1500)));
                }
                103 => {
                    assert_eq!(error.model_error_kind(), Some(AiErrorKind::InvalidRequest));
                    assert!(
                        error.retry_after().is_none(),
                        "duplicate request is not a retryable queue response"
                    );
                }
                _ => unreachable!(),
            }
            assert!(
                !error
                    .diagnostic_message()
                    .unwrap_or_default()
                    .contains("synthetic-secret-oauth")
            );
            assert!(
                !local
                    .events
                    .lock()
                    .iter()
                    .any(|event| matches!(event, RuntimeEvent::Completed))
            );
        }
        assert!(local.replies.lock().is_empty());
        println!(
            "{}: independent request decoding/signature verification; byte-fragmented Chinese/reasoning/tools/usage; finish authority; truncated and in-band failure",
            region.channel
        );
    }
}
