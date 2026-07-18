//! Lexical scopes (`Env`/`Binding`), moved verbatim out of `lib.rs`.

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

pub const ENV_BINDING_NAME_BYTES: usize = 256;
pub const ENV_BINDING_IDENTITY_CAP: usize = 4096;
// Large command tables are retained in the implicit `it`/`out` bindings. An
// outcome can retain both the kernel's ordinary 1 MiB captured-output envelope
// and its parsed table, plus the value envelope, so keep one binding above that
// doubled representation. The independent aggregate wall still prevents a
// session from retaining an unbounded sequence of maximum-sized values.
pub const ENV_BINDING_VALUE_BYTES: usize = 4 * 1024 * 1024;
pub const ENV_BINDING_AGGREGATE_BYTES: usize = 16 * 1024 * 1024;
pub const ENV_BINDING_VALUE_DEPTH: usize = 64;
pub const ENV_BINDING_VALUE_NODES: usize = 16 * 1024;
const ENV_OPAQUE_HANDLE_CHARGE: usize = 64 * 1024;

const ENV_RETAINED_LIMITS: RetainedLimits = RetainedLimits {
    max_bytes: ENV_BINDING_VALUE_BYTES,
    max_depth: ENV_BINDING_VALUE_DEPTH,
    max_nodes: ENV_BINDING_VALUE_NODES,
    opaque: OpaqueHandling::Charge(ENV_OPAQUE_HANDLE_CHARGE),
    allow_secret: true,
};

#[derive(Debug, Clone)]
pub struct Env {
    inner: Arc<Mutex<EnvInner>>,
}

#[derive(Debug)]
struct EnvInner {
    vars: HashMap<String, Binding>,
    parent: Option<Env>,
    budget: Arc<Mutex<EnvBudget>>,
}

