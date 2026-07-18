//! Bounded host-side producers for `[module.custom.*]` prompt snapshots.
//!
//! Rendering remains pure: this scheduler runs only during the REPL's
//! once-per-command snapshot refresh and hands the renderer resolved values.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use shoal_exec::{CancelToken, ExecMode, ExecSpec, StdinSpec};
use shoal_prompt::{CustomModule, CustomSegment, PromptConfig};

const CUSTOM_WORKERS: usize = 2;
const MAX_CUSTOM_ARGV: usize = 64;
const MAX_CUSTOM_COMMAND_BYTES: usize = 4 * 1024;
const MAX_CUSTOM_OUTPUT_BYTES: usize = 4 * 1024;
const CUSTOM_COMMAND_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const CUSTOM_ONE_SHOT_WAIT: Duration = Duration::from_millis(525);
const MAX_CUSTOM_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
struct PreparedCustom {
    argv: Vec<OsString>,
    when: Option<OsString>,
    ttl: Duration,
}

enum Definition {
    Ready(PreparedCustom),
    Invalid(String),
}

struct CacheEntry {
    cwd: PathBuf,
    output: Option<String>,
    last_attempt: Instant,
    last_success: Option<Instant>,
    last_error: Option<String>,
}

struct Job {
    name: String,
    cwd: PathBuf,
    env: Arc<Vec<(OsString, OsString)>>,
    prepared: PreparedCustom,
}

struct JobResult {
    name: String,
    cwd: PathBuf,
    finished: Instant,
    result: Result<String, String>,
}

struct Worker {
    jobs: Option<SyncSender<Job>>,
    join: Option<JoinHandle<()>>,
}

/// Process-lifetime custom-prompt scheduler with bounded workers, queues,
/// retained state, subprocess lifetime, and output.
pub struct CustomScheduler {
    definitions: BTreeMap<String, Definition>,
    cache: BTreeMap<String, CacheEntry>,
    in_flight: BTreeSet<String>,
    workers: Vec<Worker>,
    results: Receiver<JobResult>,
    next_worker: usize,
    shutdown: Arc<AtomicBool>,
    cancel: CancelToken,
}

impl CustomScheduler {
    pub fn new(config: &PromptConfig, warnings: &mut Vec<String>) -> Self {
        let mut definitions = BTreeMap::new();
        for (name, module) in &config.module.custom {
            if !module.enabled {
                continue;
            }
            match prepare(module) {
                Ok(prepared) => {
                    definitions.insert(name.clone(), Definition::Ready(prepared));
                }
                Err(error) => {
                    warnings.push(format!("prompt custom `{name}` disabled: {error}"));
                    definitions.insert(name.clone(), Definition::Invalid(error));
                }
            }
        }

        let (result_tx, results) = sync_channel(shoal_prompt::PROMPT_MAX_DYNAMIC_MODULES);
        let shutdown = Arc::new(AtomicBool::new(false));
        let cancel = CancelToken::new();
        let mut workers = Vec::with_capacity(CUSTOM_WORKERS);
        let ready = definitions
            .values()
            .any(|definition| matches!(definition, Definition::Ready(_)));
        for index in 0..if ready { CUSTOM_WORKERS } else { 0 } {
            let (job_tx, job_rx) = sync_channel(shoal_prompt::PROMPT_MAX_DYNAMIC_MODULES);
            let worker_results = result_tx.clone();
            let worker_shutdown = shutdown.clone();
            let worker_cancel = cancel.clone();
            match std::thread::Builder::new()
                .name(format!("shoal-prompt-custom-{index}"))
                .spawn(move || worker_loop(job_rx, worker_results, worker_shutdown, worker_cancel))
            {
                Ok(join) => workers.push(Worker {
                    jobs: Some(job_tx),
                    join: Some(join),
                }),
                Err(error) => {
                    warnings.push(format!("prompt custom worker {index} unavailable: {error}"))
                }
            }
        }
        drop(result_tx);

        Self {
            definitions,
            cache: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            workers,
            results,
            next_worker: 0,
            shutdown,
            cancel,
        }
    }

