//! Reactive streams + in-language `channel()` (site/content/internals/streams-channels.md, site/content/internals/roadmap-and-priorities.md).
//!
//! Timing sources (`every`/`watch`/`tail`) and the concurrent channel/`on`
//! machinery are unit-tested here rather than in the host-safe conformance
//! corpus (which stays deterministic). Intervals are small and timeouts generous
//! so the suite is CI-green on Linux and macOS alike.

use shoal_eval::Evaluator;
use shoal_value::{Value, render::render_inline};
use std::time::{Duration, Instant};

/// Evaluate a whole program in one evaluator (channel state persists across its
/// statements) rooted at a fresh temp dir.
fn run(src: &str) -> Value {
    let dir = tempfile::tempdir().unwrap();
    run_in(src, dir.path())
}

fn run_in(src: &str, cwd: &std::path::Path) -> Value {
    let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}\n{src}"));
    Evaluator::new(cwd.to_path_buf())
        .eval_program(&program)
        .unwrap_or_else(|e| panic!("eval failed: {e}\n{src}"))
}

fn run_err(src: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let program = shoal_syntax::parse(src).unwrap();
    Evaluator::new(dir.path().to_path_buf())
        .eval_program(&program)
        .err()
        .unwrap_or_else(|| panic!("expected an error\n{src}"))
        .code
}

fn rendered(src: &str) -> String {
    render_inline(&run(src))
}

// --- in-language channels ------------------------------------------------