#[derive(Debug, Default)]
struct EnvBudget {
    identities: usize,
    retained_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
    value_bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssignError {
    NotFound,
    Immutable,
    Limit(ErrorVal),
}

impl Env {
    /// Recover a lexical scope after an unwind while preserving its bindings.
    ///
    /// Every mutation in this module is a single `HashMap` operation or a
    /// `Binding` value replacement. Rust's containers remain structurally
    /// valid if hashing, allocation, or value code unwinds, and `parent` is
    /// never mutated after construction. The guarded graph can therefore be
    /// reused; unlike execution state, there is no half-advanced cursor or
    /// external side effect to quarantine.
    fn lock_inner(&self) -> MutexGuard<'_, EnvInner> {
        match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => {
                let inner = poisoned.into_inner();
                self.inner.clear_poison();
                inner
            }
        }
    }

    pub fn root() -> Env {
        let budget = Arc::new(Mutex::new(EnvBudget::default()));
        Env {
            inner: Arc::new(Mutex::new(EnvInner {
                vars: HashMap::new(),
                parent: None,
                budget,
            })),
        }
    }

    pub fn child(&self) -> Env {
        let budget = self.lock_inner().budget.clone();
        Env {
            inner: Arc::new(Mutex::new(EnvInner {
                vars: HashMap::new(),
                parent: Some(self.clone()),
                budget,
            })),
        }
    }

    /// Create a fresh lexical root that cannot see this environment's names,
    /// while keeping all retained bindings on the same session budget.
    /// Script and module evaluators use this instead of starting an unrelated
    /// budget that an escaping closure could retain independently.
    pub fn isolated(&self) -> Env {
        let budget = self.lock_inner().budget.clone();
        Env {
            inner: Arc::new(Mutex::new(EnvInner {
                vars: HashMap::new(),
                parent: None,
                budget,
            })),
        }
    }

    pub fn declare(&self, name: impl Into<String>, value: Value, mutable: bool) -> VResult<()> {
        let name = name.into();
        validate_binding_name(&name)?;
        let value_bytes = measure_binding_value(&value)?;
        let mut inner = self.lock_inner();
        let shared_budget = Arc::clone(&inner.budget);

        if let Some(binding) = inner.vars.get_mut(&name) {
            let mut budget = lock_budget(&shared_budget);
            let next = budget
                .retained_bytes
                .saturating_sub(binding.value_bytes)
                .checked_add(value_bytes)
                .ok_or_else(binding_accounting_overflow)?;
            ensure_aggregate_bytes(next)?;
            budget.retained_bytes = next;
            binding.value = value;
            binding.mutable = mutable;
            binding.value_bytes = value_bytes;
            return Ok(());
        }

        inner.vars.try_reserve(1).map_err(|error| {
            ErrorVal::new(
                "binding_aggregate_limit",
                format!("cannot reserve another environment binding: {error}"),
            )
        })?;
        let charge = binding_charge(&name, value_bytes);
        let mut budget = lock_budget(&shared_budget);
        if budget.identities >= ENV_BINDING_IDENTITY_CAP {
            return Err(ErrorVal::new(
                "binding_identity_limit",
                format!(
                    "environment binding identity limit ({ENV_BINDING_IDENTITY_CAP}) reached; replace or remove a binding before declaring another"
                ),
            ));
        }
        let next = budget
            .retained_bytes
            .checked_add(charge)
            .ok_or_else(binding_accounting_overflow)?;
        ensure_aggregate_bytes(next)?;
        budget.identities += 1;
        budget.retained_bytes = next;
        inner.vars.insert(
            name,
            Binding {
                value,
                mutable,
                value_bytes,
            },
        );
        Ok(())
    }

    /// Validate and publish several bindings as one environment transaction.
    /// Either every binding and its shared-budget charge is installed, or the
    /// environment remains byte-for-byte unchanged.
    pub fn declare_many(&self, bindings: Vec<(String, Value, bool)>) -> VResult<()> {
        let mut measured = Vec::new();
        measured.try_reserve(bindings.len()).map_err(|error| {
            ErrorVal::new(
                "binding_aggregate_limit",
                format!("cannot stage environment binding transaction: {error}"),
            )
        })?;
        for (index, (name, value, mutable)) in bindings.into_iter().enumerate() {
            validate_binding_name(&name)?;
            if measured
                .iter()
                .take(index)
                .any(|(existing, _, _, _)| existing == &name)
            {
                return Err(ErrorVal::new(
                    "binding_transaction",
                    format!("binding transaction contains duplicate name `{name}`"),
                ));
            }
            let value_bytes = measure_binding_value(&value)?;
            measured.push((name, value, mutable, value_bytes));
        }

        let mut inner = self.lock_inner();
        let shared_budget = Arc::clone(&inner.budget);
        let new_identities = measured
            .iter()
            .filter(|(name, _, _, _)| !inner.vars.contains_key(name))
            .count();
        inner.vars.try_reserve(new_identities).map_err(|error| {
            ErrorVal::new(
                "binding_aggregate_limit",
                format!("cannot reserve environment binding transaction: {error}"),
            )
        })?;

        let mut budget = lock_budget(&shared_budget);
        let next_identities = budget
            .identities
            .checked_add(new_identities)
            .ok_or_else(binding_accounting_overflow)?;
        if next_identities > ENV_BINDING_IDENTITY_CAP {
            return Err(ErrorVal::new(
                "binding_identity_limit",
                format!(
                    "environment binding identity limit ({ENV_BINDING_IDENTITY_CAP}) reached; replace or remove a binding before declaring another"
                ),
            ));
        }

        let mut next_bytes = budget.retained_bytes;
        for (name, _, _, value_bytes) in &measured {
            next_bytes = match inner.vars.get(name) {
                Some(binding) => next_bytes
                    .saturating_sub(binding.value_bytes)
                    .checked_add(*value_bytes),
                None => next_bytes.checked_add(binding_charge(name, *value_bytes)),
            }
            .ok_or_else(binding_accounting_overflow)?;
        }
        ensure_aggregate_bytes(next_bytes)?;

        for (name, value, mutable, value_bytes) in measured {
            inner.vars.insert(
                name,
                Binding {
                    value,
                    mutable,
                    value_bytes,
                },
            );
        }
        budget.identities = next_identities;
        budget.retained_bytes = next_bytes;
        Ok(())
    }

    /// Remove a binding declared in this exact scope, without walking into a
    /// parent. Hosts use this to refresh a mirrored remote-session namespace
    /// without accidentally deleting an inherited language binding.
    pub fn remove_local(&self, name: &str) -> Option<Binding> {
        let mut inner = self.lock_inner();
        let (name, binding) = inner.vars.remove_entry(name)?;
        let mut budget = lock_budget(&inner.budget);
        budget.identities = budget.identities.saturating_sub(1);
        budget.retained_bytes = budget
            .retained_bytes
            .saturating_sub(binding_charge(&name, binding.value_bytes));
        Some(binding)
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let parent = {
            let g = self.lock_inner();
            if let Some(b) = g.vars.get(name) {
                return Some(b.value.clone());
            }
            g.parent.clone()
        };
        parent.and_then(|p| p.get(name))
    }

    pub fn is_bound(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Assign to an existing binding, walking up the scope chain.
    pub fn assign(&self, name: &str, value: Value) -> Result<(), AssignError> {
        let value_bytes = measure_binding_value(&value).map_err(AssignError::Limit)?;
        self.assign_measured(name, value, value_bytes)
    }

    fn assign_measured(
        &self,
        name: &str,
        value: Value,
        value_bytes: usize,
    ) -> Result<(), AssignError> {
        let parent = {
            let mut g = self.lock_inner();
            let shared_budget = Arc::clone(&g.budget);
            if let Some(b) = g.vars.get_mut(name) {
                if !b.mutable {
                    return Err(AssignError::Immutable);
                }
                let mut budget = lock_budget(&shared_budget);
                let next = budget
                    .retained_bytes
                    .saturating_sub(b.value_bytes)
                    .checked_add(value_bytes)
                    .ok_or_else(|| AssignError::Limit(binding_accounting_overflow()))?;
                ensure_aggregate_bytes(next).map_err(AssignError::Limit)?;
                budget.retained_bytes = next;
                b.value = value;
                b.value_bytes = value_bytes;
                return Ok(());
            }
            g.parent.clone()
        };
        match parent {
            Some(p) => p.assign_measured(name, value, value_bytes),
            None => Err(AssignError::NotFound),
        }
    }

    /// Snapshot of every visible name (innermost shadowing wins) — for
    /// completion and introspection.
    pub fn visible_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cur = Some(self.clone());
        while let Some(env) = cur {
            let g = env.lock_inner();
            for k in g.vars.keys() {
                if seen.insert(k.clone()) {
                    names.push(k.clone());
                }
            }
            cur = g.parent.clone();
        }
        names
    }
}

