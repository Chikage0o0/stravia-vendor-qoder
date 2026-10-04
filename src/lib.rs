mod allowance;
mod auth;
mod inference;
mod models;
mod profile;
mod protocol;
mod state;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    ErrorKind, GuestHost, Operation, OperationInput, OperationOutput, PluginError,
    ProviderSnapshot, VendorDescriptor, VendorGuest,
};

pub const VENDOR_ID: &str = "qoder";
pub(crate) const PROTOCOL: &str = "openai-compatible/chat-completions/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Region {
    Cn,
    Global,
}

impl Region {
    pub(crate) fn from_channel(channel: &str) -> Result<Self, PluginError> {
        match channel {
            "cn" => Ok(Self::Cn),
            "global" => Ok(Self::Global),
            _ => Err(common::unsupported("channel", VENDOR_ID, channel)),
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Cn => "cn",
            Self::Global => "global",
        }
    }

    pub(crate) fn infer_origin(self) -> &'static str {
        match self {
            Self::Cn => "https://gateway.qoder.com.cn",
            Self::Global => "https://api2.qoder.sh",
        }
    }

    pub(crate) fn openapi_origin(self) -> &'static str {
        match self {
            Self::Cn => "https://openapi.qoder.com.cn",
            Self::Global => "https://openapi.qoder.sh",
        }
    }

    pub(crate) fn center_origin(self) -> &'static str {
        match self {
            Self::Cn => self.infer_origin(),
            Self::Global => "https://center.qoder.sh",
        }
    }

    pub(crate) fn website(self) -> &'static str {
        match self {
            Self::Cn => "https://qoder.cn",
            Self::Global => "https://qoder.com",
        }
    }
}

pub fn descriptor() -> VendorDescriptor {
    profile::descriptor()
}

fn admit(channel: &str, provider: &ProviderSnapshot) -> Result<Region, PluginError> {
    if provider.provider_id != VENDOR_ID || provider.channel != channel {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Qoder provider/channel mismatch",
        ));
    }
    let region = Region::from_channel(channel)?;
    // 推理地址和凭据的区域必须同时匹配；不允许用连接地址覆盖可信上游。
    if provider.base_url.trim_end_matches('/') != region.infer_origin() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Qoder base URL does not match the selected region",
        ));
    }
    Ok(region)
}

pub struct Qoder;

impl VendorGuest for Qoder {
    fn descriptor() -> VendorDescriptor {
        descriptor()
    }

    fn select_protocol(
        operation: Operation,
        channel: &str,
        provider: &ProviderSnapshot,
        _request: &AiRequest,
    ) -> Result<String, PluginError> {
        admit(channel, provider)?;
        if operation != Operation::Infer {
            return Err(common::unsupported(operation.as_str(), VENDOR_ID, channel));
        }
        Ok(PROTOCOL.into())
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        if operation != input.operation() {
            return Err(common::plugin_error(
                ErrorKind::Invalid,
                "operation/input mismatch",
            ));
        }
        let region = admit(channel, input.provider())?;
        match input {
            OperationInput::Infer { provider, request } => {
                inference::execute(host, &provider, region, request)
            }
            OperationInput::Auth { provider, request } => {
                auth::execute(host, &provider, region, request).map(OperationOutput::Auth)
            }
            OperationInput::Discover { provider, request } => {
                models::discover(host, &provider, region, request).map(OperationOutput::Discover)
            }
            OperationInput::Allowance {
                provider,
                request: _,
            } => allowance::execute(host, &provider, region).map(OperationOutput::Allowance),
            OperationInput::ConfigValidation {
                provider: _,
                request,
            } => Ok(OperationOutput::ConfigValidation(models::validate(
                &request.options,
            ))),
            _ => Err(common::unsupported(operation.as_str(), VENDOR_ID, channel)),
        }
    }
}

#[cfg(target_arch = "wasm32")]
stravia_vendor_sdk::export_vendor!(Qoder);
