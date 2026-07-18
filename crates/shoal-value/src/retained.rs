//! Non-allocating structural measurement for values retained by long-lived
//! runtime state (channel rings, lexical environments, caches).

use crate::{Record, Value};

#[derive(Debug, Clone, Copy)]
pub enum OpaqueHandling {
    Reject,
    /// Charge a fixed amount. The subsystem must separately bound state owned
    /// behind the handle.
    Charge(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct RetainedLimits {
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_nodes: usize,
    pub opaque: OpaqueHandling,
    pub allow_secret: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainedError {
    Bytes { measured: usize, max: usize },
    Depth { measured: usize, max: usize },
    Nodes { measured: usize, max: usize },
    Opaque(&'static str),
    Secret,
    AccountingOverflow,
}

/// Measure `value` without allocating or entering opaque runtime handles.
/// Recursion stops at `max_depth + 1`, so hostile pre-existing values cannot
/// make this walk recurse without bound.
pub fn retained_size(value: &Value, limits: RetainedLimits) -> Result<usize, RetainedError> {
    let mut measure = Measure {
        limits,
        bytes: 0,
        nodes: 0,
    };
    measure.visit(value, 1)?;
    Ok(measure.bytes)
}

struct Measure {
    limits: RetainedLimits,
    bytes: usize,
    nodes: usize,
}

impl Measure {
    fn add_bytes(&mut self, bytes: usize) -> Result<(), RetainedError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(RetainedError::AccountingOverflow)?;
        if self.bytes > self.limits.max_bytes {
            return Err(RetainedError::Bytes {
                measured: self.bytes,
                max: self.limits.max_bytes,
            });
        }
        Ok(())
    }

    fn enter_node(&mut self, depth: usize, resident_bytes: usize) -> Result<(), RetainedError> {
        if depth > self.limits.max_depth {
            return Err(RetainedError::Depth {
                measured: depth,
                max: self.limits.max_depth,
            });
        }
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(RetainedError::AccountingOverflow)?;
        if self.nodes > self.limits.max_nodes {
            return Err(RetainedError::Nodes {
                measured: self.nodes,
                max: self.limits.max_nodes,
            });
        }
        self.add_bytes(resident_bytes)
    }

    fn opaque(&mut self, kind: &'static str) -> Result<(), RetainedError> {
        match self.limits.opaque {
            OpaqueHandling::Reject => Err(RetainedError::Opaque(kind)),
            OpaqueHandling::Charge(bytes) => self.add_bytes(bytes),
        }
    }

    fn visit(&mut self, value: &Value, depth: usize) -> Result<(), RetainedError> {
        self.enter_node(depth, std::mem::size_of::<Value>())?;
        match value {
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Size(_)
            | Value::Duration(_)
            | Value::Time(_)
            | Value::Range(_) => Ok(()),
            Value::DateTime(_) => self.add_bytes(512),
            Value::Str(text) => self.add_bytes(text.capacity()),
            Value::Path(path) => self.add_bytes(path.as_os_str().as_encoded_bytes().len()),
            Value::Glob(glob) => {
                self.add_bytes(glob.pattern.capacity())?;
                self.add_bytes(glob.cwd.as_os_str().as_encoded_bytes().len())
            }
            Value::Bytes(bytes) => self.add_bytes(bytes.capacity()),
            Value::List(values) => {
                self.add_bytes(
                    values
                        .capacity()
                        .saturating_sub(values.len())
                        .saturating_mul(std::mem::size_of::<Value>()),
                )?;
                for value in values {
                    self.visit(value, depth + 1)?;
                }
                Ok(())
            }
            Value::Record(record) => self.visit_record(record, depth),
            Value::Table(rows) => {
                self.add_bytes(
                    rows.capacity()
                        .saturating_mul(std::mem::size_of::<Record>()),
                )?;
                for row in rows {
                    self.enter_node(depth + 1, std::mem::size_of::<Record>())?;
                    self.visit_record(row, depth + 1)?;
                }
                Ok(())
            }
            Value::Error(error) => {
                self.add_bytes(std::mem::size_of_val(error.as_ref()))?;
                self.add_bytes(error.code.capacity())?;
                self.add_bytes(error.msg.capacity())?;
                if let Some(hint) = &error.hint {
                    self.add_bytes(hint.capacity())?;
                }
                if let Some(stderr) = &error.stderr {
                    self.add_bytes(stderr.capacity())?;
                }
                Ok(())
            }
            Value::Outcome(outcome) => {
                self.add_bytes(std::mem::size_of_val(outcome.as_ref()))?;
                self.add_bytes(outcome.stdout.capacity())?;
                self.add_bytes(outcome.stderr.capacity())?;
                self.add_bytes(outcome.cmd.capacity())?;
                if let Some(signal) = &outcome.signal {
                    self.add_bytes(signal.capacity())?;
                }
                if outcome.stdout_ref.is_some() {
                    self.opaque("CAS-backed outcome")?;
                }
                if let Some(parsed) = &outcome.parsed {
                    self.visit(parsed, depth + 1)?;
                }
                Ok(())
            }
            Value::Secret(secret) => {
                if !self.limits.allow_secret {
                    return Err(RetainedError::Secret);
                }
                self.add_bytes(secret.name.capacity())?;
                self.add_bytes(secret.value.len())
            }
            Value::CasBytes(bytes) => {
                self.add_bytes(bytes.hash.capacity())?;
                self.add_bytes(bytes.preview.capacity())?;
                self.opaque("CAS-backed bytes")
            }
            Value::Regex(regex) => {
                self.add_bytes(regex.src.capacity())?;
                self.opaque("compiled regex")
            }
            Value::Stream(_) => self.opaque("stream"),
            Value::Task(_) => self.opaque("task"),
            Value::Closure(closure) => {
                if let Some(name) = &closure.name {
                    self.add_bytes(name.capacity())?;
                }
                if let Some(doc) = &closure.doc {
                    self.add_bytes(doc.capacity())?;
                }
                self.opaque("closure")
            }
            Value::CmdRef(_) => self.opaque("command reference"),
        }
    }

    fn visit_record(&mut self, record: &Record, depth: usize) -> Result<(), RetainedError> {
        self.add_bytes(
            record
                .capacity()
                .saturating_mul(std::mem::size_of::<(String, Value)>()),
        )?;
        for (key, value) in record {
            self.add_bytes(key.capacity())?;
            self.visit(value, depth + 1)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: RetainedLimits = RetainedLimits {
        max_bytes: 1024,
        max_depth: 8,
        max_nodes: 16,
        opaque: OpaqueHandling::Reject,
        allow_secret: false,
    };

    #[test]
    fn hostile_depth_and_width_stop_at_typed_limits() {
        let mut deep = Value::Null;
        for _ in 0..9 {
            deep = Value::List(vec![deep]);
        }
        assert!(matches!(
            retained_size(&deep, LIMITS),
            Err(RetainedError::Depth { .. })
        ));
        assert!(matches!(
            retained_size(&Value::List(vec![Value::Null; 17]), LIMITS),
            Err(RetainedError::Nodes { .. }) | Err(RetainedError::Bytes { .. })
        ));
    }
}