impl Drop for EnvInner {
    fn drop(&mut self) {
        let identities = self.vars.len();
        let retained_bytes = self
            .vars
            .iter()
            .map(|(name, binding)| binding_charge(name, binding.value_bytes))
            .fold(0usize, usize::saturating_add);
        let mut budget = lock_budget(&self.budget);
        budget.identities = budget.identities.saturating_sub(identities);
        budget.retained_bytes = budget.retained_bytes.saturating_sub(retained_bytes);
    }
}

fn lock_budget(budget: &Mutex<EnvBudget>) -> MutexGuard<'_, EnvBudget> {
    match budget.lock() {
        Ok(budget) => budget,
        Err(poisoned) => {
            let guard = poisoned.into_inner();
            budget.clear_poison();
            guard
        }
    }
}

fn validate_binding_name(name: &str) -> VResult<()> {
    if name.len() <= ENV_BINDING_NAME_BYTES {
        return Ok(());
    }
    Err(ErrorVal::new(
        "binding_name_limit",
        format!(
            "binding name is {} UTF-8 bytes; maximum is {ENV_BINDING_NAME_BYTES}",
            name.len()
        ),
    ))
}

fn measure_binding_value(value: &Value) -> VResult<usize> {
    retained_size(value, ENV_RETAINED_LIMITS).map_err(|error| {
        let detail = match error {
            RetainedError::Bytes { measured, max } => {
                format!("retains {measured} bytes; maximum per binding is {max}")
            }
            RetainedError::Depth { measured, max } => {
                format!("has depth {measured}; maximum is {max}")
            }
            RetainedError::Nodes { measured, max } => {
                format!("has {measured} nodes; maximum is {max}")
            }
            RetainedError::Opaque(kind) => format!("contains unsupported {kind} state"),
            RetainedError::Secret => "contains a secret disallowed by policy".into(),
            RetainedError::AccountingOverflow => "retained-size accounting overflowed".into(),
        };
        ErrorVal::new(
            "binding_value_limit",
            format!("environment binding value {detail}"),
        )
    })
}

fn binding_charge(name: &str, value_bytes: usize) -> usize {
    name.len()
        .saturating_add(value_bytes)
        .saturating_add(std::mem::size_of::<Binding>())
}

fn ensure_aggregate_bytes(bytes: usize) -> VResult<()> {
    if bytes <= ENV_BINDING_AGGREGATE_BYTES {
        return Ok(());
    }
    Err(ErrorVal::new(
        "binding_aggregate_limit",
        format!(
            "environment bindings would retain {bytes} bytes; session maximum is {ENV_BINDING_AGGREGATE_BYTES}"
        ),
    ))
}

