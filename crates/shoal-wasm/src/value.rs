//! Versioned, bounded value envelope at the guest ABI boundary.

use super::abi::shoal::plugin::types::{GuestValue, ValueKind};
use super::{ABI_VERSION, PluginError};

#[derive(Debug, Clone, PartialEq)]
pub enum PluginValue {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
}

impl PluginValue {
    pub(super) fn into_guest(self, max_bytes: usize) -> Result<GuestValue, PluginError> {
        let (kind, payload) = match self {
            Self::Null => (ValueKind::Null, Vec::new()),
            Self::Bool(value) => (ValueKind::Boolean, vec![u8::from(value)]),
            Self::Signed(value) => (ValueKind::Signed, value.to_le_bytes().to_vec()),
            Self::Unsigned(value) => (ValueKind::Unsigned, value.to_le_bytes().to_vec()),
            Self::Float(value) => (ValueKind::Float, value.to_le_bytes().to_vec()),
            Self::Text(value) => (ValueKind::Text, value.into_bytes()),
            Self::Bytes(value) => (ValueKind::Bytes, value),
            Self::Json(value) => (
                ValueKind::Json,
                serde_json::to_vec(&value)
                    .map_err(|error| PluginError::Value(error.to_string()))?,
            ),
        };
        if payload.len() > max_bytes {
            return Err(PluginError::Value(format!(
                "plugin value exceeds the {max_bytes}-byte limit"
            )));
        }
        Ok(GuestValue {
            abi_version: ABI_VERSION,
            kind,
            payload,
        })
    }

    pub(super) fn from_guest(value: GuestValue, max_bytes: usize) -> Result<Self, PluginError> {
        if value.abi_version != ABI_VERSION {
            return Err(PluginError::Value(format!(
                "guest value uses ABI version {}; expected {ABI_VERSION}",
                value.abi_version
            )));
        }
        if value.payload.len() > max_bytes {
            return Err(PluginError::Value(format!(
                "guest value exceeds the {max_bytes}-byte limit"
            )));
        }
        let exact = |size: usize| {
            if value.payload.len() == size {
                Ok(())
            } else {
                Err(PluginError::Value(format!(
                    "{:?} payload must be exactly {size} bytes",
                    value.kind
                )))
            }
        };
        match value.kind {
            ValueKind::Null => {
                exact(0)?;
                Ok(Self::Null)
            }
            ValueKind::Boolean => {
                exact(1)?;
                match value.payload[0] {
                    0 => Ok(Self::Bool(false)),
                    1 => Ok(Self::Bool(true)),
                    other => Err(PluginError::Value(format!(
                        "boolean payload must be 0 or 1, got {other}"
                    ))),
                }
            }
            ValueKind::Signed => {
                exact(8)?;
                Ok(Self::Signed(i64::from_le_bytes(
                    value.payload.try_into().unwrap(),
                )))
            }
            ValueKind::Unsigned => {
                exact(8)?;
                Ok(Self::Unsigned(u64::from_le_bytes(
                    value.payload.try_into().unwrap(),
                )))
            }
            ValueKind::Float => {
                exact(8)?;
                Ok(Self::Float(f64::from_le_bytes(
                    value.payload.try_into().unwrap(),
                )))
            }
            ValueKind::Text => String::from_utf8(value.payload)
                .map(Self::Text)
                .map_err(|error| PluginError::Value(format!("text payload is not UTF-8: {error}"))),
            ValueKind::Bytes => Ok(Self::Bytes(value.payload)),
            ValueKind::Json => serde_json::from_slice(&value.payload)
                .map(Self::Json)
                .map_err(|error| PluginError::Value(format!("invalid JSON payload: {error}"))),
        }
    }
}