    /// Drain completed probes, schedule stale identities without blocking, and
    /// return the exact immutable snapshot the renderer may consume.
    pub fn refresh(
        &mut self,
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> BTreeMap<String, CustomSegment> {
        self.drain_results();
        let mut segments = BTreeMap::new();
        let mut shared_env = None;
        let names = self.definitions.keys().cloned().collect::<Vec<_>>();
        for name in names {
            let Some(definition) = self.definitions.get(&name) else {
                continue;
            };
            let prepared = match definition {
                Definition::Ready(prepared) => prepared.clone(),
                Definition::Invalid(error) => {
                    segments.insert(name, CustomSegment::Error(error.clone()));
                    continue;
                }
            };
            if !condition_matches(prepared.when.as_ref(), env) {
                continue;
            }

            let cached = self.cache.get(&name).filter(|entry| entry.cwd == cwd);
            let due = cached.is_none_or(|entry| entry.last_attempt.elapsed() >= prepared.ttl);
            if due && !self.in_flight.contains(&name) {
                let job = Job {
                    name: name.clone(),
                    cwd: cwd.to_path_buf(),
                    env: shared_env
                        .get_or_insert_with(|| Arc::new(env.to_vec()))
                        .clone(),
                    prepared,
                };
                if let Err(error) = self.schedule(job) {
                    segments.insert(name, CustomSegment::Error(error));
                    continue;
                }
            }

            let segment = match self.cache.get(&name).filter(|entry| entry.cwd == cwd) {
                Some(entry) if entry.last_error.is_none() && !self.in_flight.contains(&name) => {
                    entry.output.clone().map(CustomSegment::Ready)
                }
                Some(entry) => entry.output.clone().map(|output| {
                    CustomSegment::Stale(
                        output,
                        entry
                            .last_success
                            .map_or(Duration::ZERO, |instant| instant.elapsed()),
                    )
                }),
                None => None,
            }
            .unwrap_or_else(|| {
                if self.in_flight.contains(&name) {
                    CustomSegment::Pending
                } else {
                    CustomSegment::Error(
                        self.cache
                            .get(&name)
                            .and_then(|entry| entry.last_error.clone())
                            .unwrap_or_else(|| "custom command unavailable".into()),
                    )
                }
            });
            segments.insert(name, segment);
        }
        segments
    }

    /// Bounded convenience for one-shot `shoal prompt print|explain`: wait for
    /// the first worker results without ever exceeding the caller's wall.
    pub fn refresh_until(
        &mut self,
        cwd: &Path,
        env: &[(OsString, OsString)],
        max_wait: Duration,
    ) -> BTreeMap<String, CustomSegment> {
        let deadline = Instant::now() + max_wait;
        loop {
            let segments = self.refresh(cwd, env);
            if !segments
                .values()
                .any(|segment| matches!(segment, CustomSegment::Pending))
                || Instant::now() >= deadline
            {
                return segments;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn schedule(&mut self, job: Job) -> Result<(), String> {
        if self.workers.is_empty() {
            return Err("custom command workers are unavailable".into());
        }
        let name = job.name.clone();
        let mut pending = Some(job);
        for offset in 0..self.workers.len() {
            let index = (self.next_worker + offset) % self.workers.len();
            let Some(sender) = self.workers[index].jobs.as_ref() else {
                continue;
            };
            let Some(job) = pending.take() else {
                break;
            };
            match sender.try_send(job) {
                Ok(()) => {
                    self.next_worker = (index + 1) % self.workers.len();
                    self.in_flight.insert(name);
                    return Ok(());
                }
                Err(TrySendError::Full(job)) => pending = Some(job),
                Err(TrySendError::Disconnected(job)) => pending = Some(job),
            }
        }
        Err("custom command queue is unavailable or full".into())
    }

    fn drain_results(&mut self) {
        while let Ok(result) = self.results.try_recv() {
            self.in_flight.remove(&result.name);
            let prior = self.cache.remove(&result.name);
            let (output, last_success) = match &result.result {
                Ok(output) => (Some(output.clone()), Some(result.finished)),
                Err(_) => prior
                    .filter(|entry| entry.cwd == result.cwd)
                    .map_or((None, None), |entry| (entry.output, entry.last_success)),
            };
            self.cache.insert(
                result.name,
                CacheEntry {
                    cwd: result.cwd,
                    output,
                    last_attempt: result.finished,
                    last_success,
                    last_error: result.result.err(),
                },
            );
        }
    }
}

impl Drop for CustomScheduler {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.cancel.cancel();
        for worker in &mut self.workers {
            worker.jobs.take();
        }
        for worker in &mut self.workers {
            if let Some(join) = worker.join.take() {
                let _ = join.join();
            }
        }
    }
}

fn worker_loop(
    jobs: Receiver<Job>,
    results: SyncSender<JobResult>,
    shutdown: Arc<AtomicBool>,
    cancel: CancelToken,
) {
    while let Ok(job) = jobs.recv() {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&job, &cancel)))
                .unwrap_or_else(|_| Err("custom command worker panicked".into()));
        let response = JobResult {
            name: job.name,
            cwd: job.cwd,
            finished: Instant::now(),
            result,
        };
        if results.send(response).is_err() {
            break;
        }
    }
}

