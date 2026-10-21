include!("../tests/component_contract.rs");
use std::io::Write;

struct PipeHost;

#[async_trait]
impl HostServices for PipeHost {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let coding = |coding| match coding {
            stravia_vendor_sdk::HttpContentCoding::Gzip => "gzip",
            stravia_vendor_sdk::HttpContentCoding::Deflate => "deflate",
            stravia_vendor_sdk::HttpContentCoding::Br => "br",
            stravia_vendor_sdk::HttpContentCoding::Zstd => "zstd",
        };
        let compression = request.compression.map(|policy| json!({
            "request": policy.request.map(coding),
            "response": policy.response.into_iter().map(coding).collect::<Vec<_>>()
        }));
        println!("{}", json!({"http_request": {
            "method": request.method, "url": request.url,
            "headers": request.headers, "body": STANDARD.encode(request.body),
            "compression": compression
        }}));
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        Ok(Arc::new(Reply {
            status: response["status"].as_u64().unwrap() as u16,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            chunks: Mutex::new(response["body"].as_str().unwrap().as_bytes().chunks(7).map(<[u8]>::to_vec).collect()),
        }))
    }
    async fn ws_connect(&self, _: WsRequest) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        Err(HostFailure::new(ErrorKind::Unsupported, "not used by this smoke"))
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> { Ok(None) }
    async fn write_private_state(&self, _: Vec<u8>) -> Result<(), HostFailure> {
        Err(HostFailure::new(ErrorKind::Unsupported, "this smoke must not persist credentials"))
    }
    async fn emit_event(&self, _: RuntimeEvent) -> Result<(), HostFailure> { Ok(()) }
    fn log(&self, _: LogLevel, _: &str) {}
    fn generation_is_current(&self, generation: u64) -> bool { generation == 1 }
}

#[tokio::main]
async fn main() {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let config: Value = serde_json::from_str(&line).unwrap();
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&artifact()).await.unwrap();
    let mut provider = snapshot(REGIONS[0]);
    provider.model = Some("qfmodel".into());
    provider.credentials = serde_json::from_value(config["credentials"].clone()).unwrap();
    provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
        id: Some("qfmodel".into()), family: None, selector: None,
        capabilities: Default::default(),
        extensions: BTreeMap::from([("qoder_model_config".into(), config["model_config"].clone())]),
    });
    provider.operation_metadata.insert("session_affinity".into(), config["affinity"].clone());
    let endpoint = common::endpoint("open-responses").unwrap();
    let request = ProtocolTransform::global().bind(endpoint, endpoint).unwrap().decode_request(config["request"].clone()).unwrap();
    let scope = OperationScope::new(Arc::new(PipeHost), CancellationToken::new(), Deadline::from_now(Duration::from_secs(180)), 1);
    let result = run(&runtime, &plugin, "cn", OperationInput::Infer { provider, request }, scope).await.unwrap();
    println!("{}", json!({"response":response_wire(result)}));
}
