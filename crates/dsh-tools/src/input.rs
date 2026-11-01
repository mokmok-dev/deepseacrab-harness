//! Argument decoding shared by the built-in tools.

use dsh_core::ToolOutcome;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, ToolOutcome> {
    serde_json::from_value(arguments)
        .map_err(|error| ToolOutcome::error(format!("invalid arguments: {error}")))
}
