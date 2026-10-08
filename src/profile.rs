use std::collections::BTreeSet;

use semver::Version;
use stravia_vendor_sdk::{
    AuthDescriptor, AuthFlow, CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField,
    ConfigFieldKind, DataCompatibility, EnumOption, NetworkDeclaration, OriginDeclaration,
    ProviderDescriptor, VendorDescriptor, VendorKind,
};

use crate::{Region, VENDOR_ID, messages};

pub(crate) fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
        Capability::ConfigValidation,
    ]);
    let channels = [Region::Cn, Region::Global]
        .into_iter()
        .map(|region| ChannelDescriptor {
            id: region.id().into(),
            name: match region {
                Region::Cn => messages::channel_cn(),
                Region::Global => messages::channel_global(),
            },
            description: Some(match region {
                Region::Cn => messages::channel_cn_description(),
                Region::Global => messages::channel_global_description(),
            }),
            auth: Some(AuthDescriptor {
                flow: AuthFlow::DeviceCode,
                callback: None,
                manual_input: None,
            }),
            protocol: Some("openai-compatible".into()),
            protocols: Vec::new(),
            default_base_url: Some(region.infer_origin().into()),
            default_models_source: None,
            consumes_catalog_models: false,
            capabilities: capabilities.clone(),
            model_capabilities: BTreeSet::new(),
        })
        .collect();
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: Version::parse(env!("CARGO_PKG_VERSION")).expect("valid Cargo version"),
        display_name: "Qoder".into(),
        description: Some("Qoder China and global native HTTP provider".into()),
        authors: vec!["Chikage0o0 <chikage@939.me>".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: None,
            display_name: "Qoder".into(),
            description: Some("Unofficial Qoder integration; account and model availability are controlled by the upstream service.".into()),
            channels,
            capabilities,
            website: Some(Region::Global.website().into()),
            icon_svg: Some(include_str!("../assets/qoder.svg").into()),
            implementation: None,
            config_groups: Vec::new(),
            config_fields: vec![ConfigField {
                key: "model_ids".into(),
                label: messages::model_ids(),
                description: Some(messages::model_ids_description()),
                kind: ConfigFieldKind::String { multiline: true },
                required: false,
                default_json: None,
                group: None,
                secret: false,
                min: None,
                max: None,
                max_length: Some(16384),
                pattern: None,
                visible_when: None,
            }, ConfigField {
                key: "machine_os".into(),
                label: messages::machine_os(),
                description: Some(messages::machine_os_description()),
                kind: ConfigFieldKind::Enum {
                    options: [
                        ("x86_64_win32", messages::machine_os_windows_x64()),
                        ("aarch64_win32", messages::machine_os_windows_arm64()),
                        ("x86_64_linux", messages::machine_os_linux_x64()),
                        ("aarch64_linux", messages::machine_os_linux_arm64()),
                        ("x86_64_darwin", messages::machine_os_macos_x64()),
                        ("aarch64_darwin", messages::machine_os_macos_arm64()),
                    ].into_iter().map(|(value, label)| EnumOption { value: value.into(), label }).collect(),
                },
                required: false,
                default_json: Some(serde_json::json!("x86_64_win32")),
                group: None,
                secret: false,
                min: None,
                max: None,
                max_length: None,
                pattern: None,
                visible_when: None,
            }, ConfigField {
                key: "machine_hostname".into(),
                label: messages::machine_hostname(),
                description: Some(messages::machine_hostname_description()),
                kind: ConfigFieldKind::String { multiline: false },
                required: false,
                default_json: None,
                group: None,
                secret: false,
                min: None,
                max: None,
                max_length: Some(4096),
                pattern: None,
                visible_when: None,
            }],
            // 登录、刷新和额度由区域 OpenAPI 服务承载，推理由 channel origin 承载。
            network: NetworkDeclaration {
                extra_origins: vec![
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "openapi.qoder.com.cn".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "openapi.qoder.sh".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "center.qoder.sh".into(),
                        port: None,
                    },
                ],
                ..NetworkDeclaration::default()
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}