fn binding_accounting_overflow() -> ErrorVal {
    ErrorVal::new(
        "binding_aggregate_limit",
        "environment retained-size accounting overflowed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Barrier;
    use std::thread;

    #[test]
    fn remove_local_never_deletes_an_inherited_binding() {
        let root = Env::root();
        root.declare("shared", Value::Int(1), false).unwrap();
        let child = root.child();
        assert!(child.remove_local("shared").is_none());
        assert!(matches!(root.get("shared"), Some(Value::Int(1))));

        child.declare("shared", Value::Int(2), false).unwrap();
        assert!(child.remove_local("shared").is_some());
        assert!(matches!(child.get("shared"), Some(Value::Int(1))));
    }

    #[test]
    fn poisoned_scope_is_recovered_once_for_waiters_and_future_mutations() {
        let root = Env::root();
        root.declare("before", Value::Int(1), true).unwrap();
        let child = root.child();
        child.declare("local", Value::Int(2), true).unwrap();

        let locked = Arc::clone(&child.inner);
        let held = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let poisoner = {
            let held = Arc::clone(&held);
            let release = Arc::clone(&release);
            thread::spawn(move || {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let _guard = locked.lock().expect("scope starts healthy");
                    held.wait();
                    release.wait();
                    panic!("inject lexical-scope poison");
                }));
            })
        };

        held.wait();
        let waiter = {
            let child = child.clone();
            thread::spawn(move || child.get("local"))
        };
        release.wait();
        poisoner.join().expect("poison injector must be contained");

        assert_eq!(
            waiter.join().expect("waiter must recover"),
            Some(Value::Int(2))
        );
        assert!(!child.inner.is_poisoned());
        assert_eq!(child.assign("local", Value::Int(3)), Ok(()));
        child.declare("after", Value::Int(4), false).unwrap();
        assert_eq!(child.get("local"), Some(Value::Int(3)));
        assert_eq!(child.get("before"), Some(Value::Int(1)));
        assert_eq!(child.get("after"), Some(Value::Int(4)));
        assert_eq!(
            child.remove_local("after").map(|b| b.value),
            Some(Value::Int(4))
        );
        assert!(child.visible_names().contains(&"before".to_owned()));
    }

    #[test]
    fn binding_names_and_unique_identities_are_bounded_without_blocking_replacement() {
        let env = Env::root();
        let oversized = "n".repeat(ENV_BINDING_NAME_BYTES + 1);
        let error = env
            .declare(oversized.clone(), Value::Null, false)
            .unwrap_err();
        assert_eq!(error.code, "binding_name_limit");
        assert!(env.get(&oversized).is_none());

        for index in 0..ENV_BINDING_IDENTITY_CAP {
            env.declare(format!("v{index}"), Value::Int(index as i64), true)
                .unwrap();
        }
        let error = env.declare("one_too_many", Value::Null, false).unwrap_err();
        assert_eq!(error.code, "binding_identity_limit");
        assert!(env.get("one_too_many").is_none());

        env.declare("v0", Value::Str("replacement".into()), false)
            .unwrap();
        assert_eq!(env.get("v0"), Some(Value::Str("replacement".into())));
        assert_eq!(env.assign("v0", Value::Int(9)), Err(AssignError::Immutable));
    }

    #[test]
    fn hostile_binding_values_fail_before_mutating_the_environment() {
        let env = Env::root();
        env.declare("stable", Value::Int(7), true).unwrap();

        let huge = Value::Str("x".repeat(ENV_BINDING_VALUE_BYTES + 1));
        let error = env.declare("huge", huge.clone(), false).unwrap_err();
        assert_eq!(error.code, "binding_value_limit");
        assert!(env.get("huge").is_none());
        assert!(matches!(
            env.assign("stable", huge),
            Err(AssignError::Limit(error)) if error.code == "binding_value_limit"
        ));
        assert_eq!(env.get("stable"), Some(Value::Int(7)));

        let mut deep = Value::Null;
        for _ in 0..=ENV_BINDING_VALUE_DEPTH {
            deep = Value::List(vec![deep]);
        }
        let error = env.declare("deep", deep, false).unwrap_err();
        assert_eq!(error.code, "binding_value_limit");
        assert!(env.get("deep").is_none());

        let wide = Value::List(vec![Value::Null; ENV_BINDING_VALUE_NODES]);
        let error = env.declare("wide", wide, false).unwrap_err();
        assert_eq!(error.code, "binding_value_limit");
        assert!(env.get("wide").is_none());
    }

    #[test]
    fn aggregate_budget_rejects_without_partial_mutation() {
        let env = Env::root();
        let chunk = "x".repeat(900_000);
        let mut accepted = 0usize;
        loop {
            let name = format!("chunk{accepted}");
            match env.declare(name.clone(), Value::Str(chunk.clone()), false) {
                Ok(()) => accepted += 1,
                Err(error) => {
                    assert_eq!(error.code, "binding_aggregate_limit");
                    assert!(env.get(&name).is_none());
                    break;
                }
            }
        }
        assert!(accepted > 1);
        assert!(accepted < ENV_BINDING_IDENTITY_CAP);
        assert_eq!(env.visible_names().len(), accepted);
    }

    #[test]
    fn multi_binding_declarations_are_atomic() {
        let env = Env::root();
        env.declare("first", Value::Int(1), true).unwrap();
        env.declare("second", Value::Int(2), true).unwrap();

        env.declare_many(vec![
            ("first".into(), Value::Int(10), true),
            ("second".into(), Value::Int(20), false),
        ])
        .unwrap();
        assert_eq!(env.get("first"), Some(Value::Int(10)));
        assert_eq!(env.get("second"), Some(Value::Int(20)));

        let huge = Value::Str("x".repeat(ENV_BINDING_VALUE_BYTES + 1));
        let error = env
            .declare_many(vec![
                ("first".into(), Value::Int(99), true),
                ("second".into(), huge, true),
            ])
            .unwrap_err();
        assert_eq!(error.code, "binding_value_limit");
        assert_eq!(env.get("first"), Some(Value::Int(10)));
        assert_eq!(env.get("second"), Some(Value::Int(20)));

        let error = env
            .declare_many(vec![
                ("first".into(), Value::Int(30), true),
                ("first".into(), Value::Int(40), true),
            ])
            .unwrap_err();
        assert_eq!(error.code, "binding_transaction");
        assert_eq!(env.get("first"), Some(Value::Int(10)));
    }

    #[test]
    fn dropping_a_child_scope_refunds_the_shared_session_budget() {
        let root = Env::root();
        let budget = root.lock_inner().budget.clone();
        let baseline = {
            let budget = lock_budget(&budget);
            (budget.identities, budget.retained_bytes)
        };
        {
            let child = root.child();
            for index in 0..128 {
                child
                    .declare(format!("temporary{index}"), Value::Int(index), false)
                    .unwrap();
            }
            let budget = lock_budget(&budget);
            assert_eq!(budget.identities, baseline.0 + 128);
            assert!(budget.retained_bytes > baseline.1);
        }
        let budget = lock_budget(&budget);
        assert_eq!((budget.identities, budget.retained_bytes), baseline);
    }

    #[test]
    fn isolated_roots_hide_names_but_share_and_refund_the_session_budget() {
        let root = Env::root();
        root.declare("private", Value::Int(1), false).unwrap();
        let budget = root.lock_inner().budget.clone();
        let baseline = lock_budget(&budget).identities;
        {
            let isolated = root.isolated();
            assert!(isolated.get("private").is_none());
            isolated
                .declare("module_export", Value::Int(2), false)
                .unwrap();
            assert_eq!(lock_budget(&budget).identities, baseline + 1);
        }
        assert_eq!(lock_budget(&budget).identities, baseline);
    }

    #[test]
    fn bounded_opaque_handles_and_closures_remain_bindable() {
        let env = Env::root();
        let task = TaskVal::new("retained task");
        env.declare("job", Value::Task(task.clone()), false)
            .unwrap();
        assert!(matches!(env.get("job"), Some(Value::Task(found)) if found.id == task.id));

        let captured = Env::root();
        captured.declare("captured", Value::Int(42), false).unwrap();
        let closure = Value::Closure(Arc::new(ClosureVal {
            name: Some("answer".into()),
            params: Vec::new(),
            rest: None,
            ret: None,
            body: ast::Expr::Var {
                name: "captured".into(),
                span: Span::default(),
            },
            env: captured,
            doc: None,
        }));
        env.declare("answer", closure, false).unwrap();
        assert!(matches!(env.get("answer"), Some(Value::Closure(_))));
    }

    #[test]
    fn production_env_locking_has_no_panic_path() {
        let production = include_str!("env.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("test module marker remains present");
        assert!(!production.contains(".lock().unwrap()"));
        assert!(!production.contains(".lock().expect("));
    }
}