#[test]
fn channel_emit_latest_roundtrips() {
    // The stream/channel contract's replacement for a `.done` sentinel file.
    assert_eq!(
        run(r#"channel("x").emit(1); channel("x").emit(2); channel("x").latest()"#),
        Value::Int(2)
    );
}

#[test]
fn channel_latest_is_null_before_any_emit() {
    assert_eq!(run(r#"channel("fresh").latest()"#), Value::Null);
}

#[test]
fn channel_events_replays_ring_as_a_stream() {
    // `.events()` (no cursor) replays the whole ring then goes live — the same
    // shape as the kernel EventBus. Mapping to `.payload` drops the wall-clock
    // `ts`, keeping the assertion deterministic.
    let v = rendered(
        r#"channel("z").emit("a")
channel("z").emit("b")
channel("z").events().map(ev => ev.payload).take(2).collect()"#,
    );
    assert_eq!(v, r#"["a", "b"]"#);
}

#[test]
fn channel_events_since_cursor_skips_earlier() {
    let v = rendered(
        r#"channel("c").emit(10)
channel("c").emit(20)
channel("c").emit(30)
channel("c").events(since: 0).map(ev => ev.payload).take(2).collect()"#,
    );
    assert_eq!(v, "[20, 30]");
}

#[test]
fn channel_take_blocks_for_the_next_value() {
    // A background emitter publishes after a short delay; `.take(timeout:)` blocks
    // for it — no file, no poll (site/content/internals/streams-channels.md).
    let v = run(r#"spawn { sleep 20ms; channel("k").emit(99) }
channel("k").take(timeout: 5s)"#);
    assert_eq!(v, Value::Int(99));
}

#[test]
fn channel_take_times_out() {
    assert_eq!(
        run_err(r#"channel("silent").take(timeout: 30ms)"#),
        "timeout"
    );
}

// --- stream combinators on deterministic finite streams ------------------

#[test]
fn where_map_chain() {
    assert_eq!(
        rendered("[1,2,3,4,5,6].stream().where(x => x % 2 == 0).map(x => x * 10).collect()"),
        "[20, 40, 60]"
    );
}

#[test]
fn scan_running_fold() {
    assert_eq!(
        rendered("[1,2,3,4].stream().scan(0, (a, x) => a + x).collect()"),
        "[1, 3, 6, 10]"
    );
}

#[test]
fn take_bounds_and_take_until_pred() {
    assert_eq!(
        rendered("[1,2,3,4,5].stream().take(3).collect()"),
        "[1, 2, 3]"
    );
    assert_eq!(
        rendered("[1,2,3,4,5].stream().take_until(x => x == 4).collect()"),
        "[1, 2, 3]"
    );
}

#[test]
fn window_slides() {
    assert_eq!(
        rendered("[1,2,3,4].stream().window(2).collect()"),
        "[[1, 2], [2, 3], [3, 4]]"
    );
    assert_eq!(run_err("[1].stream().window(4097).collect()"), "arg_error");
}

#[test]
fn dedupe_and_distinct() {
    assert_eq!(
        rendered("[1,1,2,2,2,3,1].stream().dedupe().collect()"),
        "[1, 2, 3, 1]"
    );
    assert_eq!(
        rendered("[1,1,2,2,2,3,1].stream().distinct().collect()"),
        "[1, 2, 3]"
    );
    assert_eq!(
        rendered("[1,1.0,[2],[2.0]].stream().distinct().collect()"),
        "[1, [2]]",
        "distinct must preserve mixed numeric equality recursively"
    );
    assert_eq!(
        run_err("[1,1,2,3].stream().distinct(2).collect()"),
        "stream_distinct_limit"
    );
    assert_eq!(run_err("[1].stream().distinct(0).collect()"), "arg_error");
}

#[test]
fn flat_map_over_lists() {
    // `flat_map` is deliberately concat-map: each expansion is exhausted
    // before the next outer item is pulled. It does not claim concurrent
    // interleaving of child streams.
    assert_eq!(
        rendered("[1,2,3].stream().flat_map(x => [x, x * 10]).collect()"),
        "[1, 10, 2, 20, 3, 30]"
    );
    assert_eq!(
        rendered("[0].stream().flat_map(_ => 1..1000000).take(3).collect()"),
        "[1, 2, 3]",
        "compact range expansions must stay lazy inside flat_map"
    );
}

#[test]
fn compact_ranges_bound_eager_materialization_but_stream_lazily() {
    assert_eq!(
        run_err("(1..1000000).collect()"),
        "range_materialization_limit"
    );
    assert_eq!(
        run_err("json.stringify(1..1000000)"),
        "range_materialization_limit"
    );
    assert_eq!(
        rendered("(1..1000000).stream().take(3).collect()"),
        "[1, 2, 3]"
    );
    assert_eq!(
        run_err("(0..16385).stream().collect()"),
        "stream_collect_limit"
    );
}

#[test]
fn enumerate_pairs() {
    assert_eq!(
        rendered(r#"["a","b"].stream().enumerate().collect()"#),
        r#"[[0, "a"], [1, "b"]]"#
    );
}

#[test]
fn buffer_decouples_through_a_bounded_owned_pump() {
    assert_eq!(
        rendered("[1,2,3].stream().map(x => x * 2).buffer(2).collect()"),
        "[2, 4, 6]"
    );
    assert_eq!(
        rendered("[1,2,3].stream().buffer(0).collect()"),
        "[1, 2, 3]"
    );
}

#[test]
fn finite_stream_feeds_command_stdin_as_line_framed_chunks() {
    let Value::Outcome(outcome) = run(r#"["alpha", "beta"].stream().feed(cat)"#) else {
        panic!("stream feed must return an outcome");
    };
    assert_eq!(outcome.stdout.as_slice(), b"alpha\nbeta\n");
}

#[test]
fn live_stream_feeds_items_that_arrive_after_the_child_starts() {
    let Value::Outcome(outcome) = run(
        r#"spawn { sleep 10ms; channel("feed-live").emit("one"); sleep 10ms; channel("feed-live").emit("two") }
channel("feed-live").events().map(ev => ev.payload).take(2).feed(cat)"#,
    ) else {
        panic!("stream feed must return an outcome");
    };
    assert_eq!(outcome.stdout.as_slice(), b"one\ntwo\n");
}

#[test]
fn command_early_exit_disconnects_an_endless_stream_feed() {
    let start = Instant::now();
    let Value::Outcome(outcome) = run(r#"every(5ms).map(x => "tick").feed(head -n 2)"#) else {
        panic!("stream feed must return an outcome");
    };
    assert_eq!(outcome.stdout.as_slice(), b"tick\ntick\n");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn stream_feed_surfaces_item_serialization_errors() {
    assert_eq!(
        run_err(r#"[path("not-content")].stream().feed(cat)"#),
        "type_error"
    );
}

#[test]
fn stream_feed_stops_an_idle_pump_when_command_spawn_fails() {
    let start = Instant::now();
    assert_eq!(
        run_err("every(1s).feed(definitely-not-a-command-xyz)"),
        "not_found"
    );
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "spawn failure left the live stream pump blocked"
    );
}

#[test]
fn zip_pairs_positionally() {
    assert_eq!(
        rendered("[1,2,3].stream().zip([10,20].stream()).collect()"),
        "[[1, 10], [2, 20]]"
    );
}

#[test]
fn merge_interleaves_finite_streams() {
    // Both sides are immediately ready, so round-robin preference is exact.
    assert_eq!(
        rendered("[1,2].stream().merge([3,4].stream()).collect()"),
        "[1, 3, 2, 4]"
    );
}

#[test]
fn take_short_circuits_an_endless_source() {
    // `every` is endless; `.take(3)` bounds it and cancels the timer upstream.
    let start = Instant::now();
    let v = run("every(5ms).take(3).collect().len()");
    assert_eq!(v, Value::Int(3));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
}

#[test]
fn collect_on_unbounded_errors() {
    assert_eq!(run_err("every(5ms).collect()"), "stream_unbounded");
}

#[test]
fn single_consumption_enforced() {
    assert_eq!(
        run_err("let s = [1,2,3].stream()\ns.collect()\ns.collect()"),
        "stream_consumed"
    );
}

#[test]
fn each_drives_a_stream_to_a_channel() {
    // `.each` sink runs the closure per item; combined with `channel.emit`, it is
    // a fully in-language side-effecting consumer.
    let v = run(r#"[1,2,3].stream().each(x => channel("acc").emit(x))
channel("acc").latest()"#);
    assert_eq!(v, Value::Int(3));
}

#[test]
fn into_republishes_onto_a_channel() {
    let v = run(r#"[7,8,9].stream().into(channel("mirror"))
channel("mirror").latest()"#);
    assert_eq!(v, Value::Int(9));
}

#[test]
fn for_loop_iterates_a_bounded_stream() {
    let v = run(r#"var total = 0
let s = [1,2,3,4].stream()
for x in s { total = total + x }
total"#);
    assert_eq!(v, Value::Int(10));
}

// --- `on(channel, handler)` — the spawned subscriber ---------------------

#[test]
fn on_runs_handler_per_event() {
    // `on(...)` ≡ `spawn { channel(x).events().each(handler) }`. The handler
    // republishes onto a second channel we then block on.
    let v = run(
        r#"on(channel("src"), ev => channel("sink").emit(ev.payload * 2))
sleep 20ms
channel("src").emit(21)
channel("sink").take(timeout: 5s)"#,
    );
    assert_eq!(v, Value::Int(42));
}

#[test]
fn cancelling_idle_on_handler_terminates_its_task() {
    let v = run(r#"let task = on(channel("idle-on"), ev => ev)
task.cancel()
task.await()"#);
    assert_eq!(v, Value::Null);
}

// --- `every` yields datetimes -------------------------------------------

#[test]
fn every_yields_datetimes() {
    let v = run("every(5ms).take(1).collect()");
    match v {
        Value::List(xs) => assert!(matches!(xs.first(), Some(Value::DateTime(_)))),
        other => panic!("expected a list of datetimes, got {other:?}"),
    }
}

// --- watch (notify: inotify / FSEvents-kqueue) --------------------------

#[test]
fn watch_reports_file_events() {
    // The worker's `.take(1)` blocks until the first event, so its result comes
    // back over a channel we `recv_timeout` on — a slow/missing notification
    // fails cleanly instead of hanging `join()` forever (macOS FSEvents can lag).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let program =
        shoal_syntax::parse(r#"watch(".").take(1).map(ev => ev.kind).collect()"#).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(Evaluator::new(root).eval_program(&program));
    });
    let start = Instant::now();
    let v = loop {
        let _ = std::fs::write(dir.path().join("poke.txt"), b"x");
        if let Ok(r) = rx.recv_timeout(Duration::from_millis(50)) {
            break r.expect("watch program");
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "watch produced no event within 10s"
        );
    };
    match v {
        Value::List(xs) => {
            assert_eq!(xs.len(), 1, "one event taken");
            assert!(matches!(&xs[0], Value::Str(_)), "kind is a string");
        }
        other => panic!("expected a list, got {other:?}"),
    }
}

// --- tail (event-driven append following) -------------------------------

#[test]
fn tail_follows_appends() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("app.log");
    std::fs::write(&file, b"old line\n").unwrap();
    let root = dir.path().to_path_buf();
    let program =
        shoal_syntax::parse(r#"tail("app.log").where(x => x.contains("ERROR")).take(1).collect()"#)
            .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(Evaluator::new(root).eval_program(&program));
    });
    let start = Instant::now();
    let v = loop {
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&file)
                .unwrap();
            let _ = writeln!(f, "some INFO");
            let _ = writeln!(f, "an ERROR happened");
            f.flush().ok();
        }
        if let Ok(r) = rx.recv_timeout(Duration::from_millis(50)) {
            break r.expect("tail program");
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "tail produced no matching line within 10s"
        );
    };
    assert_eq!(render_inline(&v), r#"["an ERROR happened"]"#);
}
