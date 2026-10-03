//! Incremental admission for builtin results constructed before they reach a
//! lexical binding or wire elision boundary.

use shoal_value::{OpaqueHandling, Record, RetainedLimits, VResult, Value, retained_size};

pub(crate) const MAX_VALUES: usize = 16_384;
pub(crate) const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct OutputValues {
    values: Vec<Value>,
    retained_bytes: usize,
    max_values: usize,
    max_retained_bytes: usize,
}

impl OutputValues {
    pub(crate) fn new() -> Self {
        Self::with_limits(MAX_VALUES, MAX_RETAINED_BYTES)
    }

    pub(crate) fn with_limits(max_values: usize, max_retained_bytes: usize) -> Self {
        Self {
            values: Vec::new(),
            retained_bytes: 0,
            max_values,
            max_retained_bytes,
        }
    }

    pub(crate) fn remaining_values(&self) -> usize {
        self.max_values.saturating_sub(self.values.len())
    }

    pub(crate) fn remaining_retained_bytes(&self) -> usize {
        self.max_retained_bytes.saturating_sub(self.retained_bytes)
    }

    pub(crate) fn push(&mut self, value: Value) -> VResult<()> {
        if self.values.len() >= self.max_values {
            return Err(output_limit(format!(
                "builtin result reached its {}-value limit",
                self.max_values
            )));
        }
        let retained = retained_size(
            &value,
            RetainedLimits {
                max_bytes: self.max_retained_bytes.saturating_sub(self.retained_bytes),
                max_depth: 64,
                max_nodes: 16_384,
                opaque: OpaqueHandling::Charge(256),
                allow_secret: true,
            },
        )
        .map_err(|_| {
            output_limit(format!(
                "builtin result exceeds its {}-byte retained-value limit",
                self.max_retained_bytes
            ))
        })?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(retained)
            .ok_or_else(|| output_limit("builtin result accounting overflowed"))?;
        self.values.push(value);
        Ok(())
    }

    pub(crate) fn finish_list(self) -> Value {
        Value::List(self.values)
    }

    pub(crate) fn finish_table(self) -> Value {
        Value::Table(
            self.values
                .into_iter()
                .map(|value| match value {
                    Value::Record(record) => record,
                    _ => unreachable!("table admission only accepts records"),
                })
                .collect(),
        )
    }
}

pub(crate) struct OutputBudget {
    values: usize,
    retained_bytes: usize,
    max_values: usize,
    max_retained_bytes: usize,
}

impl OutputBudget {
    pub(crate) fn new() -> Self {
        Self::with_limits(MAX_VALUES, MAX_RETAINED_BYTES)
    }

    pub(crate) fn with_limits(max_values: usize, max_retained_bytes: usize) -> Self {
        Self {
            values: 0,
            retained_bytes: 0,
            max_values,
            max_retained_bytes,
        }
    }

    pub(crate) fn admit_value(&mut self, value: &Value) -> VResult<()> {
        if self.values >= self.max_values {
            return Err(output_limit(format!(
                "builtin result reached its {}-value limit",
                self.max_values
            )));
        }
        let retained = retained_size(
            value,
            RetainedLimits {
                max_bytes: self.max_retained_bytes.saturating_sub(self.retained_bytes),
                max_depth: 64,
                max_nodes: 16_384,
                opaque: OpaqueHandling::Charge(256),
                allow_secret: true,
            },
        )
        .map_err(|_| {
            output_limit(format!(
                "builtin result exceeds its {}-byte retained-value limit",
                self.max_retained_bytes
            ))
        })?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(retained)
            .ok_or_else(|| output_limit("builtin result accounting overflowed"))?;
        self.values += 1;
        Ok(())
    }

    pub(crate) fn admit_record_entry(&mut self, key: &str, value: &Value) -> VResult<()> {
        self.admit_value(&Value::List(vec![
            Value::Str(key.to_owned()),
            value.clone(),
        ]))
    }
}

pub(super) struct OutputString {
    value: String,
    max_bytes: usize,
}

impl OutputString {
    pub(super) fn new() -> Self {
        Self::with_limit(MAX_RETAINED_BYTES)
    }

    pub(super) fn with_limit(max_bytes: usize) -> Self {
        Self {
            value: String::new(),
            max_bytes,
        }
    }

    pub(super) fn push_str(&mut self, chunk: &str) -> VResult<()> {
        let next = self
            .value
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| output_limit("builtin string result length overflowed"))?;
        if next > self.max_bytes {
            return Err(output_limit(format!(
                "builtin string result exceeds its {}-byte limit",
                self.max_bytes
            )));
        }
        self.value.push_str(chunk);
        Ok(())
    }

    pub(super) fn finish(self) -> String {
        self.value
    }
}

pub(crate) fn output_limit(message: impl Into<String>) -> shoal_value::ErrorVal {
    shoal_value::ErrorVal::new("builtin_output_limit", message)
        .with_hint("narrow the input, process it as a stream, or request a bounded subset")
}

pub(crate) fn table_record(value: Record) -> Value {
    Value::Record(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_and_byte_walls_reject_before_retention() {
        let mut values = OutputValues::with_limits(2, 1024);
        values.push(Value::Int(1)).unwrap();
        values.push(Value::Int(2)).unwrap();
        assert_eq!(
            values.push(Value::Int(3)).unwrap_err().code,
            "builtin_output_limit"
        );

        let mut values = OutputValues::with_limits(8, 32);
        assert_eq!(
            values.push(Value::Str("x".repeat(64))).unwrap_err().code,
            "builtin_output_limit"
        );

        let mut string = OutputString::with_limit(3);
        string.push_str("abc").unwrap();
        assert_eq!(
            string.push_str("d").unwrap_err().code,
            "builtin_output_limit"
        );
        assert_eq!(string.finish(), "abc");
    }
}
