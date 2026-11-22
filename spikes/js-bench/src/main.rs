//! Spike D: how much memory and startup time does a QuickJS app process cost, times eight?
//!
//! `cargo run --release -p js-bench` spawns copies of itself as app processes, each
//! running `app.js` (signals plus scene ops), and reports their resident memory and time
//! to ready. It then measures signal-update throughput in this process.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rquickjs::{CatchResultExt, Context, Function, Runtime};

const APP_JS: &str = include_str!("app.js");

type Result<T> = std::result::Result<T, String>;

thread_local! {
    /// Frames the app sent: (count, bytes, and the frames themselves when capturing).
    static SENT: RefCell<(usize, usize, Option<Vec<String>>)> = const { RefCell::new((0, 0, None)) };
}

fn send(frame: String) {
    SENT.with(|s| {
        let mut s = s.borrow_mut();
        s.0 += 1;
        s.1 += frame.len();
        if let Some(frames) = s.2.as_mut() {
            frames.push(frame);
        }
    });
}

/// Unused functions that make the bundle about as large as a real app plus SDK, so
/// parse time and bytecode memory are not flattered by a tiny script.
fn padding(kb: usize) -> String {
    let mut s = String::new();
    let mut i = 0;
    while s.len() < kb * 1024 {
        let _ = writeln!(
            s,
            "function pad{i}(a, b) {{ const t = [a, b, {i}]; return t.map((x) => x * {i} + 1).reduce((x, y) => x + y, 0); }}"
        );
        i += 1;
    }
    s
}

struct App {
    runtime: Runtime,
    context: Context,
}

impl App {
    /// Start a runtime and run the app: `rows` live labels, `bundle_kb` of extra code.
    fn start(rows: usize, bundle_kb: usize) -> Result<App> {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let context = Context::full(&runtime).map_err(|e| e.to_string())?;
        let source = format!("globalThis.ROWS = {rows};\n{}\n{APP_JS}", padding(bundle_kb));
        context.with(|ctx| -> Result<()> {
            let send = Function::new(ctx.clone(), send).map_err(|e| e.to_string())?;
            ctx.globals().set("__send", send).map_err(|e| e.to_string())?;
            ctx.eval::<(), _>(source).catch(&ctx).map_err(|e| e.to_string())
        })?;
        Ok(App { runtime, context })
    }

    fn bench(&self, updates: usize) -> Result<()> {
        self.context.with(|ctx| -> Result<()> {
            let bench: Function = ctx.globals().get("bench").map_err(|e| e.to_string())?;
            bench.call::<_, ()>((updates,)).catch(&ctx).map_err(|e| e.to_string())
        })
    }

    /// Bytes the QuickJS heap is using.
    fn heap(&self) -> i64 {
        self.runtime.memory_usage().memory_used_size
    }
}

/// Resident set size of a process in KiB.
fn rss_kib(pid: u32) -> Option<u64> {
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        return line.split_whitespace().nth(1)?.parse().ok();
    }
    let out = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Child mode: start the app (or just an empty runtime), report, then wait for stdin to close.
fn child(rows: usize, bundle_kb: usize, empty: bool) -> Result<()> {
    let started = Instant::now();
    if empty {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let _context = Context::full(&runtime).map_err(|e| e.to_string())?;
        println!("READY {} {}", started.elapsed().as_micros(), runtime.memory_usage().memory_used_size);
        let _ = std::io::stdin().read_to_end(&mut Vec::new());
    } else {
        let app = App::start(rows, bundle_kb)?;
        println!("READY {} {}", started.elapsed().as_micros(), app.heap());
        let _ = std::io::stdin().read_to_end(&mut Vec::new());
    }
    Ok(())
}

struct Probe {
    child: Child,
    ready: Duration,
    js_heap: i64,
}

/// Spawn one app process and wait until it reports ready.
fn spawn(rows: usize, bundle_kb: usize, empty: bool) -> Result<Probe> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(["--child", &rows.to_string(), &bundle_kb.to_string()]);
    if empty {
        cmd.arg("--empty");
    }
    let started = Instant::now();
    let mut child =
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().map_err(|e| format!("spawn: {e}"))?;
    let mut line = String::new();
    BufReader::new(child.stdout.take().ok_or("no stdout")?)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let ready = started.elapsed();
    let mut parts = line.split_whitespace();
    if parts.next() != Some("READY") {
        let _ = child.kill();
        return Err(format!("child did not start: {line:?}"));
    }
    let js_heap = parts.nth(1).and_then(|h| h.parse().ok()).unwrap_or(0);
    Ok(Probe { child, ready, js_heap })
}

fn mib(kib: u64) -> String {
    format!("{:.1} MiB", kib as f64 / 1024.0)
}