fn run_job(job: &Job, cancel: &CancelToken) -> Result<String, String> {
    let output = shoal_exec::run_bounded(
        ExecSpec {
            argv: job.prepared.argv.clone(),
            cwd: job.cwd.clone(),
            env: job.env.as_ref().clone(),
            stdin: StdinSpec::Null,
            mode: ExecMode::Capture,
            sandbox: None,
            spill: None,
        },
        CUSTOM_COMMAND_TIMEOUT,
        MAX_CUSTOM_OUTPUT_BYTES,
        cancel,
    )
    .map_err(|error| format!("custom command failed to start: {error}"))?;
    if output.timed_out {
        return Err("custom command exceeded the 250ms deadline".into());
    }
    if output.truncated {
        return Err("custom command exceeded the 4096-byte output limit".into());
    }
    if !output.status.success() {
        return Err(format!("custom command exited with {}", output.status));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "custom command output is not valid UTF-8".to_string())?;
    sanitize_output(&text)
}

fn prepare(module: &CustomModule) -> Result<PreparedCustom, String> {
    let source = module.command.trim();
    if source.is_empty() {
        return Err("command is empty".into());
    }
    if source.len() > MAX_CUSTOM_COMMAND_BYTES {
        return Err("command exceeds the 4096-byte limit".into());
    }
    let words =
        shell_words::split(source).map_err(|error| format!("invalid argv quoting: {error}"))?;
    if words.is_empty() || words.len() > MAX_CUSTOM_ARGV {
        return Err(format!(
            "command must contain 1..={MAX_CUSTOM_ARGV} arguments"
        ));
    }
    let retained = words.iter().try_fold(0usize, |total, word| {
        total.checked_add(word.len()).ok_or(())
    });
    if retained.is_err() || retained.unwrap_or(usize::MAX) > MAX_CUSTOM_COMMAND_BYTES {
        return Err("decoded command exceeds the 4096-byte limit".into());
    }

    let when = match module.when.trim() {
        "" => None,
        name if valid_env_name(name) => Some(OsString::from(name)),
        _ => return Err("when must be empty or one environment variable name".into()),
    };
    let nanos = shoal_value::parse_duration(module.cache_ttl.trim())
        .ok_or_else(|| "cache_ttl is not a duration".to_string())?;
    let nanos = u64::try_from(nanos).map_err(|_| "cache_ttl cannot be negative")?;
    let ttl = Duration::from_nanos(nanos);
    if ttl > MAX_CUSTOM_CACHE_TTL {
        return Err("cache_ttl exceeds the one-hour limit".into());
    }
    Ok(PreparedCustom {
        argv: words.into_iter().map(OsString::from).collect(),
        when,
        ttl,
    })
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn condition_matches(when: Option<&OsString>, env: &[(OsString, OsString)]) -> bool {
    when.is_none_or(|name| {
        env.iter()
            .any(|(key, value)| key == name && !value.is_empty())
    })
}

fn sanitize_output(source: &str) -> Result<String, String> {
    let output = source.trim();
    if output.chars().any(char::is_control) {
        return Err("custom command output contains terminal control characters".into());
    }
    Ok(output.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn config(command: &str, when: &str, ttl: &str) -> PromptConfig {
        let mut config = PromptConfig::default();
        config.module.custom.insert(
            "probe".into(),
            CustomModule {
                command: command.into(),
                when: when.into(),
                cache_ttl: ttl.into(),
                ..CustomModule::default()
            },
        );
        config
    }

    fn wait_for_terminal(
        scheduler: &mut CustomScheduler,
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> CustomSegment {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let segment = scheduler.refresh(cwd, env).remove("probe").unwrap();
            if !matches!(segment, CustomSegment::Pending) {
                return segment;
            }
            assert!(Instant::now() < deadline, "custom prompt worker timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn quoted_argv_runs_off_the_render_thread_and_caches_output() {
        let mut warnings = Vec::new();
        let mut scheduler =
            CustomScheduler::new(&config("printf 'cluster west'", "", "10s"), &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let env = std::env::vars_os().collect::<Vec<_>>();
        let cwd = std::env::current_dir().unwrap();
        assert!(matches!(
            scheduler.refresh(&cwd, &env).get("probe"),
            Some(CustomSegment::Pending)
        ));
        assert!(matches!(
            wait_for_terminal(&mut scheduler, &cwd, &env),
            CustomSegment::Ready(output) if output == "cluster west"
        ));
        assert!(matches!(
            scheduler.refresh(&cwd, &env).get("probe"),
            Some(CustomSegment::Ready(output)) if output == "cluster west"
        ));
    }

    #[test]
    fn when_is_an_exact_nonempty_environment_gate() {
        let mut warnings = Vec::new();
        let mut scheduler = CustomScheduler::new(
            &config("definitely-not-a-command", "PROMPT_GATE", "5s"),
            &mut warnings,
        );
        let cwd = Path::new(".");
        assert!(scheduler.refresh(cwd, &[]).is_empty());
        assert!(
            scheduler
                .refresh(cwd, &[("PROMPT_GATE".into(), "".into())])
                .is_empty()
        );
        assert!(matches!(
            scheduler
                .refresh(cwd, &[("PROMPT_GATE".into(), "1".into())])
                .get("probe"),
            Some(CustomSegment::Pending)
        ));
    }

    #[test]
    fn invalid_config_and_terminal_controls_fail_closed() {
        let mut warnings = Vec::new();
        let scheduler =
            CustomScheduler::new(&config("printf ok", "not-an-env-name", "5s"), &mut warnings);
        assert_eq!(scheduler.definitions.len(), 1);
        assert!(scheduler.workers.is_empty());
        assert!(warnings.iter().any(|warning| warning.contains("disabled")));
        assert!(sanitize_output("ordinary text\n").is_ok());
        assert!(sanitize_output("bad\u{1b}[31m").is_err());
        assert!(sanitize_output("two\nlines").is_err());
    }

    #[test]
    fn subprocess_deadline_terminalizes_without_blocking_refresh() {
        let mut warnings = Vec::new();
        let mut scheduler = CustomScheduler::new(&config("sleep 2", "", "5s"), &mut warnings);
        let env = std::env::vars_os().collect::<Vec<_>>();
        let cwd = std::env::current_dir().unwrap();
        let started = Instant::now();
        assert!(matches!(
            scheduler.refresh(&cwd, &env).get("probe"),
            Some(CustomSegment::Pending)
        ));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(matches!(
            wait_for_terminal(&mut scheduler, &cwd, &env),
            CustomSegment::Error(error) if error.contains("250ms")
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
