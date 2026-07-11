//! Lexical scopes (`Env`/`Binding`), moved verbatim out of `lib.rs`.

use super::*;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Env {
    inner: Arc<Mutex<EnvInner>>,
}

#[derive(Debug)]
struct EnvInner {
    vars: HashMap<String, Binding>,
    parent: Option<Env>,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssignError {
    NotFound,
    Immutable,
}

impl Env {
    pub fn root() -> Env {
        Env {
            inner: Arc::new(Mutex::new(EnvInner {
                vars: HashMap::new(),
                parent: None,
            })),
        }
    }

    pub fn child(&self) -> Env {
        Env {
            inner: Arc::new(Mutex::new(EnvInner {
                vars: HashMap::new(),
                parent: Some(self.clone()),
            })),
        }
    }

    pub fn declare(&self, name: impl Into<String>, value: Value, mutable: bool) {
        self.inner
            .lock()
            .unwrap()
            .vars
            .insert(name.into(), Binding { value, mutable });
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let parent = {
            let g = self.inner.lock().unwrap();
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
        let parent = {
            let mut g = self.inner.lock().unwrap();
            if let Some(b) = g.vars.get_mut(name) {
                if !b.mutable {
                    return Err(AssignError::Immutable);
                }
                b.value = value;
                return Ok(());
            }
            g.parent.clone()
        };
        match parent {
            Some(p) => p.assign(name, value),
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
            let g = env.inner.lock().unwrap();
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