fn run(processes: usize, rows: usize, bundle_kb: usize, updates: usize) -> Result<Vec<String>> {
    let mut out = vec![
        format!(
            "{processes} app processes, {rows} live labels each, {bundle_kb} KiB bundle, {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        String::new(),
        "| Process | Ready in | RSS | QuickJS heap |".into(),
        "| --- | --- | --- | --- |".into(),
    ];

    let empty = spawn(rows, bundle_kb, true)?;
    let empty_rss = rss_kib(empty.child.id()).unwrap_or(0);
    out.push(format!(
        "| Empty runtime (baseline) | {:.1} ms | {} | {} KiB |",
        empty.ready.as_secs_f64() * 1000.0,
        mib(empty_rss),
        empty.js_heap / 1024
    ));

    // Start them all before measuring, as `session` would at login.
    let started = Instant::now();
    let mut probes = Vec::new();
    for _ in 0..processes {
        probes.push(spawn(rows, bundle_kb, false)?);
    }
    let all_ready = started.elapsed();
    let mut total = 0;
    for (i, p) in probes.iter().enumerate() {
        let rss = rss_kib(p.child.id()).unwrap_or(0);
        total += rss;
        out.push(format!(
            "| App {} | {:.1} ms | {} | {} KiB |",
            i + 1,
            p.ready.as_secs_f64() * 1000.0,
            mib(rss),
            p.js_heap / 1024
        ));
    }
    out.push(String::new());
    out.push(format!(
        "All {processes} ready in {:.0} ms (started one after another). Total RSS {}; {} above the empty baseline per app.",
        all_ready.as_secs_f64() * 1000.0,
        mib(total),
        mib((total / processes.max(1) as u64).saturating_sub(empty_rss))
    ));
    for mut p in probes.into_iter().chain([empty]) {
        drop(p.child.stdin.take());
        let _ = p.child.wait();
    }

    // Throughput in this process.
    let app = App::start(rows, bundle_kb)?;
    SENT.with(|s| *s.borrow_mut() = (0, 0, None));
    let started = Instant::now();
    app.bench(updates)?;
    let elapsed = started.elapsed().as_secs_f64();
    let (frames, bytes, _) = SENT.with(|s| s.borrow().clone());
    out.push(format!(
        "Signal updates: {updates} in {:.0} ms = {:.0} updates/s, each a `set` op; {frames} commits, {:.1} MB of JSON.",
        elapsed * 1000.0,
        updates as f64 / elapsed,
        bytes as f64 / 1e6
    ));
    Ok(out)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |flag: &str, default: usize| -> usize {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let result = if args.first().map(String::as_str) == Some("--child") {
        let rows = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(500);
        let kb = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(100);
        child(rows, kb, args.iter().any(|a| a == "--empty")).map(|_| Vec::new())
    } else if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("js-bench [--processes N] [--rows N] [--bundle-kb N] [--updates N]");
        return;
    } else {
        run(num("--processes", 8), num("--rows", 500), num("--bundle-kb", 100), num("--updates", 100_000))
    };
    match result {
        Ok(lines) if !lines.is_empty() => println!("{}", lines.join("\n")),
        Ok(_) => {}
        Err(e) => {
            eprintln!("js-bench: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Commit, Message, SceneOp};

    fn captured<T>(f: impl FnOnce() -> T) -> (T, Vec<Commit>) {
        SENT.with(|s| *s.borrow_mut() = (0, 0, Some(Vec::new())));
        let out = f();
        let frames = SENT.with(|s| s.borrow_mut().2.take().unwrap_or_default());
        let commits = frames
            .iter()
            .map(|f| match serde_json::from_str::<Message>(f).expect("frame is a bus message") {
                Message::Request { to, method, params, .. } => {
                    assert_eq!((to.as_str(), method.as_str()), ("compositor", "commit"));
                    serde_json::from_value::<Commit>(params).expect("params are a commit")
                }
                other => panic!("expected a request, got {other:?}"),
            })
            .collect();
        (out, commits)
    }

    #[test]
    fn app_emits_valid_commits() {
        let (app, commits) = captured(|| App::start(3, 1).unwrap());
        assert_eq!(commits.len(), 1, "initial build is one commit");
        let creates = commits[0].ops.iter().filter(|op| matches!(op, SceneOp::Create { .. })).count();
        assert_eq!(creates, 5, "vstack, title and three rows");

        let ((), commits) = captured(|| app.bench(250).unwrap());
        assert_eq!(commits.len(), 3, "a commit per 100 updates plus the remainder");
        let sets: usize = commits.iter().map(|c| c.ops.len()).sum();
        assert_eq!(sets, 250);
    }

    #[test]
    fn padding_reaches_size() {
        assert!(padding(4).len() >= 4096);
        assert!(App::start(1, 4).is_ok(), "padded bundle must still parse");
    }
}
