//! Shared admission guards for eager value-method results.
//!
//! Environment quotas protect values once they are bound. These builders
//! protect the transient result while a pure method is still constructing it.

use super::*;

pub(crate) const EAGER_COLLECTION_MAX_VALUES: usize = 16_384;
pub(crate) const EAGER_COLLECTION_MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const EAGER_STRING_MAX_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct MaterializedCollection {
    values: Vec<Value>,
    budget: MaterializationBudget,
}

pub(crate) struct MaterializationBudget {
    values: usize,
    retained_bytes: usize,
    max_values: usize,
    max_retained_bytes: usize,
}

impl MaterializedCollection {
    pub(crate) fn eager() -> Self {
        Self::new(
            EAGER_COLLECTION_MAX_VALUES,
            EAGER_COLLECTION_MAX_RETAINED_BYTES,
        )
    }

    pub(crate) fn new(max_values: usize, max_retained_bytes: usize) -> Self {
        Self {
            values: Vec::new(),
            budget: MaterializationBudget::new(max_values, max_retained_bytes),
        }
    }

    pub(crate) fn push(&mut self, value: Value) -> VResult<()> {
        self.budget.admit(&value)?;
        self.values.push(value);
        Ok(())
    }

    pub(crate) fn extend(&mut self, values: impl IntoIterator<Item = Value>) -> VResult<()> {
        for value in values {
            self.push(value)?;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Value {
        Value::List(self.values)
    }

    pub(crate) fn finish_vec(self) -> Vec<Value> {
        self.values
    }

    pub(crate) fn values(&self) -> &[Value] {
        &self.values
    }
}

impl MaterializationBudget {
    pub(crate) fn eager() -> Self {
        Self::new(
            EAGER_COLLECTION_MAX_VALUES,
            EAGER_COLLECTION_MAX_RETAINED_BYTES,
        )
    }

    pub(crate) fn new(max_values: usize, max_retained_bytes: usize) -> Self {
        Self {
            values: 0,
            retained_bytes: 0,
            max_values,
            max_retained_bytes,
        }
    }

    pub(crate) fn admit(&mut self, value: &Value) -> VResult<()> {
        if self.values >= self.max_values {
            return Err(collection_materialization_limit(format!(
                "eager collection reached its {}-value limit",
                self.max_values
            )));
        }
        self.charge_retained(value)?;
        self.values += 1;
        Ok(())
    }

    pub(crate) fn charge_retained(&mut self, value: &Value) -> VResult<()> {
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
            collection_materialization_limit(format!(
                "eager collection exceeds its {}-byte retained-value limit",
                self.max_retained_bytes
            ))
        })?;
        self.retained_bytes = self.retained_bytes.checked_add(retained).ok_or_else(|| {
            collection_materialization_limit("eager collection accounting overflowed")
        })?;
        Ok(())
    }
}

pub(crate) struct BoundedString {
    value: String,
    max_bytes: usize,
}

impl BoundedString {
    pub(crate) fn eager() -> Self {
        Self::new(EAGER_STRING_MAX_BYTES)
    }

    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            value: String::new(),
            max_bytes,
        }
    }

    pub(crate) fn push_str(&mut self, value: &str) -> VResult<()> {
        let next = self
            .value
            .len()
            .checked_add(value.len())
            .ok_or_else(|| string_materialization_limit(self.max_bytes))?;
        if next > self.max_bytes {
            return Err(string_materialization_limit(self.max_bytes));
        }
        self.value.push_str(value);
        Ok(())
    }

    pub(crate) fn push_char(&mut self, value: char) -> VResult<()> {
        let mut encoded = [0; 4];
        self.push_str(value.encode_utf8(&mut encoded))
    }

    pub(crate) fn finish(self) -> Value {
        Value::Str(self.value)
    }
}

fn collection_materialization_limit(message: impl Into<String>) -> ErrorVal {
    ErrorVal::new("collection_materialization_limit", message)
        .with_hint("use `.stream()` with incremental transforms/sinks, or reduce the input first")
}

fn string_materialization_limit(max_bytes: usize) -> ErrorVal {
    ErrorVal::new(
        "string_materialization_limit",
        format!("eager string result exceeds its {max_bytes}-byte limit"),
    )
    .with_hint("stream or chunk the input, or reduce it before constructing the result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_string_rejects_before_appending_over_limit() {
        let mut out = BoundedString::new(4);
        out.push_str("sho").unwrap();
        assert_eq!(
            out.push_str("al").unwrap_err().code,
            "string_materialization_limit"
        );
    }
}
