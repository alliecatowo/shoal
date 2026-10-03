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
    pub(super) fn into_guest(
        self,
        max_bytes: usize,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<GuestValue, PluginError> {
        let (kind, payload) = match self {
            Self::Null => (ValueKind::Null, Vec::new()),
            Self::Bool(value) => (ValueKind::Boolean, vec![u8::from(value)]),
            Self::Signed(value) => (ValueKind::Signed, value.to_le_bytes().to_vec()),
            Self::Unsigned(value) => (ValueKind::Unsigned, value.to_le_bytes().to_vec()),
            Self::Float(value) => (ValueKind::Float, value.to_le_bytes().to_vec()),
            Self::Text(value) => (ValueKind::Text, value.into_bytes()),
            Self::Bytes(value) => (ValueKind::Bytes, value),
            Self::Json(value) => {
                validate_json(&value, max_bytes, max_depth, max_nodes)?;
                (
                    ValueKind::Json,
                    serde_json::to_vec(&value)
                        .map_err(|error| PluginError::Value(error.to_string()))?,
                )
            }
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

    pub(super) fn from_guest(
        value: GuestValue,
        max_bytes: usize,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<Self, PluginError> {
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
                let bytes = value.payload.try_into().map_err(|_| {
                    PluginError::Value("signed payload length changed during validation".into())
                })?;
                Ok(Self::Signed(i64::from_le_bytes(bytes)))
            }
            ValueKind::Unsigned => {
                exact(8)?;
                let bytes = value.payload.try_into().map_err(|_| {
                    PluginError::Value("unsigned payload length changed during validation".into())
                })?;
                Ok(Self::Unsigned(u64::from_le_bytes(bytes)))
            }
            ValueKind::Float => {
                exact(8)?;
                let bytes = value.payload.try_into().map_err(|_| {
                    PluginError::Value("float payload length changed during validation".into())
                })?;
                Ok(Self::Float(f64::from_le_bytes(bytes)))
            }
            ValueKind::Text => String::from_utf8(value.payload)
                .map(Self::Text)
                .map_err(|error| PluginError::Value(format!("text payload is not UTF-8: {error}"))),
            ValueKind::Bytes => Ok(Self::Bytes(value.payload)),
            ValueKind::Json => {
                let json = serde_json::from_slice(&value.payload).map_err(|error| {
                    PluginError::Value(format!("invalid JSON payload: {error}"))
                })?;
                validate_json(&json, max_bytes, max_depth, max_nodes)?;
                Ok(Self::Json(json))
            }
        }
    }
}

fn validate_json(
    value: &serde_json::Value,
    max_bytes: usize,
    max_depth: usize,
    max_nodes: usize,
) -> Result<(), PluginError> {
    struct Measure {
        bytes: usize,
        nodes: usize,
    }

    fn add(measure: &mut Measure, amount: usize, max_bytes: usize) -> Result<(), PluginError> {
        measure.bytes = measure
            .bytes
            .checked_add(amount)
            .ok_or_else(|| PluginError::Value("plugin JSON size accounting overflowed".into()))?;
        if measure.bytes > max_bytes {
            return Err(PluginError::Value(format!(
                "plugin JSON exceeds the {max_bytes}-byte limit"
            )));
        }
        Ok(())
    }

    fn visit(
        value: &serde_json::Value,
        depth: usize,
        measure: &mut Measure,
        max_bytes: usize,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<(), PluginError> {
        if depth > max_depth {
            return Err(PluginError::Value(format!(
                "plugin JSON exceeds the {max_depth}-level depth limit"
            )));
        }
        measure.nodes = measure
            .nodes
            .checked_add(1)
            .ok_or_else(|| PluginError::Value("plugin JSON node accounting overflowed".into()))?;
        if measure.nodes > max_nodes {
            return Err(PluginError::Value(format!(
                "plugin JSON exceeds the {max_nodes}-node limit"
            )));
        }
        match value {
            serde_json::Value::Null => add(measure, 4, max_bytes),
            serde_json::Value::Bool(_) => add(measure, 5, max_bytes),
            serde_json::Value::Number(_) => add(measure, 32, max_bytes),
            // Six bytes per source byte is a conservative upper bound for JSON
            // string escaping, plus the surrounding quotes.
            serde_json::Value::String(value) => add(
                measure,
                value.len().saturating_mul(6).saturating_add(2),
                max_bytes,
            ),
            serde_json::Value::Array(values) => {
                add(measure, values.len().saturating_add(2), max_bytes)?;
                for value in values {
                    visit(value, depth + 1, measure, max_bytes, max_depth, max_nodes)?;
                }
                Ok(())
            }
            serde_json::Value::Object(values) => {
                add(measure, values.len().saturating_add(2), max_bytes)?;
                for (key, value) in values {
                    add(
                        measure,
                        key.len().saturating_mul(6).saturating_add(3),
                        max_bytes,
                    )?;
                    visit(value, depth + 1, measure, max_bytes, max_depth, max_nodes)?;
                }
                Ok(())
            }
        }
    }

    let mut measure = Measure { bytes: 0, nodes: 0 };
    visit(value, 1, &mut measure, max_bytes, max_depth, max_nodes)
}
