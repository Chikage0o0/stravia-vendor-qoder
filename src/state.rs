use serde_json::{Map, Value};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, GuestHost, PluginError};

pub(crate) fn read(host: &GuestHost) -> Result<Map<String, Value>, PluginError> {
    let Some(bytes) = host.read_private_state()?.filter(|bytes| !bytes.is_empty()) else {
        return Ok(Map::new());
    };
    serde_json::from_slice(&bytes).map_err(|_| {
        common::plugin_error(
            ErrorKind::Invalid,
            "stored Qoder private state is malformed",
        )
    })
}

pub(crate) fn write(host: &GuestHost, root: Map<String, Value>) -> Result<(), PluginError> {
    if root.is_empty() {
        return host.write_private_state(b"");
    }
    let bytes = serde_json::to_vec(&root).map_err(|_| {
        common::plugin_error(
            ErrorKind::Invalid,
            "Qoder private state could not be encoded",
        )
    })?;
    host.write_private_state(&bytes)
}
