//! Candidate discovery, filtering, flag projection, and Reedline assembly.

use std::collections::BTreeSet;
#[cfg(test)]
use std::path::Path;

use reedline::{Span as RlSpan, Suggestion};
use shoal_adapters::CmdAdapter;
use shoal_syntax::commands::{CommandFacts, CommandSource, builtin_names, resolve_command_source};
use shoal_syntax::lexer::RESERVED;
use shoal_value::{Value, method_names, methods_for};

use super::ShoalCompleter;
use super::discovery::filesystem_candidates;

impl ShoalCompleter {
    /// Live executable names from the executing session's PATH projection.
    pub(super) fn path_names(&mut self) -> Vec<String> {
        let cwd = self.cwd();
        self.discovery.path_names(&cwd)
    }

    #[cfg(test)]
    pub(super) fn path_dir_names(&mut self, dir: &Path) -> Vec<String> {
        self.discovery.path_dir_names(dir)
    }

    fn adapter_lookup(&self, head: &str) -> Option<&CmdAdapter> {
        self.adapters
            .iter()
            .find_map(|catalog| catalog.lookup(head))
    }

    fn head_source(&self, head: &str) -> CommandSource {
        let binding = self.env.get(head);
        resolve_command_source(
            head,
            CommandFacts {
                session_callable: binding.as_ref().is_some_and(Value::is_callable),
                session_value: binding.as_ref().is_some_and(|value| !value.is_callable()),
                value_eligible: false,
                forced: false,
                dynamic_run: false,
                runner: false,
                plugin: false,
                adapter: self.adapter_lookup(head).is_some(),
            },
        )
    }

    pub(super) fn candidate_matches(&self, name: &str, prefix: &str) -> bool {
        if prefix.is_empty() {
            return true;
        }
        if self.case_insensitive {
            let name = name.to_lowercase();
            let prefix = prefix.to_lowercase();
            if self.fuzzy {
                subsequence_match(&name, &prefix)
            } else {
                name.starts_with(&prefix)
            }
        } else if self.fuzzy {
            subsequence_match(name, prefix)
        } else {
            name.starts_with(prefix)
        }
    }

    pub(super) fn head_candidates(&mut self, prefix: &str) -> Vec<String> {
        let mut names = BTreeSet::new();
        names.extend(RESERVED.iter().map(|name| name.to_string()));
        names.extend(builtin_names().iter().map(|name| name.to_string()));
        for name in self.env.visible_names() {
            if self.env.get(&name).is_some_and(|value| value.is_callable()) {
                names.insert(name);
            }
        }
        names.extend(self.adapter_names.iter().cloned());
        names.extend(self.path_names());
        names.retain(|name| self.candidate_matches(name, prefix));
        names.into_iter().collect()
    }

    pub(super) fn expr_candidates(&self, prefix: &str) -> Vec<String> {
        let mut names: BTreeSet<String> = self.env.visible_names().into_iter().collect();
        names.extend(RESERVED.iter().map(|name| name.to_string()));
        names.retain(|name| self.candidate_matches(name, prefix));
        names.into_iter().collect()
    }

    pub(super) fn method_candidates(&self, prefix: &str, receiver: Option<&str>) -> Vec<String> {
        let per_type = receiver.and_then(methods_for);
        let names: &[&str] = per_type.as_deref().unwrap_or_else(|| method_names());
        names
            .iter()
            .filter(|name| self.candidate_matches(name, prefix))
            .map(|name| name.to_string())
            .collect()
    }

    /// Adapter flags and lexical function parameters under shared command
    /// resolution precedence.
    pub(super) fn flag_candidates(&self, head: &str, prefix: &str) -> Vec<String> {
        let mut names = BTreeSet::new();
        match self.head_source(head) {
            CommandSource::Adapter => {
                let adapter = self
                    .adapter_lookup(head)
                    .expect("adapter resolution carries its catalog entry");
                extend_adapter_flags(&mut names, adapter);
            }
            CommandSource::SessionCallable => {
                if let Some(Value::Closure(closure)) = self.env.get(head) {
                    for parameter in &closure.params {
                        names.insert(format!("--{}", parameter.name));
                    }
                }
            }
            _ => {}
        }
        names.retain(|name| self.candidate_matches(name, prefix));
        names.into_iter().collect()
    }

    pub(super) fn fs_candidates(&self, word: &str) -> Vec<String> {
        let cwd = self.cwd();
        filesystem_candidates(&cwd, word, |name, prefix| {
            self.candidate_matches(name, prefix)
        })
    }
}

fn extend_adapter_flags(names: &mut BTreeSet<String>, adapter: &CmdAdapter) {
    for parameter in &adapter.top.params {
        names.insert(format!("--{}", parameter.name));
    }
    for short in adapter.top.short_flags.keys() {
        names.insert(format!("-{short}"));
    }
    for subcommand in adapter.subs.values() {
        for parameter in &subcommand.params {
            names.insert(format!("--{}", parameter.name));
        }
        for short in subcommand.short_flags.keys() {
            names.insert(format!("-{short}"));
        }
    }
}

pub(super) fn subsequence_match(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|needle| chars.any(|item| item == needle))
}

/// Sort, deduplicate, cap, and convert candidates into Reedline suggestions.
pub(super) fn finish(
    mut names: Vec<String>,
    start: usize,
    pos: usize,
    max_results: usize,
) -> Vec<Suggestion> {
    names.sort();
    names.dedup();
    names.truncate(max_results);
    names
        .into_iter()
        .map(|value| {
            let append_whitespace = !value.ends_with('/');
            Suggestion {
                value,
                span: RlSpan::new(start, pos),
                append_whitespace,
                ..Default::default()
            }
        })
        .collect()
}
