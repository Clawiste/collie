use crate::logs::Record;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Target {
    pub name: String,
    pub ecosystem: String,
    /// Workspace root (cargo) or package.json dir (node); `collie.toml` is read from here.
    pub dir: PathBuf,
    pub kind: String,
    pub package: Option<String>,
    pub needs_port: bool,
    pub port: Option<u16>,
    /// Cargo's `target_directory`; None for other ecosystems.
    pub target_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Build,
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Resource {
    Port { preferred: Option<u16> }, // later: Device, BuildSlot
    Simulator,
}

/// Leased resources a command needs other than through env.
#[derive(Default)]
pub struct Ctx {
    /// UDID of the leased simulator device.
    pub simulator: Option<String>,
    /// UDID of the leased physical device; build and run use it instead of `simulator`.
    pub device: Option<String>,
    /// Open the device in DeviceHub; headless otherwise.
    pub head: bool,
    /// The caller's env, for commands the adapter runs itself (the daemon adds it to the commands it returns).
    pub env: HashMap<String, String>,
    /// The run args, for a build that must match its run (`cargo test`); the daemon appends them to the run itself.
    pub args: Vec<String>,
    /// The process's log file; a test run's result files go beside it (`<id>.xcresult`).
    pub log: PathBuf,
}

/// A kind of project collie detects and runs. Adding one:
/// - implement the required methods: `id`, `detect` (its targets in a dir), `needs` (port, simulator, device),
///   `build` (None: no build step), `run` and `classify` (a log line's level);
/// - override what it supports: `launched` (an app's launch line), `results` (an ended test run's tests), `select`
///   (`run --only`), `check_args` (refusing run args its run phase would drop), `outcome` (live test counts),
///   `continues` (lines that belong to the record before them, as a diagnostic's source excerpt does) and `platform`
///   (where its devices come from: an existing `Platform`, or a new one registered in `PLATFORMS`);
/// - register it in `ECOSYSTEMS` (the daemon finds a target's ecosystem by `id` there);
/// - add a project under `tests/fixtures/` and a test of `detect` and `run` on it in this file's test module, as the
///   existing adapters have.
pub trait Ecosystem: Sync {
    fn id(&self) -> &'static str;
    fn detect(&self, dir: &Path) -> Result<Vec<Target>>;
    fn needs(&self, t: &Target) -> Vec<Resource>;
    /// None = no build step.
    fn build(&self, t: &Target, ctx: &Ctx) -> Option<Command>;
    /// The daemon adds the caller's env and `PORT` to the returned command.
    fn run(&self, t: &Target, ctx: &Ctx) -> Command;
    fn classify(&self, src: Source, line: &str) -> (Level, String);
    /// Whether this run-phase line shows the app has launched. Asked only for targets that need a simulator;
    /// for others `launched` is reached with `running`.
    fn launched(&self, _src: Source, _line: &str) -> bool {
        false
    }
    /// An ended test run's results, from its log `recs` (each with its cursor) or files beside `ctx.log`:
    /// `{"sep": <id separator>, "tests": [{"id", "name", "status", "duration_ms", "failure": {"message", "file",
    /// "line", "cursor"} | null}]}`. None when there are none.
    fn results(&self, _t: &Target, _ctx: &Ctx, _recs: &[(String, Record)]) -> Option<Value> {
        None
    }
    /// `args` that make `t` run only the tests `ids` (from `results`; a group's ends in its `sep`); `ids` is never
    /// empty. Code `bad_request` when `t` can't.
    fn select(&self, t: &Target, _ids: &[String], _args: Vec<String>) -> Result<Vec<String>> {
        bail!(crate::daemon::coded("bad_request", only_refused(t)))
    }
    /// Refuses (code `bad_request`) non-empty run `args` that `t`'s run phase wouldn't pass on.
    fn check_args(&self, _t: &Target, _args: &[String]) -> Result<()> {
        Ok(())
    }
    /// The outcome (`passed`, `failed`, `skipped`) a run-phase line reports for one test, for live totals.
    fn outcome(&self, _line: &str) -> Option<&'static str> {
        None
    }
    /// Whether raw `line` continues the open record (whose first line is `open`) rather than starting one: the daemon
    /// joins such lines to that record, and classifies the record by its first line alone.
    fn continues(&self, _open: &str, _line: &str) -> bool {
        false
    }
    /// The platform its simulator and physical-device runs use; None when it has none.
    fn platform(&self) -> Option<&'static dyn Platform> {
        None
    }
}

fn only_refused(t: &Target) -> String {
    format!("{} can't run some of its tests (`only`): only cargo and Xcode test targets can", t.name)
}

pub const ECOSYSTEMS: [&dyn Ecosystem; 4] = [&Cargo, &Node, &Xcode, &Task];

pub fn get(id: &str) -> &'static dyn Ecosystem {
    *ECOSYSTEMS.iter().find(|e| e.id() == id).expect("target from a known ecosystem")
}

pub fn detect_all(dir: &Path) -> Result<Vec<Target>> {
    let (mut all, mut err) = (vec![], None);
    for e in ECOSYSTEMS {
        match e.detect(dir) {
            Ok(ts) => all.extend(ts),
            // The user's own file: they must see it's broken, not get targets with its overrides quietly missing.
            Err(x) if x.downcast_ref::<crate::daemon::Coded>().is_some_and(|c| c.0 == "bad_config") => return Err(x),
            Err(x) => {
                eprintln!("warn: {} detect in {}: {x:#}", e.id(), dir.display());
                err = Some(x);
            }
        }
    }
    if let (true, Some(x)) = (all.is_empty(), err) {
        return Err(x);
    }
    for t in &mut all {
        if let Some(o) = config(&t.dir)?.targets.get(&t.name) {
            t.port = o.port.or(t.port);
            t.needs_port = o.needs_port.unwrap_or(t.needs_port);
        }
    }
    // `run` resolves by name, so a task shadowing a detected target would make it pick one of the two silently.
    for t in all.iter().filter(|t| t.ecosystem == "task") {
        if let Some(o) = all.iter().find(|o| o.name == t.name && o.ecosystem != "task") {
            let path = t.dir.join("collie.toml");
            bail!(crate::daemon::coded("bad_config", format!("{}: task \"{}\" clashes with {} target \"{}\"", path.display(), t.name, o.ecosystem, o.name)));
        }
    }
    Ok(all)
}

/// `collie.toml`: `[targets.<name>]` overrides, `[tasks.<name>]` declared tasks and `[prepare]`.
#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    targets: HashMap<String, Overrides>,
    #[serde(default)]
    tasks: std::collections::BTreeMap<String, TaskDef>,
    prepare: Option<Prepare>,
}

/// `[prepare]`: a command that brings generated project files up to date (`tuist generate`, `xcodegen`,
/// `pod install`). The daemon runs it before a target builds, in the dir of the `collie.toml` that declares it, when a
/// `watch` path changed since it last succeeded (always when `watch` is empty).
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Prepare {
    pub cmd: String,
    /// Paths relative to that dir; a directory counts everything under it, so adding or removing a file in it counts.
    #[serde(default)]
    pub watch: Vec<String>,
}

/// The `[prepare]` of the nearest `collie.toml` at or above `dir`, with that file's dir.
pub fn prepare_of(dir: &Path) -> Result<Option<(PathBuf, Prepare)>> {
    let Some(root) = dir.ancestors().find(|d| d.join("collie.toml").is_file()) else { return Ok(None) };
    Ok(config(root)?.prepare.map(|p| (root.to_path_buf(), p)))
}

/// The newest modification time among `watch` (relative to `root`), recursing into directories; None when none exist.
pub fn newest_change(root: &Path, watch: &[String]) -> Option<std::time::SystemTime> {
    fn walk(p: &Path, newest: &mut Option<std::time::SystemTime>) {
        let Ok(m) = std::fs::symlink_metadata(p) else { return };
        if let Ok(t) = m.modified() {
            *newest = (*newest).max(Some(t));
        }
        if m.is_dir() {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                walk(&e.path(), newest);
            }
        }
    }
    let mut newest = None;
    for w in watch {
        walk(&root.join(w), &mut newest);
    }
    newest
}

#[derive(Deserialize)]
struct Overrides {
    port: Option<u16>,
    needs_port: Option<bool>,
}

#[derive(Deserialize)]
struct TaskDef {
    cmd: String,
}

/// `dir`'s `collie.toml`; empty when there is none. Code `bad_config` when it can't be read or parsed.
fn config(dir: &Path) -> Result<Config> {
    let path = dir.join("collie.toml");
    let bad = |e: &dyn std::fmt::Display| crate::daemon::coded("bad_config", format!("{}: {e}", path.display()));
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).map_err(|e| bad(&e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(bad(&e)),
    }
}

fn port_need(t: &Target) -> Vec<Resource> {
    if t.needs_port || t.port.is_some() {
        vec![Resource::Port { preferred: t.port }]
    } else {
        vec![]
    }
}

/// `line` without ANSI colour codes, wherever they are: forced-colour pytest prints `FAILED\x1b[0m tests/…`, and
/// `CARGO_TERM_COLOR=always` colours `Blocking`.
fn plain(line: &str) -> String {
    let (mut plain, mut rest) = (String::new(), line);
    while let Some(i) = rest.find("\x1b[") {
        plain.push_str(&rest[..i]);
        let code = &rest[i + 2..];
        rest = code.find(|c: char| ('@'..='~').contains(&c)).map_or("", |end| &code[end + 1..]);
    }
    plain.push_str(rest);
    plain
}

/// Whether `line` says the tool waits on (or, for xcodebuild, failed on) a lock another build holds: cargo's build
/// directory or package cache, yarn 1's `--mutex`, xcodebuild's build database. npm 10 and pnpm 10 print no such line.
pub fn waits_for_lock(line: &str) -> bool {
    let p = plain(line);
    let l = p.trim_start();
    l.starts_with("Blocking waiting for file lock on ")
        // From yarn 1.22's source (`waitingInstance`), not seen live.
        || l.contains("Waiting for the other yarn instance to finish")
        // From memory, unverified: `error: unable to attach DB: error: accessing build database "…/build.db": database is locked …`
        || (l.contains("build database") && l.contains("database is locked"))
}

/// Whether `line` reports a failed test, from any runner a task may run; anchored to the line start (past
/// indentation and colour codes), so dev-server output that merely mentions a failure stays safe.
fn test_failed(line: &str) -> bool {
    let p = plain(line);
    let l = p.trim();
    // jest `✕`, vitest `×` and their `FAIL ` file lines and xcodebuild's `** TEST FAILED **` are from memory or the
    // brief, unverified here; node:test, libtest, pytest, go test and xcodebuild 27's test-without-building lines were
    // captured: Swift Testing prints `✘` for an iOS simulator and SF Symbols' private-use `􀢄` for macOS.
    let starts = ["not ok ", "✕ ", "× ", "✘ ", "􀢄 ", "FAIL ", "FAIL\t", "--- FAIL:", "FAILED ", "test result: FAILED.", "** TEST FAILED **", "** TEST EXECUTE FAILED **"];
    // pytest's summary: `==== 1 failed, 2 passed in 0.12s ====`.
    let pytest = || l.starts_with('=') && l.trim_matches(|c| c == '=' || c == ' ').split(", ").any(|p| p.split_once(' ').is_some_and(|(n, w)| n.parse::<u32>().is_ok() && w.split(' ').next() == Some("failed")));
    starts.iter().any(|s| l.starts_with(s))
        || l == "FAIL"
        || (l.starts_with("test ") && l.ends_with(" ... FAILED"))
        || (l.starts_with("Test Case '") && l.contains("' failed"))
        || pytest()
}

/// The build problem a log record reports, as `{"kind": "error"|"warning"|"lint", "rule", "path", "line", "col",
/// "message"}` with `path` as printed. Tool formats, so any target's: swiftc/clang `<path>:<line>:<col>: warning: …`
/// (a column required: XCTest's `File.swift:10: error: -[…]` is a test result), SwiftLint's `… Violation: … (rule)`
/// and SwiftFormat's `(rule)` as lint, tuist's `[!]`/`[x]`, rustc's rendered `warning: …` with its `--> path:line:col`,
/// tsc's `path(line,col): error TS…` and `path:line:col - error TS…`. None for anything without a file.
pub fn diagnostic(record: &str) -> Option<Value> {
    let p = plain(record);
    let first = p.lines().next()?.trim();
    let (kind, path, line, col, message) = if let Some(at) = p.lines().find_map(|l| l.trim_start().strip_prefix("--> ")) {
        // rustc: `error[E0425]: …` on the first line.
        let (kind, msg) = first.split_once(": ")?;
        let (path, line, col, _) = place(at.trim())?;
        (kind.split('[').next()?, path, line, col, msg)
    } else if let Some((tag, r)) = first.strip_prefix("[!] ").map(|r| ("warning", r)).or_else(|| first.strip_prefix("[x] ").map(|r| ("error", r))) {
        let (path, line, col, msg) = place(r.trim_start())?;
        (tag, path, line, col, msg)
    } else {
        let (path, line, col, rest) = place(first).or_else(|| tsc_place(first))?;
        let (kind, msg) = rest.split_once(": ")?;
        // tsc: `error TS2322`.
        (kind.split(' ').next()?, path, line, col, msg)
    };
    if !matches!(kind, "warning" | "error") || path.is_empty() || path.starts_with("macro expansion") {
        return None;
    }
    // A rule is an identifier in parentheses: `(empty_string)` at the end, or SwiftFormat's `(emptyBraces)` up front.
    let ident = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let tail = message.trim_end().strip_suffix(')').and_then(|m| m.rsplit_once('(')).map(|(_, r)| r).filter(|r| ident(r));
    let head = message.strip_prefix('(').and_then(|m| m.split_once(')')).map(|(r, _)| r).filter(|r| ident(r));
    // Swift's diagnostic group, `… constant [#VariableNeverMutated]`, names the warning's rule; still a warning, not lint.
    let group = message.trim_end().strip_suffix(']').and_then(|m| m.rsplit_once(" [#")).filter(|(_, g)| ident(g));
    let lint = message.contains(" Violation: ") || head.is_some();
    let (rule, message) = match group {
        Some((m, g)) => (Some(g), m),
        None if message.contains(" Violation: ") => (tail, message),
        None => (head, message),
    };
    Some(json!({"kind": if lint { "lint" } else { kind }, "rule": rule, "path": path, "line": line, "col": col, "message": message.trim()}))
}

/// `<path>:<line>:<col>` at the start of `s`, and what follows it (after `: ` or ` - `, or nothing).
fn place(s: &str) -> Option<(&str, u64, u64, &str)> {
    fn num(s: &str) -> Option<(u64, &str)> {
        let n = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        Some((s[..n].parse().ok()?, &s[n..]))
    }
    s.match_indices(':').find_map(|(i, _)| {
        let (line, r) = num(&s[i + 1..])?;
        let (col, r) = num(r.strip_prefix(':')?)?;
        let after = if r.is_empty() { Some("") } else { r.strip_prefix(": ").or_else(|| r.strip_prefix(" - ")) };
        Some((&s[..i], line, col, after?))
    })
}

/// tsc's `<path>(<line>,<col>): …`.
fn tsc_place(s: &str) -> Option<(&str, u64, u64, &str)> {
    let (path, r) = s.split_once('(')?;
    let (at, rest) = r.split_once("): ")?;
    let (line, col) = at.split_once(',')?;
    Some((path, line.parse().ok()?, col.parse().ok()?, rest))
}

/// libtest's outcome line `test … ok` (`FAILED`, `ignored…`). Anchored like `test_failed`; the run summary reports
/// none.
fn libtest_outcome(line: &str) -> Option<&'static str> {
    let p = plain(line);
    let (_, outcome) = p.trim().strip_prefix("test ").and_then(|r| r.rsplit_once(" ... "))?;
    match outcome {
        "ok" => Some("passed"),
        "FAILED" => Some("failed"),
        o if o.starts_with("ignored") => Some("skipped"),
        _ => None,
    }
}

/// XCTest's `Test Case '…' passed (…)` and Swift Testing's `✔ Test … passed after …` (`􁁛`/`􀢄` on macOS). Anchored
/// like `test_failed`; suite and run summaries, and Swift Testing's `recorded an issue`, report none.
fn xctest_outcome(line: &str) -> Option<&'static str> {
    let p = plain(line);
    let l = p.trim();
    let words = [("' passed (", "passed"), ("' failed (", "failed"), ("' skipped (", "skipped")];
    if l.starts_with("Test Case '") {
        return words.iter().find(|(w, _)| l.contains(w)).map(|(_, o)| *o);
    }
    let rest = ["✔ Test ", "✘ Test ", "􁁛 Test ", "􀢄 Test "].iter().find_map(|m| l.strip_prefix(m)).filter(|r| !r.starts_with("run with "))?;
    [(" passed after ", "passed"), (" failed after ", "failed")].iter().find(|(w, _)| rest.contains(w)).map(|(_, o)| *o)
}

/// Whether `line` is part of the diagnostic above it in swiftc's (and SwiftPM's) grouped format: a numbered source
/// line (` 85 | …`, `589 |`), a marker or macro-expansion line (`    |   \`- warning: …`, `    +--- macro expansion
/// @X ---`), an elided-lines `   :`, a `` `- <path>: note: … `` pointer, or a `<path>:<l>:<c>: note: …` header, which
/// always belongs to a warning or error. Clang's caret lines are left out: seen only in an app's run-time Metal
/// output, and a caret follows its code line, so it couldn't join the header anyway.
fn swift_continues(line: &str) -> bool {
    let p = plain(line);
    let t = p.trim_start();
    let indented = t.len() < p.len();
    let numbered = t.split_once(" |").is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let marker = indented && (t.starts_with('|') || t.starts_with("+---") || t.trim_end() == ":");
    let note = !indented && place(t).is_some_and(|(_, _, _, rest)| rest.starts_with("note: "));
    numbered || marker || t.starts_with("`- ") || note
}

/// Whether `line` is part of the rustc diagnostic above it in rustc's human-readable form (a task's `cargo build`;
/// Cargo targets read JSON instead): its `  --> path:l:c`, a `   = note:`/`help:`/`warning:` line, or the `...` of
/// elided source; its `  |` and `12 | …` lines are swiftc's shapes too (`swift_continues`). An unindented `note: …`
/// starts a record: rustc's sub-notes look like xcodebuild's standalone `note: Removed stale file …`, far more common.
fn rustc_continues(line: &str) -> bool {
    let p = plain(line);
    let t = p.trim_start();
    let indented = t.len() < p.len();
    let sub = ["--> ", "= note: ", "= help: ", "= warning: "].iter().any(|s| t.starts_with(s));
    (indented && sub) || p.trim_end() == "..."
}

// ponytail: regex heuristic, replace with per-adapter parsers when it misclassifies
pub fn heuristic(src: Source, line: &str) -> Level {
    let l = line.to_lowercase();
    // Progress lines of passing tests (seen from xcodebuild 27): Swift Testing's (the private-use ones on macOS) and
    // XCTest's `Test Case '…' started.`/`passed (…)` and `Test Suite '…' started at`/`passed at`. A passing test
    // named "… error …" mustn't read as a failure.
    let progress = || {
        let p = plain(line);
        let l = p.trim_start();
        let xctest = (l.starts_with("Test Case '") || l.starts_with("Test Suite '")) && (l.contains("' started") || l.contains("' passed"));
        xctest || ["✔ ", "◇ ", "↳ ", "􁁛 ", "􀟈 ", "􀄵 "].iter().any(|m| l.starts_with(m))
    };
    if waits_for_lock(line) || plain(line).trim_start().starts_with("[!] ") {
        Level::Warn
    } else if test_failed(line) {
        Level::Error
    } else if progress() {
        Level::Info
    } else if error_word(line) {
        Level::Error
    } else if l.contains("warn") || src == Source::Stderr {
        Level::Warn
    } else {
        Level::Info
    }
}

/// Whether `line` has a word saying something failed: `error`, `errors`, `fatal`, `panic`, `panicked`, or one ending
/// in `error` right before a `:` (`TypeError:`). A word is a run of letters, digits and `_`, so `ErrorView`,
/// `YES_ERROR`, `nw_endpoint_flow_failed_with_error` and `-Werror` aren't one; tokens with a `/` (paths, URLs) are skipped.
fn error_word(line: &str) -> bool {
    let in_word = |c: char| c.is_alphanumeric() || c == '_';
    line.split_whitespace().filter(|t| !t.contains('/')).any(|tok| {
        let tok = tok.to_lowercase();
        let mut rest = tok.as_str();
        while !rest.is_empty() {
            rest = &rest[rest.find(in_word).unwrap_or(rest.len())..];
            let (word, after) = rest.split_at(rest.find(|c| !in_word(c)).unwrap_or(rest.len()));
            if matches!(word, "error" | "errors" | "fatal" | "panic" | "panicked") || (word.ends_with("error") && after.starts_with(':')) {
                return true;
            }
            rest = after;
        }
        false
    })
}

/// Whether `argv` runs xcodebuild or tuist (or `xcrun xcodebuild`), whose output `Xcode`'s rules read.
// ponytail: argv[0] (and xcrun's argv[1]) only: `sh -c "…xcodebuild…"`, a script or `env X=1 xcodebuild` don't match,
// so their output gets the generic heuristic; look inside `sh -c` too if that misclassifies.
pub fn runs_xcode(argv: &[String]) -> bool {
    let base = |s: &String| Path::new(s).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match argv {
        [a, ..] if matches!(base(a).as_str(), "xcodebuild" | "tuist") => true,
        [a, b, ..] => base(a) == "xcrun" && b == "xcodebuild",
        _ => false,
    }
}

/// The command `t` runs, as words: an exec's own `args`; a task's `collie.toml` line split on whitespace; else none.
pub fn command_of(t: &Target, args: &[String]) -> Vec<String> {
    match (t.ecosystem.as_str(), t.kind.as_str()) {
        ("task", "exec") => args.to_vec(),
        ("task", _) => config(&t.dir).ok().and_then(|c| c.tasks.get(&t.name).map(|d| d.cmd.split_whitespace().map(String::from).collect())).unwrap_or_default(),
        _ => vec![],
    }
}

pub struct Cargo;

impl Cargo {
    fn select(t: &Target, cmd: &mut Command) {
        if let Some(p) = &t.package {
            cmd.args(["-p", p]);
        }
        cmd.args([format!("--{}", t.kind), t.name.clone()]).current_dir(&t.dir);
    }
}

impl Ecosystem for Cargo {
    fn id(&self) -> &'static str {
        "cargo"
    }

    fn detect(&self, dir: &Path) -> Result<Vec<Target>> {
        if !dir.ancestors().any(|d| d.join("Cargo.toml").is_file()) {
            return Ok(vec![]);
        }
        let out = Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(dir)
            .output()?;
        ensure!(out.status.success(), "cargo metadata: {}", String::from_utf8_lossy(&out.stderr).trim());
        let m: Value = serde_json::from_slice(&out.stdout)?;
        let root = PathBuf::from(m["workspace_root"].as_str().unwrap_or_default());
        let target_dir = m["target_directory"].as_str().map(PathBuf::from);
        let mut targets = vec![];
        for pkg in m["packages"].as_array().into_iter().flatten() {
            for t in pkg["targets"].as_array().into_iter().flatten() {
                let kind = t["kind"][0].as_str().unwrap_or_default();
                if kind == "bin" || kind == "example" {
                    targets.push(Target {
                        name: t["name"].as_str().unwrap_or_default().into(),
                        ecosystem: self.id().into(),
                        dir: root.clone(),
                        kind: kind.into(),
                        package: pkg["name"].as_str().map(Into::into),
                        needs_port: false,
                        port: None,
                        target_dir: target_dir.clone(),
                    });
                }
            }
        }
        // What `cargo test` runs at the root; `-- -p <pkg>` or `-- <filter>` narrows it.
        targets.push(Target {
            name: "cargo test".into(),
            ecosystem: self.id().into(),
            dir: root,
            kind: "test".into(),
            package: None,
            needs_port: false,
            port: None,
            target_dir,
        });
        Ok(targets)
    }

    fn needs(&self, t: &Target) -> Vec<Resource> {
        port_need(t)
    }

    fn build(&self, t: &Target, ctx: &Ctx) -> Option<Command> {
        let mut cmd = Command::new("cargo");
        if t.kind == "test" {
            // The run's args (`-p alpha`, a filter, `-- --nocapture`), so the build compiles only what the run needs; but
            // not `--doc`, which cargo refuses with `--no-run` (doc tests are compiled by the run itself).
            cmd.args(["test", "--no-run", "--message-format=json"]).args(ctx.args.iter().filter(|a| *a != "--doc")).current_dir(&t.dir);
            return Some(cmd);
        }
        cmd.args(["build", "--message-format=json"]);
        Self::select(t, &mut cmd);
        Some(cmd)
    }

    fn run(&self, t: &Target, _: &Ctx) -> Command {
        if t.kind == "test" {
            // JSON, as the build: the warnings `cargo test` replays then come as JSON lines `classify` can tell apart.
            let mut cmd = Command::new("cargo");
            cmd.args(["test", "--no-fail-fast", "--message-format=json"]).current_dir(&t.dir);
            return cmd;
        }
        // Not `cargo run`: even with -q it replays cached compiler warnings on stderr, duplicating the build's warn records.
        // ponytail: target dir comes from detect's env, so a caller-set CARGO_TARGET_DIR or a [build] target triple makes this run a
        // stale binary or fail to spawn; take `executable` from the build's compiler-artifact message to fix.
        let dir = t.target_dir.clone().unwrap_or_else(|| t.dir.join("target")).join("debug");
        let bin = if t.kind == "example" { dir.join("examples").join(&t.name) } else { dir.join(&t.name) };
        let mut cmd = Command::new(bin);
        cmd.current_dir(&t.dir);
        cmd
    }

    fn classify(&self, src: Source, line: &str) -> (Level, String) {
        if src == Source::Stderr && !waits_for_lock(line) {
            let t = line.trim();
            // The targets `cargo test --no-fail-fast` lists under `error: 1 target failed:`.
            if t.starts_with("`--") && t.ends_with('`') {
                return (Level::Error, line.into());
            }
            // cargo's own `{verb:>12} …` status lines around a test run; `Blocking` stays a warning (`waits_for_lock`).
            let verbs = ["Finished", "Running", "Doc-tests", "Compiling", "Fresh", "Updating", "Locking", "Downloading", "Downloaded", "Adding"];
            let verb = line.get(..12).map(str::trim_start).unwrap_or_default();
            if line.as_bytes().get(12) == Some(&b' ') && verbs.contains(&verb) {
                return (Level::Info, line.into());
            }
        }
        let cargo_json = ["compiler-message", "compiler-artifact", "build-script-executed", "build-finished"];
        let msg = match serde_json::from_str::<Value>(line) {
            Ok(v) if src == Source::Build || cargo_json.contains(&v["reason"].as_str().unwrap_or_default()) => v,
            _ => return (heuristic(src, line), line.into()),
        };
        let (level, text) = match msg["reason"].as_str().unwrap_or_default() {
            "compiler-message" => {
                let level = match msg["message"]["level"].as_str().unwrap_or_default() {
                    l if l.starts_with("error") => Level::Error,
                    "warning" => Level::Warn,
                    _ => Level::Info,
                };
                (level, msg["message"]["rendered"].as_str().unwrap_or_default().trim_end().into())
            }
            "build-finished" => (Level::Info, format!("build finished, success={}", msg["success"])),
            "compiler-artifact" => (Level::Debug, format!("compiled {}", msg["target"]["name"].as_str().unwrap_or_default())),
            other => (Level::Debug, other.into()),
        };
        // On stdout, warnings are `cargo test` replaying the build's; errors are a real recompile failing (after `-p`,
        // `--features` or an edit), so they keep their level.
        (if src == Source::Build || level == Level::Error { level } else { Level::Debug }, text)
    }

    fn results(&self, t: &Target, _: &Ctx, recs: &[(String, Record)]) -> Option<Value> {
        let tests = cargo_tests(recs);
        (t.kind == "test" && !tests.is_empty()).then(|| json!({"sep": "::", "tests": tests}))
    }

    /// Filters after the first `--`.
    fn select(&self, t: &Target, ids: &[String], mut args: Vec<String>) -> Result<Vec<String>> {
        if t.kind != "test" {
            bail!(crate::daemon::coded("bad_request", only_refused(t)));
        }
        if !args.iter().any(|a| a == "--") {
            args.push("--".into());
        }
        // ponytail: libtest filters by substring, in every test binary and the doc tests: a group (`tests::`) also
        // matches `other::tests::x`, and `--exact` holds for all filters, so a mix with a group runs without it.
        args.extend(ids.iter().cloned());
        if !ids.iter().any(|id| id.ends_with("::")) {
            args.push("--exact".into());
        }
        Ok(args)
    }

    fn outcome(&self, line: &str) -> Option<&'static str> {
        libtest_outcome(line)
    }
}

/// libtest's `test <path> ... ok|FAILED|ignored` lines on stdout, each failure with the location and message its
/// `---- <path> stdout ----` block's panic gives. Stable libtest prints no durations (its JSON output needs nightly).
fn cargo_tests(recs: &[(String, Record)]) -> Vec<Value> {
    let lines: Vec<(&str, String)> = recs.iter().filter(|(_, r)| r.source == Source::Stdout).map(|(c, r)| (c.as_str(), plain(&r.line))).collect();
    let mut tests: Vec<Value> = vec![];
    let mut captured: Option<&str> = None;
    for (k, (cursor, l)) in lines.iter().enumerate() {
        let l = l.trim_end();
        if let (Some(status), Some((path, _))) = (Cargo.outcome(l), l.strip_prefix("test ").and_then(|r| r.rsplit_once(" ... "))) {
            // ponytail: the same path in two test binaries (or as a doc test) is one entry, a failure winning
            match tests.iter_mut().find(|t| t["id"] == path) {
                Some(t) if status == "failed" => t["status"] = status.into(),
                Some(_) => {}
                None => tests.push(json!({"id": path, "name": path.rsplit("::").next(), "status": status, "duration_ms": null, "failure": null})),
            }
        } else if let Some(path) = l.strip_prefix("---- ").and_then(|s| s.strip_suffix(" stdout ----")) {
            captured = Some(path);
        } else if let (Some(path), Some((_, at))) = (captured, l.split_once(" panicked at ")) {
            // `src/lib.rs:15:9:`, the message on the next line.
            let mut loc = at.trim_end_matches(':').rsplitn(3, ':').skip(1);
            let (line, file) = (loc.next().and_then(|n| n.parse::<u64>().ok()), loc.next());
            let message = lines.get(k + 1).map(|(_, m)| m.trim());
            if let Some(t) = tests.iter_mut().find(|t| t["id"] == path) {
                t["failure"] = json!({"message": message, "file": file, "line": line, "cursor": cursor});
            }
            captured = None;
        }
    }
    tests
}

pub struct Node;

impl Ecosystem for Node {
    fn id(&self) -> &'static str {
        "node"
    }

    fn detect(&self, dir: &Path) -> Result<Vec<Target>> {
        let Some(root) = dir.ancestors().find(|d| d.join("package.json").is_file()) else {
            return Ok(vec![]);
        };
        let pkg: Value = serde_json::from_str(&std::fs::read_to_string(root.join("package.json"))?)?;
        Ok(pkg["scripts"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, _)| Target {
                name: name.clone(),
                ecosystem: self.id().into(),
                dir: root.into(),
                kind: if name == "test" || name.starts_with("test:") { "test" } else { "script" }.into(),
                package: pkg["name"].as_str().map(Into::into),
                needs_port: name == "dev" || name == "start",
                port: None,
                target_dir: None,
            })
            .collect())
    }

    fn needs(&self, t: &Target) -> Vec<Resource> {
        port_need(t)
    }

    fn build(&self, _: &Target, _: &Ctx) -> Option<Command> {
        None
    }

    fn run(&self, t: &Target, _: &Ctx) -> Command {
        // Nearest lockfile up the tree, so a workspace package uses its root's pm; package-lock.json is listed so it stops the walk.
        // ponytail: walks to /, so a stray lockfile above the project picks the pm; stop at the first non-workspace package.json if that bites
        let locks = [("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn"), ("bun.lock", "bun"), ("bun.lockb", "bun"), ("package-lock.json", "npm")];
        let pm = t.dir.ancestors().find_map(|d| locks.iter().find(|(f, _)| d.join(f).exists())).map_or("npm", |(_, pm)| pm);
        let mut cmd = Command::new(pm);
        cmd.args(["run", &t.name]).current_dir(&t.dir);
        if pm == "npm" {
            // So run args reach the script: npm takes `--fix` after the script name as its own config.
            cmd.arg("--");
        }
        cmd
    }

    fn classify(&self, src: Source, line: &str) -> (Level, String) {
        (heuristic(src, line), line.into())
    }

    /// A `test` script can run anything, as a task can.
    // ponytail: no node runner (node:test, vitest, jest) is counted yet; add its lines here when one is wanted.
    fn outcome(&self, line: &str) -> Option<&'static str> {
        Task.outcome(line)
    }
}

/// Tasks declared in `collie.toml` (`[tasks.<name>] cmd = "…"`, kind `task`), and `exec`'s ad hoc commands (kind
/// `exec`, never detected: the daemon builds the target, and the command arrives as the run args).
pub struct Task;

impl Ecosystem for Task {
    fn id(&self) -> &'static str {
        "task"
    }

    fn detect(&self, dir: &Path) -> Result<Vec<Target>> {
        let Some(root) = dir.ancestors().find(|d| d.join("collie.toml").is_file()) else { return Ok(vec![]) };
        Ok(config(root)?
            .tasks
            .into_keys()
            .map(|name| Target {
                name,
                ecosystem: self.id().into(),
                dir: root.into(),
                kind: "task".into(),
                package: None,
                needs_port: false,
                port: None,
                target_dir: None,
            })
            .collect())
    }

    fn needs(&self, t: &Target) -> Vec<Resource> {
        port_need(t)
    }

    fn build(&self, _: &Target, _: &Ctx) -> Option<Command> {
        None
    }

    fn run(&self, t: &Target, ctx: &Ctx) -> Command {
        // The daemon appends the run args, which `sh` passes to the line as "$@": at its end, or as exec's whole command.
        let line = match t.kind.as_str() {
            "exec" => Ok("exec".to_string()),
            _ => config(&t.dir).and_then(|c| c.tasks.get(&t.name).map(|d| d.cmd.trim_end().to_string()).with_context(|| format!("no task {} in collie.toml", t.name))),
        };
        let line = match line {
            Ok(l) => l,
            Err(e) => return failing(e),
        };
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &format!("{line} \"$@\""), "sh"]).current_dir(&t.dir);
        boot_first(ctx, cmd)
    }

    fn classify(&self, src: Source, line: &str) -> (Level, String) {
        (heuristic(src, line), line.into())
    }

    /// A task's command can be any runner's: `cargo test` and `xcodebuild test` both count.
    fn outcome(&self, line: &str) -> Option<&'static str> {
        Cargo.outcome(line).or_else(|| Xcode.outcome(line))
    }

    /// As `outcome`, any runner's: `swift build` and `swift test` (not xcodebuild, so not `XcodeTool`) print swiftc's
    /// grouped diagnostics, and `cargo build` rustc's human-readable ones.
    fn continues(&self, open: &str, line: &str) -> bool {
        rustc_continues(line) || Xcode.continues(open, line)
    }

    /// Used only by `--sim` and `--physical`, which lease iOS devices.
    fn platform(&self) -> Option<&'static dyn Platform> {
        Some(&Ios)
    }
}

/// A task or exec that runs xcodebuild or tuist (`runs_xcode`): a `Task` whose output is read by Xcode's build rules.
/// Its own type because `classify` has no target, and an Xcode app's run phase needs other rules.
pub struct XcodeTool;

impl Ecosystem for XcodeTool {
    fn id(&self) -> &'static str {
        Task.id()
    }

    fn detect(&self, dir: &Path) -> Result<Vec<Target>> {
        Task.detect(dir)
    }

    fn needs(&self, t: &Target) -> Vec<Resource> {
        Task.needs(t)
    }

    fn build(&self, t: &Target, ctx: &Ctx) -> Option<Command> {
        Task.build(t, ctx)
    }

    fn run(&self, t: &Target, ctx: &Ctx) -> Command {
        Task.run(t, ctx)
    }

    fn classify(&self, _: Source, line: &str) -> (Level, String) {
        (xcode_level(line), line.into())
    }

    /// Task's, not just Xcode's: the script may run `cargo test` beside xcodebuild.
    fn outcome(&self, line: &str) -> Option<&'static str> {
        Task.outcome(line)
    }

    fn continues(&self, open: &str, line: &str) -> bool {
        Task.continues(open, line)
    }

    fn platform(&self) -> Option<&'static dyn Platform> {
        Some(&Ios)
    }
}

/// Boots the device, streams the app's unified log, installs and launches it, and runs until the app exits.
/// Args: udid, app path, bundle id, executable name, `1` to open the device in DeviceHub. That goes through DeviceHub's
/// (undocumented) `devices:` URL (a simulator's CoreDevice id is its UDID): DeviceHub ignores a `-CurrentDeviceUDID` argument.
/// The app's stdout/stderr go to files that are followed only after simctl's `<bundle id>: <pid>` line is printed, so
/// no app output can come before that line. The pid is a host process, so `kill -0` sees it exit. The files live under
/// ~/Library because the simulator maps `/tmp` and `/var/folders` (`$TMPDIR`) into the device's own data directory.
/// Each run removes the dirs of runs whose script is gone (killed with SIGKILL, so its EXIT trap never ran).
const LAUNCH: &str = r#"
xcrun simctl bootstatus "$1" -b || exit 1
[ "$5" = 1 ] && open "devices://device/open?id=$1"
for d in "$HOME"/Library/Caches/collie.*.*; do
  p=${d##*/collie.}; p=${p%%.*}
  case $p in ''|*[!0-9]*) continue ;; esac
  [ -d "$d" ] && ! kill -0 "$p" 2>/dev/null && rm -rf "$d"
done
out=$(mktemp -d "$HOME/Library/Caches/collie.$$.XXXXXX") || exit 1
trap 'kill $log $tails 2>/dev/null; rm -rf "$out"' EXIT
trap 'exit 143' TERM INT
xcrun simctl spawn "$1" log stream --style ndjson --level debug --predicate "process == \"$4\"" &
log=$!
xcrun simctl install "$1" "$2" || exit 1
# As Xcode does: with stdout a file, print() would otherwise only show up when its buffer fills or the app exits.
export SIMCTL_CHILD_NSUnbufferedIO="${SIMCTL_CHILD_NSUnbufferedIO:-YES}"
# The first launch on a freshly slimmed device (simslim 0.13, iOS 27.0) takes ~6 s and often fails "did not return a
# process handle" (NSPOSIXErrorDomain 3) though the app did start; so does a relaunch that terminates that app. Asking
# again without terminating gets the running app's pid (or launches it, if it really didn't start).
udid=$1 bundle=$3
launch() { xcrun simctl launch "$@" --stdout="$out/stdout" --stderr="$out/stderr" "$udid" "$bundle"; }
if ! launched=$(launch --terminate-running-process 2>"$out/launch"); then
  cat "$out/launch" >&2
  grep -q "did not return a process handle" "$out/launch" || exit 1
  echo "collie: launch failed, retrying"
  sleep 1
  launched=$(launch) || exit 1
fi
echo "$launched"
tail -F -n +1 "$out/stdout" 2>/dev/null & tails=$!
tail -F -n +1 "$out/stderr" >&2 2>/dev/null & tails="$tails $!"
while kill -0 "${launched##*: }" 2>/dev/null; do sleep 1; done
sleep 1
"#;

/// Installs and launches the app on a physical device and runs until it exits. Args: udid, app path, bundle id.
/// `--console` bridges the app's streams only "if the app is not already running", hence `--terminate-existing`; a
/// TERM to devicectl is forwarded to the app. No `-e` (it overrides every `DEVICECTL_CHILD_*`), no `-j -` (refused
/// with `--console`).
const LAUNCH_DEVICE: &str = r#"
xcrun devicectl device install app --device "$1" "$2" || exit 1
export DEVICECTL_CHILD_NSUnbufferedIO="${DEVICECTL_CHILD_NSUnbufferedIO:-YES}"
exec xcrun devicectl device process launch --console --terminate-existing --device "$1" "$3"
"#;

pub struct Xcode;

impl Xcode {
    /// `xcodebuild` with the container, scheme and destination that build and showBuildSettings must share, and the
    /// caller's env with `.xcode-version`'s `DEVELOPER_DIR` (an Xcode in `apps`) over it.
    fn xcodebuild(t: &Target, ctx: &Ctx, apps: &Path) -> Result<Command> {
        let base = || -> Result<Command> {
            let container = t.package.as_deref().unwrap_or_default();
            let flag = if container.ends_with(".xcworkspace") { "-workspace" } else { "-project" };
            let mut cmd = Command::new("xcodebuild");
            cmd.envs(&ctx.env);
            if let Some(dev) = developer_dir(&t.dir, apps)? {
                cmd.env("DEVELOPER_DIR", dev);
            }
            // An app scheme's tests are the target `<scheme> (test)`; detection refuses a scheme of that name next to the app.
            // ponytail: a lone test-only scheme named `… (test)` loses the suffix too; keep the scheme name in Target if one appears
            let scheme = if t.kind == "test" { t.name.strip_suffix(" (test)").unwrap_or(&t.name) } else { &t.name };
            cmd.args([flag, container, "-scheme", scheme]).current_dir(&t.dir);
            Ok(cmd)
        };
        let mut cmd = base()?;
        match ctx.device.as_deref().or(ctx.simulator.as_deref()) {
            Some(udid) => _ = cmd.args(["-destination", &format!("id={udid}")]),
            // A build target leases no device: any of the platform its scheme builds for.
            None if t.kind == "build" => {
                if let Some(d) = generic_destination(base()?) {
                    cmd.args(["-destination", d]);
                }
            }
            None => bail!("no dibs on a simulator or device"),
        }
        Ok(cmd)
    }

    fn launch(t: &Target, ctx: &Ctx) -> Result<Command> {
        let mut xb = Self::xcodebuild(t, ctx, Path::new("/Applications"))?;
        let out = xb.args(["-showBuildSettings", "-json"]).output()?;
        ensure!(out.status.success(), "xcodebuild -showBuildSettings: {}", String::from_utf8_lossy(&out.stderr).trim());
        let all: Value = serde_json::from_slice(&out.stdout)?;
        let s = all.as_array().into_iter().flatten().map(|e| &e["buildSettings"]).find(|s| s["WRAPPER_EXTENSION"] == "app");
        let s = s.context("scheme builds no .app")?;
        Ok(Self::launch_script(t, ctx, s, &xb))
    }

    /// The launch script for app `settings` (its build settings), with `xb`'s env.
    fn launch_script(t: &Target, ctx: &Ctx, s: &Value, xb: &Command) -> Command {
        let get = |k: &str| s[k].as_str().unwrap_or_default().to_string();
        let app = format!("{}/{}", get("TARGET_BUILD_DIR"), get("WRAPPER_NAME"));
        let mut cmd = Command::new("sh");
        let bundle = get("PRODUCT_BUNDLE_IDENTIFIER");
        if let Some(udid) = &ctx.device {
            cmd.args(["-c", LAUNCH_DEVICE, "sh", udid, &app, &bundle]).current_dir(&t.dir);
        } else {
            let udid = ctx.simulator.clone().unwrap_or_default();
            let head = if ctx.head { "1" } else { "0" };
            cmd.args(["-c", LAUNCH, "sh", &udid, &app, &bundle, &get("EXECUTABLE_NAME"), head]).current_dir(&t.dir);
        }
        for (k, v) in xb.get_envs() {
            if let Some(v) = v {
                cmd.env(k, v);
            }
        }
        if ctx.device.is_some() {
            // A device has no `log stream` for collie to follow: this has the app mirror its `Logger` output to stderr,
            // which `--console` bridges, debug level included (DT_MODE alone mirrors info and up). Set here so it beats
            // the caller's env.
            cmd.env("DEVICECTL_CHILD_OS_ACTIVITY_DT_MODE", "enable");
            cmd.env("DEVICECTL_CHILD_OS_ACTIVITY_MODE", "debug");
        }
        cmd
    }
}

/// An Xcode app's run phase on a physical device, where the app's unified log arrives mirrored to stderr as
/// `<date> <time>.<µs><tz> <process>[<pid>:<tid>] <message>`, a message's further lines unprefixed. Its own reader (as
/// `XcodeTool` is) because only a run knows it is on a device: simulator runs and xctest print prefixed NSLog lines
/// that keep Xcode's rules.
pub struct XcodeDevice;

impl Ecosystem for XcodeDevice {
    fn id(&self) -> &'static str {
        Xcode.id()
    }

    fn detect(&self, dir: &Path) -> Result<Vec<Target>> {
        Xcode.detect(dir)
    }

    fn needs(&self, t: &Target) -> Vec<Resource> {
        Xcode.needs(t)
    }

    fn build(&self, t: &Target, ctx: &Ctx) -> Option<Command> {
        Xcode.build(t, ctx)
    }

    fn run(&self, t: &Target, ctx: &Ctx) -> Command {
        Xcode.run(t, ctx)
    }

    /// A mirrored line's level is the message's own (`message_level`), a crash line's Error; anything else, Xcode's rules.
    fn classify(&self, src: Source, line: &str) -> (Level, String) {
        match mirrored(line) {
            Some(at) => (message_level(&line[at..]), line.into()),
            None if crash_line(line) => (Level::Error, line.into()),
            None => Xcode.classify(src, line),
        }
    }

    fn launched(&self, src: Source, line: &str) -> bool {
        Xcode.launched(src, line)
    }

    /// An unprefixed line continues a mirrored message (error words and all: an app's error dump is one message), or
    /// a crash line with what follows it. A crash or abort line (`crash_line`) starts its own record, so it can't hide
    /// in the info record before it; devicectl's own lines and the app's prints start records too.
    fn continues(&self, open: &str, line: &str) -> bool {
        mirrored(line).is_none() && !crash_line(line) && (mirrored(open).is_some() || crash_line(open))
    }

    fn platform(&self) -> Option<&'static dyn Platform> {
        Xcode.platform()
    }
}

/// Whether an app's own stderr `line` says the process is crashing: the Swift runtime's `[File.swift:12: ]Fatal
/// error: …`, `Precondition failed: …`, `Assertion failed: …`, ObjC's `*** Terminating app due to uncaught exception …`,
/// C++'s `libc++abi: terminating …`.
fn crash_line(line: &str) -> bool {
    let l = line.trim();
    ["Fatal error: ", "Precondition failed: ", "Assertion failed: "].iter().any(|m| l.starts_with(m) || l.contains(&format!(": {m}")))
        || l.starts_with("*** Terminating app due to uncaught exception")
        || l.starts_with("libc++abi: terminating")
}

/// Where the message starts in a line the unified log mirrored to stderr
/// (`2026-10-29 15:58:06.441197+0100 Demo_Dev[41709:12033783] …`); None for any other line.
fn mirrored(line: &str) -> Option<usize> {
    let stamp = line.get(..31)?.as_bytes();
    let digits = |r: std::ops::Range<usize>| stamp[r].iter().all(u8::is_ascii_digit);
    let seps = [(4, b'-'), (7, b'-'), (10, b' '), (13, b':'), (16, b':'), (19, b'.')].iter().all(|&(i, c)| stamp[i] == c);
    let ok = seps && matches!(stamp[26], b'+' | b'-') && [0..4, 5..7, 8..10, 11..13, 14..16, 17..19, 20..26, 27..31].into_iter().all(digits);
    if !ok || line.as_bytes().get(31) != Some(&b' ') {
        return None;
    }
    // `<process>[<pid>:<tid>] `: the process name may hold spaces and brackets, so the first `[digits:digits] `.
    line[32..].match_indices('[').find_map(|(i, _)| {
        let r = &line[32 + i + 1..];
        let (ids, _) = r.split_once("] ")?;
        let (pid, tid) = ids.split_once(':')?;
        let num = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        (num(pid) && num(tid)).then_some(32 + i + 1 + ids.len() + 2)
    })
}

/// A mirrored message's level: its first word past any leading `[…]` tags (the category, Firebase's `[I-ACS…]`) and
/// emoji, `<>`/`[]`/`:` trimmed, when that is a level word in any case (`🐛 DEBUG:`, `<Notice>`, `ERROR`); else Error
/// when `error`, `fault` or `critical` is a word anywhere; else Info.
// ponytail: a level word must lead the message (a passing "…to debug" doesn't count); per-app markers if one leads with something else.
fn message_level(message: &str) -> Level {
    let mut m = message.trim_start();
    // Leading `[…]` groups, nested ones too (`[[FirebaseAnalytics]]`).
    while m.starts_with('[') {
        let mut depth = 0;
        let end = m.char_indices().find_map(|(i, c)| {
            depth += match c {
                '[' => 1,
                ']' => -1,
                _ => 0,
            };
            (depth == 0).then_some(i + 1)
        });
        let Some(end) = end else { break };
        m = m[end..].trim_start();
    }
    let lead = m.split_whitespace().find(|w| w.chars().any(|c| c.is_ascii_alphabetic()));
    let lead = lead.map(|w| w.trim_matches(|c| matches!(c, '<' | '>' | '[' | ']' | ':')).to_ascii_lowercase());
    match lead.as_deref() {
        Some("debug") => Level::Debug,
        Some("info" | "notice") => Level::Info,
        Some("warn" | "warning") => Level::Warn,
        Some("error" | "fault" | "critical") => Level::Error,
        _ if message.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| matches!(w.to_ascii_lowercase().as_str(), "error" | "fault" | "critical")) => Level::Error,
        _ => Level::Info,
    }
}

/// `cmd`, after booting `ctx`'s leased simulator (none for a physical device: unchanged): xcodebuild shuts down a
/// simulator it booted itself once its tests end, so every rerun on a carried device would boot it again.
fn boot_first(ctx: &Ctx, cmd: Command) -> Command {
    let Some(udid) = ctx.simulator.as_deref().filter(|_| ctx.device.is_none()) else { return cmd };
    let mut boot = Command::new("sh");
    boot.args(["-c", r#"xcrun simctl bootstatus "$1" -b || exit 1; shift; exec "$@""#, "sh", udid]);
    boot.arg(cmd.get_program()).args(cmd.get_args());
    if let Some(dir) = cmd.get_current_dir() {
        boot.current_dir(dir);
    }
    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            boot.env(k, v);
        }
    }
    boot
}

/// A command that prints `e` as a build error and fails; lets build()/run() report errors through their logs.
fn failing(e: anyhow::Error) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", r#"printf 'collie: error: %s\n' "$1" >&2; exit 1"#, "sh", &format!("{e:#}")]);
    cmd
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let rest = tag.split_once(&format!("{name} = \""))?.1;
    rest.split('"').next()
}

/// Projects a workspace references; `group:` locations nest under their enclosing group's.
fn workspace_projects(ws: &Path) -> Vec<PathBuf> {
    let Ok(xml) = std::fs::read_to_string(ws.join("contents.xcworkspacedata")) else { return vec![] };
    let root = ws.parent().unwrap_or(ws).to_path_buf();
    let (mut groups, mut projects) = (vec![root.clone()], vec![]);
    for tag in xml.split('<').skip(1) {
        if tag.starts_with("/Group") {
            groups.pop();
            continue;
        }
        let group = tag.starts_with("Group");
        if !group && !tag.starts_with("FileRef") {
            continue;
        }
        let base = groups.last().unwrap_or(&root);
        let path = match attr(tag, "location").and_then(|l| l.split_once(':')) {
            Some(("group", p)) => base.join(p),
            Some(("container", p)) => root.join(p),
            Some(("absolute", p)) => PathBuf::from(p),
            _ => base.clone(),
        };
        if group && !tag.trim_end().ends_with("/>") {
            groups.push(path);
        } else if path.extension().is_some_and(|e| e == "xcodeproj") {
            projects.push(path);
        }
    }
    projects
}

/// Whether the scheme's Run action launches an `.app` (not an extension, framework or test bundle).
fn runs_app(scheme: &str) -> bool {
    let launch = scheme.split("<LaunchAction").nth(1).and_then(|s| s.split("</LaunchAction>").next());
    let runnable = launch.and_then(|l| l.split("<BuildableProductRunnable").nth(1));
    runnable.and_then(|r| attr(r, "BuildableName")).is_some_and(|n| n.ends_with(".app"))
}

/// Whether the scheme's Build action builds anything (its entries are only there).
fn builds(scheme: &str) -> bool {
    scheme.contains("<BuildActionEntry")
}

/// Each scheme's `generic_destination`, by its `xcodebuild` args (container and scheme): asking takes seconds on a big
/// workspace.
// ponytail: a scheme whose platforms change keeps its old destination until the daemon restarts.
static DESTINATIONS: std::sync::Mutex<Vec<(Vec<String>, Option<&'static str>)>> = std::sync::Mutex::new(Vec::new());

/// A generic destination for what scheme `xb` (an `xcodebuild` without one) builds, from its targets'
/// `SUPPORTED_PLATFORMS`; None to let xcodebuild pick.
fn generic_destination(mut xb: Command) -> Option<&'static str> {
    let key: Vec<String> = xb.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    if let Some((_, d)) = DESTINATIONS.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return *d;
    }
    let mut ask = || -> Option<&'static str> {
        let out = xb.args(["-showBuildSettings", "-json"]).output().ok()?;
        let all: Value = serde_json::from_slice(&out.stdout).ok()?;
        let platforms: Vec<&str> = all.as_array()?.iter().filter_map(|e| e["buildSettings"]["SUPPORTED_PLATFORMS"].as_str()).flat_map(|s| s.split(' ')).collect();
        generic_for(&platforms)
    };
    let found = ask();
    DESTINATIONS.lock().unwrap().push((key, found));
    found
}

/// The generic destination for one of `platforms` (`SUPPORTED_PLATFORMS` values): a simulator before a device, which
/// would need signing, and iOS before the others.
fn generic_for(platforms: &[&str]) -> Option<&'static str> {
    let order = [
        ("iphonesimulator", "generic/platform=iOS Simulator"),
        ("macosx", "generic/platform=macOS"),
        ("xrsimulator", "generic/platform=visionOS Simulator"),
        ("watchsimulator", "generic/platform=watchOS Simulator"),
        ("appletvsimulator", "generic/platform=tvOS Simulator"),
    ];
    order.iter().find(|(p, _)| platforms.contains(p)).map(|(_, d)| *d)
}

/// Whether the scheme's Test action has tests to run: testables or a test plan.
fn tests(scheme: &str) -> bool {
    let test = scheme.split("<TestAction").nth(1).and_then(|s| s.split("</TestAction>").next()).unwrap_or_default();
    test.contains("<TestableReference") || test.contains("<TestPlanReference")
}

/// `DEVELOPER_DIR` of the Xcode in `apps` whose version equals `.xcode-version` (nearest up from `dir`); None without the file.
fn developer_dir(dir: &Path, apps: &Path) -> Result<Option<PathBuf>> {
    let Some(want) = dir.ancestors().find_map(|d| std::fs::read_to_string(d.join(".xcode-version")).ok()) else { return Ok(None) };
    let want = want.trim();
    let mut have = vec![];
    for e in std::fs::read_dir(apps)?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !(name.starts_with("Xcode") && name.ends_with(".app")) {
            continue;
        }
        let plist = std::fs::read_to_string(e.path().join("Contents/version.plist")).unwrap_or_default();
        let version = plist.split("<key>CFBundleShortVersionString</key>").nth(1).and_then(|s| s.split("<string>").nth(1));
        let version = version.and_then(|s| s.split("</string>").next()).unwrap_or_default();
        if version == want {
            return Ok(Some(e.path().join("Contents/Developer")));
        }
        have.push(format!("{version} at {}", e.path().display()));
    }
    have.sort();
    bail!(".xcode-version wants Xcode {want}, installed: {}; install it with `xcodes install {want}`", have.join(", "))
}

/// The level of an xcodebuild or tuist line (build, tests, or an Xcode target's lines outside its log stream). Strict,
/// so files and settings named after errors (`ErrorView.swift`, `…_AS_ERROR=YES`) stay info, on stderr too.
fn xcode_level(line: &str) -> Level {
    let p = plain(line);
    let l = p.trim();
    let lower = l.to_lowercase();
    // xcodebuild 27 also says this of a script phase that succeeded (seen building an app's tests).
    let quiet_success = lower.starts_with("error: ") && l.contains("failed with exit code 0 but produced no further output");
    // `path: error: …` from the compiler, a bare `error: …` from the build system; a Swift crash's
    // `File.swift:12: Fatal error: …`; tuist's `[x]`, `❌`, `✖`; `** BUILD FAILED **`, `** TEST FAILED **`.
    let error = lower.contains(": error: ")
        || lower.starts_with("error: ")
        || lower.starts_with("fatal error: ")
        || lower.contains(": fatal error: ")
        || ["[x] ", "❌ ", "✖ "].iter().any(|m| l.starts_with(m))
        || (l.starts_with("** ") && l.ends_with(" FAILED **"))
        || test_failed(line);
    let warning = lower.contains(": warning: ") || lower.starts_with("warning: ") || ["[!] ", "⚠️ ", "⚠ "].iter().any(|m| l.starts_with(m));
    if waits_for_lock(line) || quiet_success {
        Level::Warn
    } else if error {
        Level::Error
    } else if warning {
        Level::Warn
    } else {
        Level::Info
    }
}

impl Ecosystem for Xcode {
    fn id(&self) -> &'static str {
        "xcode"
    }

    fn detect(&self, dir: &Path) -> Result<Vec<Target>> {
        let find = |ext: &str| {
            dir.ancestors().find_map(|d| {
                let mut hits: Vec<_> = std::fs::read_dir(d).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == ext)).collect();
                hits.sort();
                hits.into_iter().next()
            })
        };
        let Some(container) = find("xcworkspace").or_else(|| find("xcodeproj")) else { return Ok(vec![]) };
        let root = container.parent().unwrap_or(dir).to_path_buf();
        // Shared schemes, then the user's own (Tuist generates test schemes there), of the workspace and each project.
        let user = std::env::var("USER").map(|u| format!("xcuserdata/{u}.xcuserdatad/xcschemes"));
        let mut scheme_dirs = vec![];
        for c in std::iter::once(container.clone()).chain(workspace_projects(&container)) {
            scheme_dirs.push(c.join("xcshareddata/xcschemes"));
            scheme_dirs.extend(user.as_ref().ok().map(|u| c.join(u)));
        }
        let mut targets = vec![];
        let mut seen = HashSet::new();
        for d in scheme_dirs {
            let mut files: Vec<_> = std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).collect();
            files.sort();
            for f in files.iter().filter(|f| f.extension().is_some_and(|e| e == "xcscheme")) {
                let name: String = f.file_stem().unwrap_or_default().to_string_lossy().into();
                if !seen.insert(name.clone()) {
                    continue;
                }
                let scheme = std::fs::read_to_string(f)?;
                let app = runs_app(&scheme);
                let target = |name: String, kind: &str| Target {
                    name,
                    ecosystem: self.id().into(),
                    dir: root.clone(),
                    kind: kind.into(),
                    package: Some(container.to_string_lossy().into()),
                    needs_port: false,
                    port: None,
                    target_dir: None,
                };
                if app {
                    targets.push(target(name.clone(), "app"));
                }
                if tests(&scheme) {
                    targets.push(target(if app { format!("{name} (test)") } else { name }, "test"));
                } else if !app && builds(&scheme) {
                    // A framework's or module's: it builds, nothing runs.
                    targets.push(target(name, "build"));
                }
            }
        }
        // Schemes are unique by name, so a clash is a scheme named like an app scheme's tests; `run` would pick one.
        for (i, t) in targets.iter().enumerate() {
            if targets[..i].iter().any(|o| o.name == t.name) {
                let app = t.name.trim_end_matches(" (test)");
                let msg = format!("{}: scheme \"{}\" clashes with the tests of app scheme \"{app}\"; rename it", container.display(), t.name);
                bail!(crate::daemon::coded("bad_config", msg));
            }
        }
        // A tuist workspace has a framework scheme per module, dozens: after the apps and tests, by name.
        targets.sort_by(|a, b| match (a.kind == "build", b.kind == "build") {
            (true, true) => a.name.cmp(&b.name),
            (x, y) => x.cmp(&y),
        });
        Ok(targets)
    }

    fn needs(&self, t: &Target) -> Vec<Resource> {
        if t.kind == "build" { vec![] } else { vec![Resource::Simulator] }
    }

    fn build(&self, t: &Target, ctx: &Ctx) -> Option<Command> {
        let cmd = Self::xcodebuild(t, ctx, Path::new("/Applications")).map(|mut c| {
            c.args([if t.kind == "test" { "build-for-testing" } else { "build" }, "-quiet"]);
            if ctx.device.is_some() {
                c.arg("-allowProvisioningUpdates");
            }
            c
        });
        Some(cmd.unwrap_or_else(failing))
    }

    fn run(&self, t: &Target, ctx: &Ctx) -> Command {
        if t.kind == "test" {
            // ponytail: 600 s per test, then xcodebuild fails it as hung; make it a target setting if a suite needs longer
            let test = ["test-without-building", "-test-timeouts-enabled", "YES", "-maximum-test-execution-time-allowance", "600"];
            // After a failure xcodebuild otherwise runs `simctl diagnose` for up to 10 minutes, keeping the run live.
            let diagnostics = ["-collect-test-diagnostics", "never"];
            let mut xb = match Self::xcodebuild(t, ctx, Path::new("/Applications")) {
                Ok(c) => c,
                Err(e) => return failing(e),
            };
            // Read by `results`; unique per process, since xcodebuild refuses a path that exists.
            xb.args(test).args(diagnostics).arg("-resultBundlePath").arg(ctx.log.with_extension("xcresult"));
            return boot_first(ctx, xb);
        }
        Self::launch(t, ctx).unwrap_or_else(failing)
    }

    fn classify(&self, src: Source, line: &str) -> (Level, String) {
        if src == Source::Build {
            return (xcode_level(line), line.into());
        }
        // devicectl's line when the app dies of a signal; 15 is collie's own stop.
        if let Some(sig) = line.strip_prefix("App terminated due to signal ").filter(|_| src == Source::Stdout) {
            return (if sig == "15." { Level::Info } else { Level::Error }, line.into());
        }
        // simctl's second line when the app won't launch, after `An error was encountered processing the command …`.
        if line.starts_with("The request to open \"") && line.ends_with(" failed.") {
            return (Level::Error, line.into());
        }
        // The run phase (a launch, a test run) prints the app's own output too, so the generic rules, not the build's.
        let Ok(v) = serde_json::from_str::<Value>(line) else { return (heuristic(src, line), line.into()) };
        let Some(msg) = v["eventMessage"].as_str() else { return (heuristic(src, line), line.into()) };
        let level = match v["messageType"].as_str().unwrap_or_default() {
            _ if v["subsystem"].as_str().is_some_and(|s| s.starts_with("com.apple.")) => Level::Debug,
            "Fault" | "Error" => Level::Error,
            "Debug" => Level::Debug,
            _ => Level::Info,
        };
        (level, msg.into())
    }

    /// `-only-testing:<id>` each.
    fn select(&self, t: &Target, ids: &[String], mut args: Vec<String>) -> Result<Vec<String>> {
        if t.kind != "test" {
            bail!(crate::daemon::coded("bad_request", only_refused(t)));
        }
        args.extend(ids.iter().map(|id| format!("-only-testing:{}", id.trim_end_matches('/'))));
        Ok(args)
    }

    fn check_args(&self, t: &Target, args: &[String]) -> Result<()> {
        use crate::daemon::coded;
        match t.kind.as_str() {
            _ if args.is_empty() => Ok(()),
            "app" => bail!(coded("bad_request", format!("{} is an app: its run phase is collie's launch script, which would drop the args", t.name))),
            "build" => bail!(coded("bad_request", format!("{} only builds: there is no run phase for args", t.name))),
            _ => Ok(()),
        }
    }

    fn outcome(&self, line: &str) -> Option<&'static str> {
        xctest_outcome(line)
    }

    fn continues(&self, _: &str, line: &str) -> bool {
        swift_continues(line)
    }

    fn platform(&self) -> Option<&'static dyn Platform> {
        Some(&Ios)
    }

    fn launched(&self, src: Source, line: &str) -> bool {
        // `simctl launch` prints `<bundle id>: <pid>` once the app process exists; the launch script prints nothing of
        // the app's before it. On a device, devicectl prints its line before it bridges the app's streams.
        if src == Source::Stdout && line.starts_with("Launched application with ") && line.ends_with(" bundle identifier.") {
            return true;
        }
        let Some((id, pid)) = line.split_once(": ") else { return false };
        src == Source::Stdout && id.contains('.') && !id.contains(' ') && pid.trim().parse::<u32>().is_ok()
    }

    /// From the run's result bundle, which is deleted after (it reaches tens of MB: package schemes collect coverage).
    fn results(&self, t: &Target, ctx: &Ctx, recs: &[(String, Record)]) -> Option<Value> {
        let bundle = ctx.log.with_extension("xcresult");
        if t.kind != "test" || !bundle.exists() {
            return None;
        }
        // The env xcodebuild ran with: `get test-results` needs Xcode 16+, so the pinned one.
        let mut cmd = Command::new("xcrun");
        cmd.envs(&ctx.env).args(["xcresulttool", "get", "test-results", "tests", "--compact", "--path"]).arg(&bundle);
        if let Ok(Some(dev)) = developer_dir(&t.dir, Path::new("/Applications")) {
            cmd.env("DEVELOPER_DIR", dev);
        }
        let out = cmd.output();
        let _ = std::fs::remove_dir_all(&bundle);
        let out = out.ok().filter(|o| o.status.success())?;
        let tests = xcresult_tests(&serde_json::from_slice(&out.stdout).ok()?, recs);
        (!tests.is_empty()).then(|| json!({"sep": "/", "tests": tests}))
    }
}

/// The test cases in `xcresulttool get test-results tests` output. A case's runnable id is its nearest test bundle's
/// name plus its `nodeIdentifier` (`SumTests/Outer/adds()`), never built from depth: test plans with several
/// configurations or repetitions may add levels (unverified).
fn xcresult_tests(v: &Value, recs: &[(String, Record)]) -> Vec<Value> {
    fn walk(n: &Value, bundle: &str, recs: &[(String, Record)], out: &mut Vec<Value>) {
        let bundle = if n["nodeType"] == "Unit test bundle" || n["nodeType"] == "UI test bundle" { n["name"].as_str().unwrap_or(bundle) } else { bundle };
        let mut kids = n["children"].as_array().into_iter().flatten();
        if n["nodeType"] != "Test Case" {
            return kids.for_each(|c| walk(c, bundle, recs, out));
        }
        let name = n["name"].as_str().unwrap_or_default();
        let status = match n["result"].as_str() {
            Some("Passed" | "Expected Failure") => "passed",
            Some("Skipped") => "skipped",
            _ => "failed",
        };
        let failure = kids.find(|c| c["nodeType"] == "Failure Message").map(|f| {
            // ponytail: the first error record naming the case (`-[SumTests.SumXCTests testFails]`, `Test failsHere()
            // recorded an issue`); a display-named test or a shared name can miss or hit another's.
            let needle = name.trim_end_matches("()");
            let at = recs.iter().find(|(_, r)| r.level == Level::Error && r.line.contains(needle)).map(|(c, _)| c);
            let loc = &f["sourceLocation"];
            json!({"message": f["name"], "file": loc["filePath"], "line": loc["lineNumber"], "cursor": at})
        });
        out.push(json!({
            "id": format!("{bundle}/{}", n["nodeIdentifier"].as_str().unwrap_or(name)),
            "name": name,
            "status": status,
            "duration_ms": n["durationInSeconds"].as_f64().map(|s| (s * 1000.0).round() as u64),
            "failure": failure,
        }));
    }
    let mut out = vec![];
    for n in v["testNodes"].as_array().into_iter().flatten() {
        walk(n, "", recs, &mut out);
    }
    out
}

/// A simulator kind: simctl identifiers and their display names.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SimKind {
    pub device: String,
    pub device_name: String,
    pub runtime: String,
    pub runtime_name: String,
}

/// A device a lease holds: a created, disposable one (simulator or emulator), or an existing physical one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Sim {
    pub udid: String,
    pub name: String,
    pub device: String,
    pub runtime: String,
    /// Slimmed by simslim, and verified so: null when stock (as anything recorded before slimming read), else the
    /// categories it left enabled (`[]`: none).
    #[serde(default)]
    pub slim: Option<Vec<String>>,
    /// The `Platform` it is on. Left out for iOS, so everything recorded before platforms, and every iOS device's
    /// JSON, reads and looks as it did.
    #[serde(default = "ios", skip_serializing_if = "is_ios")]
    pub platform: String,
}

fn ios() -> String {
    Ios.id().into()
}

fn is_ios(p: &String) -> bool {
    *p == Ios.id()
}

impl Sim {
    pub fn key(&self) -> String {
        device_key(&self.platform, &self.udid)
    }
}

/// How `devices.json` and the retire queue name created device `id`: bare on iOS (as before platforms), else
/// `<platform>:<id>`.
// ponytail: the prefix only matters once a second platform exists; templates stay iOS-only (no other clones).
pub fn device_key(platform: &str, id: &str) -> String {
    match platform == Ios.id() {
        true => id.into(),
        false => format!("{platform}:{id}"),
    }
}

/// With `developer_dir`, simctl of that Xcode; else of the daemon's.
fn simctl(args: &[&str], developer_dir: Option<&str>) -> Result<Vec<u8>> {
    let mut cmd = Command::new("xcrun");
    if let Some(dev) = developer_dir {
        cmd.env("DEVELOPER_DIR", dev);
    }
    let out = cmd.arg("simctl").args(args).output()?;
    ensure!(out.status.success(), "simctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

fn version(rt: &Value) -> Vec<u32> {
    rt["version"].as_str().unwrap_or_default().split('.').filter_map(|n| n.parse().ok()).collect()
}

/// Available iOS runtimes from `simctl list runtimes --json` output, newest first (simctl lists oldest first).
fn ios_runtimes(list: &Value) -> Vec<Value> {
    let mut rts: Vec<Value> = list["runtimes"].as_array().into_iter().flatten().filter(|r| r["platform"] == "iOS" && r["isAvailable"] == true).cloned().collect();
    rts.sort_by_key(|r| std::cmp::Reverse(version(r)));
    rts
}

/// Installed iOS runtimes, newest first, in raw simctl form. Code `no_xcode` when simctl fails.
pub fn sim_runtimes(developer_dir: Option<&str>) -> Result<Vec<Value>> {
    let out = simctl(&["list", "runtimes", "available", "--json"], developer_dir);
    let out = out.map_err(|e| crate::daemon::coded("no_xcode", format!("{e:#}")))?;
    Ok(ios_runtimes(&serde_json::from_slice(&out)?))
}

/// The caller's `DEVELOPER_DIR` for the simctl calls a run of a target in `dir` makes, unless `.xcode-version` (nearest
/// up from `dir`) pins the Xcode; pool tooling then stays on the daemon's.
pub fn caller_developer_dir(dir: &Path, env: &HashMap<String, String>) -> Option<String> {
    match dir.ancestors().any(|d| d.join(".xcode-version").is_file()) {
        true => None,
        false => env.get("DEVELOPER_DIR").cloned(),
    }
}

/// The `sims` response shape of `sim_runtimes()`.
pub fn sims_json(rts: &[Value]) -> Value {
    let rts: Vec<Value> = rts
        .iter()
        .map(|r| {
            let devices: Vec<Value> = r["supportedDeviceTypes"].as_array().into_iter().flatten().map(|d| json!({"id": d["identifier"], "name": d["name"]})).collect();
            json!({"id": r["identifier"], "name": r["name"], "version": r["version"], "devices": devices})
        })
        .collect();
    json!({"runtimes": rts})
}

/// Resolves `device` (id or name) and `runtime` (id, name or version); a missing half comes from `base`
/// (`{"device": id, "runtime": id}` or null), else the default: the newest runtime and its first iPhone.
pub fn resolve(rts: &[Value], base: &Value, device: Option<&str>, runtime: Option<&str>) -> Result<SimKind> {
    use crate::daemon::coded;
    let eq = |v: &Value, q: &str| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(q));
    let rt = match runtime.or(base["runtime"].as_str()) {
        Some(q) => rts.iter().find(|r| eq(&r["identifier"], q) || eq(&r["name"], q) || eq(&r["version"], q)).ok_or_else(|| {
            let have: Vec<_> = rts.iter().filter_map(|r| r["name"].as_str()).collect();
            coded("not_installed", format!("iOS runtime {q} is not installed; installed: {}", have.join(", ")))
        })?,
        None => rts.first().ok_or_else(|| coded("not_installed", "no iOS runtimes installed"))?,
    };
    let rt_name = rt["name"].as_str().unwrap_or_default();
    let types = rt["supportedDeviceTypes"].as_array().map(Vec::as_slice).unwrap_or_default();
    let dt = match device.or(base["device"].as_str()) {
        Some(q) => types.iter().find(|d| eq(&d["identifier"], q) || eq(&d["name"], q)),
        None => types.iter().find(|d| d["productFamily"] == "iPhone"),
    };
    let dt = dt.ok_or_else(|| coded("bad_request", format!("{} isn't on {rt_name}", device.or(base["device"].as_str()).unwrap_or("an iPhone"))))?;
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    Ok(SimKind { device: s(&dt["identifier"]), device_name: s(&dt["name"]), runtime: s(&rt["identifier"]), runtime_name: rt_name.into() })
}

/// Creates a shut-down device named `name`.
pub fn create_sim(name: &str, kind: &SimKind, developer_dir: Option<&str>) -> Result<Sim> {
    let udid = String::from_utf8(simctl(&["create", name, &kind.device, &kind.runtime], developer_dir)?)?.trim().to_string();
    Ok(Sim { udid, name: name.into(), device: kind.device_name.clone(), runtime: kind.runtime_name.clone(), slim: None, platform: ios() })
}

/// Shuts `src` down (cloning needs it shut down; a no-op when it already is) and clones it, data and all, into a
/// shut-down device named `name`.
pub fn clone_sim(src: &Sim, name: &str, developer_dir: Option<&str>) -> Result<Sim> {
    let _ = simctl(&["shutdown", &src.udid], developer_dir); // fails when already shut down
    let udid = String::from_utf8(simctl(&["clone", &src.udid, name], developer_dir)?)?.trim().to_string();
    // A clone of a slimmed device comes out stock (simslim 0.13, measured): it is slimmed on its own if wanted.
    Ok(Sim { udid, name: name.into(), device: src.device.clone(), runtime: src.runtime.clone(), slim: None, platform: ios() })
}

/// Whether a runtime (`iOS 27.0`) keeps simslim's disables across a reboot, as iOS 18.5 and later do; earlier ones
/// are slimmed for the current boot only.
pub fn slim_persists(runtime_name: &str) -> bool {
    let v: Vec<u32> = runtime_name.rsplit(' ').next().unwrap_or_default().split('.').filter_map(|n| n.parse().ok()).collect();
    v >= vec![18, 5]
}

/// `simslim on`'s args to slim `udid`, leaving `except` categories enabled: persistent (the device shut down: simslim
/// boots it once, slim, and leaves it booted, so the launch boots nothing more) or for this boot only (`--no-reboot`,
/// the device booted).
pub fn slim_args(udid: &str, except: &[String], persistent: bool) -> Vec<String> {
    let mut a = vec!["on".to_string(), udid.into()];
    if !persistent {
        a.push("--no-reboot".into());
    }
    if !except.is_empty() {
        a.extend(["--except".into(), except.join(",")]);
    }
    a
}

/// `simslim verify`'s args: whether booted `udid` is slim but for `except`, as JSON.
pub fn verify_args(udid: &str, except: &[String]) -> Vec<String> {
    let mut a = vec!["verify".to_string(), udid.into()];
    if !except.is_empty() {
        a.extend(["--except".into(), except.join(",")]);
    }
    a.push("--json".into());
    a
}

/// `simslim <args>`'s stdout. Code `simslim_missing` when it isn't installed, `slim_failed` with its message when it
/// fails.
fn simslim(args: &[String], developer_dir: Option<&str>) -> Result<Vec<u8>> {
    use crate::daemon::coded;
    let mut cmd = Command::new("simslim");
    cmd.args(args);
    if let Some(dev) = developer_dir {
        cmd.env("DEVELOPER_DIR", dev);
    }
    let out = match cmd.output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(coded("simslim_missing", "slimming needs simslim: brew install mobai-app/tap/simslim")),
        r => r?,
    };
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout);
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(coded("slim_failed", format!("simslim {}: {}", args[0], tail.join(" / "))));
    }
    Ok(out.stdout)
}

/// The category ids simslim knows (`simslim profiles --json`), for checking an except list.
pub fn slim_categories() -> Result<Vec<String>> {
    let v: Value = serde_json::from_slice(&simslim(&["profiles".into(), "--json".into()], None)?)?;
    Ok(v.as_array().into_iter().flatten().filter_map(|c| c["id"].as_str().map(String::from)).collect())
}

/// Slims shut-down device `sim` but for `except`, leaving it booted, then verifies it: `sim.slim` is set only once
/// verify passes, so a device never reads slim when it isn't.
pub fn slim_sim(sim: &mut Sim, except: &[String], developer_dir: Option<&str>) -> Result<()> {
    let persistent = slim_persists(&sim.runtime);
    if !persistent {
        simctl(&["bootstatus", &sim.udid, "-b"], developer_dir)?;
    }
    simslim(&slim_args(&sim.udid, except, persistent), developer_dir)?;
    let v: Value = serde_json::from_slice(&simslim(&verify_args(&sim.udid, except), developer_dir)?)?;
    if v["ok"] != true {
        let missing = v["missing"].as_array().map_or(0, Vec::len);
        bail!(crate::daemon::coded("slim_failed", format!("simslim verify: {missing} daemons still run on {}", sim.udid)));
    }
    sim.slim = Some(except.to_vec());
    Ok(())
}

pub fn rename_sim(udid: &str, name: &str, developer_dir: Option<&str>) -> Result<()> {
    simctl(&["rename", udid, name], developer_dir).map(|_| ())
}

pub fn list_devices() -> Result<Value> {
    Ok(serde_json::from_slice(&simctl(&["list", "devices", "--json"], None)?)?)
}

/// UDIDs of the devices in `simctl list devices --json` output named exactly one of `names`.
pub fn named(list: &Value, names: &[String]) -> Vec<String> {
    let devices = list["devices"].as_object().into_iter().flat_map(|m| m.values()).flat_map(|v| v.as_array().into_iter().flatten());
    devices.filter(|d| names.iter().any(|n| d["name"] == n.as_str())).filter_map(|d| d["udid"].as_str().map(Into::into)).collect()
}

/// Shuts the device down and deletes it; Ok when it is gone, including when it already was.
pub fn retire_sim(udid: &str) -> Result<()> {
    let _ = simctl(&["shutdown", udid], None); // fails when already shut down
    match simctl(&["delete", udid], None) {
        Err(e) if !format!("{e:#}").contains("Invalid device") => {
            eprintln!("warn: {e:#}");
            Err(e)
        }
        _ => Ok(()),
    }
}

/// A physical iOS device as `devicectl list devices` reports it; serialized as `sims`' `physical` entries.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Physical {
    pub udid: String,
    /// The user's name for it, e.g. "Test iPhone".
    pub name: String,
    pub model: String,
    /// e.g. "iOS 27.0".
    pub os: String,
    /// devicectl's `connection.state`: `connected`, `disconnected` or `unavailable`.
    pub state: String,
    pub developer_mode: bool,
    #[serde(skip)]
    pub paired: bool,
}

impl Physical {
    /// The device as a lease and a process carry it.
    pub fn sim(&self) -> Sim {
        Sim { udid: self.udid.clone(), name: self.name.clone(), device: self.model.clone(), runtime: self.os.clone(), slim: None, platform: ios() }
    }
}

/// The physical iOS devices in `devicectl list devices -j -` output, which lists simulators too.
fn physical(list: &Value) -> Vec<Physical> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let devices = list["result"]["devices"].as_array().into_iter().flatten().map(|d| &d["properties"]);
    devices
        .filter(|p| p["hardware"]["reality"] == "physical" && p["hardware"]["platform"] == "iOS")
        .map(|p| Physical {
            udid: s(&p["hardware"]["udid"]),
            name: s(&p["state"]["name"]),
            model: s(&p["hardware"]["marketingName"]),
            os: format!("iOS {}", s(&p["software"]["osVersionNumber"]["stringValue"])),
            state: s(&p["connection"]["state"]),
            // An enum: `{"enabled": {"mode": 1}}`, `disabled`, `notSupported` (before iOS 16) or `unknown`; only
            // `disabled` is known to refuse a launch.
            developer_mode: p["state"]["developerModeStatus"].get("disabled").is_none(),
            paired: p["connection"]["pairingState"] == "paired",
        })
        .collect()
}

/// Physical iOS devices, paired or not. Code `no_xcode` when devicectl fails.
pub fn physical_devices() -> Result<Vec<Physical>> {
    let out = Command::new("xcrun").args(["devicectl", "list", "devices", "-j", "-"]).output();
    let out = out.map_err(|e| crate::daemon::coded("no_xcode", format!("devicectl: {e}")))?;
    if !out.status.success() {
        bail!(crate::daemon::coded("no_xcode", format!("devicectl list devices: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(physical(&serde_json::from_slice(&out.stdout)?))
}

/// The listed, paired device `q` (UDID, else name; any case). With `ready` (a run is about to use it) it must also be
/// reachable and in Developer Mode, and one that isn't listed is `unavailable` rather than `not_found`.
pub fn find_physical<'a>(list: &'a [Physical], q: &str, ready: bool) -> Result<&'a Physical> {
    use crate::daemon::coded;
    // Default device names use a typographic apostrophe ("Dev’s iPhone"), which nobody types.
    let plain = |s: &str| s.replace('\u{2019}', "'");
    let d = list.iter().find(|d| d.udid.eq_ignore_ascii_case(q)).or_else(|| list.iter().find(|d| plain(&d.name).eq_ignore_ascii_case(&plain(q))));
    let d = d.ok_or_else(|| coded(if ready { "unavailable" } else { "not_found" }, format!("physical device {q} isn't listed; see `collie sims`")))?;
    let fail = |why: &str| Err(coded("unavailable", format!("{} ({}) {why}", d.name, d.udid)));
    if !d.paired {
        return fail("isn't paired with this Mac; pair it in Xcode's Devices window");
    }
    if ready && d.state == "unavailable" {
        return fail("is unavailable: unlock it and bring it onto this Mac's network, or connect it by cable");
    }
    if ready && !d.developer_mode {
        return fail("has Developer Mode off (Settings › Privacy & Security › Developer Mode)");
    }
    Ok(d)
}

/// Where an ecosystem's devices come from: created, disposable ones (simulators or emulators) a lease holds and
/// retires, and existing physical ones. A trait with one impl (`Ios`) so a second ecosystem can bring its own.
/// `dev` is the ecosystem's pinned toolchain (Xcode's `DEVELOPER_DIR`), opaque to the daemon. What a platform can't do
/// is code `bad_request` naming it.
pub trait Platform: Sync {
    fn id(&self) -> &'static str;
    /// Installed runtimes with the device types each runs, newest first, as `resolve` reads them.
    fn runtimes(&self, dev: Option<&str>) -> Result<Vec<Value>>;
    /// The device kind `device` and `runtime` name, the rest from `base` (the stored choice), else the default.
    fn resolve(&self, rts: &[Value], base: &Value, device: Option<&str>, runtime: Option<&str>) -> Result<SimKind>;
    /// A new, shut-down device named `name`.
    fn create(&self, name: &str, kind: &SimKind, dev: Option<&str>) -> Result<Sim>;
    /// Shuts a created device down and deletes it; Ok when it is gone, including when it already was.
    fn retire(&self, id: &str) -> Result<()>;
    /// The ids of created devices named one of `names`: those a daemon killed mid-create left behind.
    fn named(&self, names: &[String]) -> Result<Vec<String>>;
    /// A shut-down copy of `src`, data and all, named `name` (templates).
    fn clone_device(&self, _src: &Sim, _name: &str, _dev: Option<&str>) -> Result<Sim> {
        Err(unsupported(self.id(), "cloning a device (templates)"))
    }
    fn rename(&self, _id: &str, _name: &str, _dev: Option<&str>) -> Result<()> {
        Err(unsupported(self.id(), "renaming a device"))
    }
    /// Slims shut-down `sim` but for `except` categories, leaving it booted; sets `sim.slim` once verified.
    fn slim(&self, _sim: &mut Sim, _except: &[String], _dev: Option<&str>) -> Result<()> {
        Err(unsupported(self.id(), "slimming"))
    }
    /// The category ids `slim` takes.
    fn slim_categories(&self) -> Result<Vec<String>> {
        Err(unsupported(self.id(), "slimming"))
    }
    /// Whether a slim device on `runtime` stays slim across a reboot, so a re-run may keep it.
    fn slim_persists(&self, _runtime: &str) -> bool {
        false
    }
    fn physical_devices(&self) -> Result<Vec<Physical>> {
        Err(unsupported(self.id(), "physical devices"))
    }
    /// Listed device `q` (id, else name); with `ready`, also usable by a run now.
    fn find_physical<'a>(&self, _list: &'a [Physical], _q: &str, _ready: bool) -> Result<&'a Physical> {
        Err(unsupported(self.id(), "physical devices"))
    }
}

fn unsupported(platform: &str, what: &str) -> anyhow::Error {
    crate::daemon::coded("bad_request", format!("{platform} devices don't support {what}"))
}

/// Simulators through simctl (and simslim), physical devices through devicectl.
pub struct Ios;

impl Platform for Ios {
    fn id(&self) -> &'static str {
        "ios"
    }
    fn runtimes(&self, dev: Option<&str>) -> Result<Vec<Value>> {
        sim_runtimes(dev)
    }
    fn resolve(&self, rts: &[Value], base: &Value, device: Option<&str>, runtime: Option<&str>) -> Result<SimKind> {
        resolve(rts, base, device, runtime)
    }
    fn create(&self, name: &str, kind: &SimKind, dev: Option<&str>) -> Result<Sim> {
        create_sim(name, kind, dev)
    }
    fn retire(&self, id: &str) -> Result<()> {
        retire_sim(id)
    }
    fn named(&self, names: &[String]) -> Result<Vec<String>> {
        Ok(named(&list_devices()?, names))
    }
    fn clone_device(&self, src: &Sim, name: &str, dev: Option<&str>) -> Result<Sim> {
        clone_sim(src, name, dev)
    }
    fn rename(&self, id: &str, name: &str, dev: Option<&str>) -> Result<()> {
        rename_sim(id, name, dev)
    }
    fn slim(&self, sim: &mut Sim, except: &[String], dev: Option<&str>) -> Result<()> {
        slim_sim(sim, except, dev)
    }
    fn slim_categories(&self) -> Result<Vec<String>> {
        slim_categories()
    }
    fn slim_persists(&self, runtime: &str) -> bool {
        slim_persists(runtime)
    }
    fn physical_devices(&self) -> Result<Vec<Physical>> {
        physical_devices()
    }
    fn find_physical<'a>(&self, list: &'a [Physical], q: &str, ready: bool) -> Result<&'a Physical> {
        find_physical(list, q, ready)
    }
}

pub const PLATFORMS: [&dyn Platform; 1] = [&Ios];

pub fn platform(id: &str) -> Option<&'static dyn Platform> {
    PLATFORMS.iter().find(|p| p.id() == id).copied()
}

/// Retires the created device `devices.json` names `key` (`Sim::key`) on its platform among `platforms`.
pub fn retire_device(platforms: &[&dyn Platform], key: &str) -> Result<()> {
    let (p, id) = key.split_once(':').unwrap_or((Ios.id(), key));
    let p = platforms.iter().find(|x| x.id() == p).with_context(|| format!("{key}: no platform {p}"))?;
    p.retire(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn cargo_detects_bins_and_examples() {
        let mut names: Vec<_> = detect_all(&fixture("cargo-ws")).unwrap().into_iter().map(|t| (t.name, t.kind)).collect();
        names.sort();
        let want = [("alpha", "bin"), ("beta", "bin"), ("cargo test", "test"), ("demo", "example"), ("lint", "task")];
        assert_eq!(names, want.map(|(n, k)| (n.into(), k.into())));
    }

    /// Each output line of `cmd` with the level `classify` gives it, and the exit code.
    fn classified(e: &dyn Ecosystem, mut cmd: Command) -> (Vec<(Level, String)>, Option<i32>) {
        let out = cmd.env("CARGO_TARGET_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/target/fixtures")).output().unwrap();
        let lines = |b: &[u8], src| String::from_utf8_lossy(b).lines().map(|l| e.classify(src, l)).collect::<Vec<_>>();
        let mut all = lines(&out.stdout, Source::Stdout);
        all.extend(lines(&out.stderr, Source::Stderr));
        (all, out.status.code())
    }

    #[test]
    fn cargo_test_reports_failures_as_errors_and_no_replayed_warnings() {
        let t = detect_all(&fixture("cargo-tests")).unwrap().into_iter().find(|t| t.kind == "test").unwrap();
        assert_eq!((t.name.as_str(), t.dir.clone()), ("cargo test", fixture("cargo-tests")));
        let build = Cargo.build(&t, &Ctx::default()).unwrap();
        assert_eq!(build.get_args().collect::<Vec<_>>(), ["test", "--no-run", "--message-format=json"]);
        assert_eq!(classified(&Cargo, build).1, Some(0));
        let args: Vec<String> = ["-p", "cargo-tests", "passes", "--", "--nocapture"].map(String::from).into();
        let narrowed = Cargo.build(&t, &Ctx { args: args.clone(), ..Default::default() }).unwrap();
        assert_eq!(narrowed.get_args().skip(3).map(|a| a.to_string_lossy()).collect::<Vec<_>>(), args, "the run's args narrow the build too");
        assert_eq!(classified(&Cargo, narrowed).1, Some(0), "cargo takes a filter and `-- …` with --no-run");
        let doc = Cargo.build(&t, &Ctx { args: vec!["--doc".into()], ..Default::default() }).unwrap();
        assert_eq!(classified(&Cargo, doc).1, Some(0), "--doc is the run's alone: cargo refuses it with --no-run");
        let beta = detect_all(&fixture("cargo-ws")).unwrap().into_iter().find(|t| t.name == "beta").unwrap();
        assert!(!Cargo.build(&beta, &Ctx { args, ..Default::default() }).unwrap().get_args().any(|a| a == "passes"), "other kinds: run only");
        let (recs, code) = classified(&Cargo, Cargo.run(&t, &Ctx::default()));
        assert_eq!(code, Some(101));
        let errors: Vec<&str> = recs.iter().filter(|(l, _)| *l == Level::Error).map(|(_, s)| s.as_str()).collect();
        for want in ["test tests::fails ... FAILED", "panicked at src/lib.rs:15:9:", "test result: FAILED. 1 passed; 1 failed; 1 ignored", "`--lib`"] {
            assert!(errors.iter().any(|e| e.contains(want)), "{want} not in {errors:#?}");
        }
        // A lock wait is a real warning: parallel tests' cargo calls share target/fixtures and the package cache.
        assert!(!recs.iter().any(|(l, s)| *l == Level::Warn && !waits_for_lock(s)), "the replayed unused-variable warning, cargo's status lines: {recs:#?}");
        assert!(recs.iter().any(|(l, s)| *l == Level::Debug && s.contains("unused variable")), "replayed as debug");
        let rerun = |level: &str| format!(r#"{{"reason":"compiler-message","message":{{"level":"{level}","rendered":"{level}: x"}}}}"#);
        assert_eq!(Cargo.classify(Source::Stdout, &rerun("error")).0, Level::Error, "a recompile in the run phase failing");
        assert_eq!(Cargo.classify(Source::Stdout, &rerun("warning")).0, Level::Debug);
        assert_eq!(Cargo.classify(Source::Stderr, "       Error bind failed").0, Level::Error, "a bin's own line in cargo's layout");
        assert_eq!(Cargo.classify(Source::Stderr, "    Blocking waiting for file lock on build directory").0, Level::Warn);
        let mut filtered = Cargo.run(&t, &Ctx::default());
        filtered.arg("passes");
        assert_eq!(classified(&Cargo, filtered).1, Some(0), "a filter narrows the run");
    }

    #[test]
    fn cargo_results_come_from_the_runs_stdout() {
        let t = detect_all(&fixture("cargo-tests")).unwrap().into_iter().find(|t| t.kind == "test").unwrap();
        let out = Cargo.run(&t, &Ctx::default()).env("CARGO_TARGET_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/target/fixtures")).output().unwrap();
        let recs: Vec<(String, Record)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .enumerate()
            .map(|(i, l)| (format!("p@{i}"), Record { ts: 0, source: Source::Stdout, level: Cargo.classify(Source::Stdout, l).0, line: l.into() }))
            .collect();
        let v = Cargo.results(&t, &Ctx::default(), &recs).unwrap();
        assert_eq!(v["sep"], "::");
        let tests = v["tests"].as_array().unwrap();
        let by = |id: &str| tests.iter().find(|t| t["id"] == id).unwrap_or_else(|| panic!("{id} in {tests:#?}"));
        assert_eq!((&by("tests::passes")["status"], &by("tests::ignored")["status"]), (&"passed".into(), &"skipped".into()));
        let fails = by("tests::fails");
        assert_eq!((&fails["status"], &fails["name"], &fails["duration_ms"]), (&"failed".into(), &"fails".into(), &Value::Null));
        let f = &fails["failure"];
        assert_eq!((&f["file"], &f["line"], &f["message"]), (&"src/lib.rs".into(), &15.into(), &"assertion `left == right` failed".into()));
        let panic = recs.iter().find(|(c, _)| f["cursor"] == c.as_str()).map(|(_, r)| r.line.as_str());
        assert!(panic.is_some_and(|l| l.contains("panicked at src/lib.rs:15:9:")), "the cursor is the panic's record: {panic:?}");
        assert_eq!(tests.len(), 3);
    }

    #[test]
    fn xcresult_tests_take_ids_from_the_bundle_and_node_identifier() {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(fixture("xcresult-tests.json")).unwrap()).unwrap();
        let rec = |level, line: &str| Record { ts: 0, source: Source::Stdout, level, line: line.into() };
        let recs = [
            ("p@0".to_string(), rec(Level::Info, "Test Case '-[SumTests.SumXCTests testFails]' started.")),
            ("p@1".to_string(), rec(Level::Error, "/x/T.swift:6: error: -[SumTests.SumXCTests testFails] : XCTAssertEqual failed")),
        ];
        let tests = xcresult_tests(&v, &recs);
        let ids: Vec<&str> = tests.iter().map(|t| t["id"].as_str().unwrap()).collect();
        for want in ["SumTests/Outer/adds()", "SumTests/Outer/Inner/nested()", "SumTests/SumXCTests/testFails()", "SumTests/topLevel()"] {
            assert!(ids.contains(&want), "{want} in {ids:?}");
        }
        let by = |id: &str| tests.iter().find(|t| t["id"] == id).unwrap();
        assert_eq!(by("SumTests/Outer/adds()")["name"], "Adds with a display name");
        let fails = by("SumTests/SumXCTests/testFails()");
        assert_eq!((&fails["status"], &fails["failure"]["line"], &fails["failure"]["cursor"]), (&"failed".into(), &6.into(), &"p@1".into()));
        assert!(fails["failure"]["file"].as_str().unwrap().ends_with("T.swift") && fails["duration_ms"] == 101);
        assert_eq!(by("SumTests/Outer/failsHere()")["failure"]["cursor"], Value::Null, "no error record names it");
        assert_eq!(tests.iter().filter(|t| t["status"] == "passed").count(), 4);
    }

    #[test]
    fn node_test_scripts_are_tests_and_not_ok_is_an_error() {
        let ts = Node.detect(&fixture("node-app")).unwrap();
        let t = ts.iter().find(|t| t.name == "test").unwrap();
        assert_eq!(t.kind, "test");
        assert!(ts.iter().filter(|t| t.name != "test").all(|t| t.kind == "script"));
        let (recs, code) = classified(&Node, Node.run(t, &Ctx::default()));
        assert_eq!(code, Some(1));
        assert!(recs.contains(&(Level::Error, "not ok 2 - fails".into())), "{recs:#?}");
    }

    #[test]
    fn tasks_pass_run_args_to_the_line_and_exec_runs_them_whole() {
        let lint = detect_all(&fixture("cargo-ws/alpha")).unwrap().into_iter().find(|t| t.name == "lint").unwrap();
        assert_eq!((lint.ecosystem.as_str(), lint.dir.clone()), ("task", fixture("cargo-ws")));
        let out = |t: &Target, args: &[&str]| String::from_utf8(Task.run(t, &Ctx::default()).args(args).output().unwrap().stdout).unwrap();
        assert_eq!(out(&lint, &["x y", "$HOME"]), "a#b|c=d|x y|$HOME|");
        let exec = Target { kind: "exec".into(), name: "printf %s".into(), ..lint.clone() };
        assert_eq!(out(&exec, &["printf", "%s-", "a b", ";"]), "a b-;-");
        let gone = Target { name: "gone".into(), ..lint };
        let err = Task.run(&gone, &Ctx::default()).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&err.stderr), "collie: error: no task gone in collie.toml\n");
    }

    #[test]
    fn a_broken_collie_toml_fails_detection_with_bad_config() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/bad-config");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts": {"dev": "node x.js"}}"#).unwrap();
        let err = |toml: &str| {
            std::fs::write(dir.join("collie.toml"), toml).unwrap();
            let e = detect_all(&dir).unwrap_err();
            (e.downcast_ref::<crate::daemon::Coded>().unwrap().0.clone(), e.to_string())
        };
        let (code, msg) = err("[tasks.lint]\ncmd = 'unterminated\n");
        assert_eq!(code, "bad_config", "even though node still has targets");
        assert!(msg.starts_with(&format!("{}: ", dir.join("collie.toml").display())), "{msg}");
        let (code, msg) = err("[targets.dev]\nport = \"3000\"\n");
        assert_eq!(code, "bad_config", "a mistyped override isn't skipped");
        assert!(msg.contains("port"), "{msg}");
        std::fs::write(dir.join("collie.toml"), "[targets.dev]\nport = 3000\n").unwrap();
        let dev = detect_all(&dir).unwrap().into_iter().find(|t| t.name == "dev").unwrap();
        assert_eq!(dev.port, Some(3000));
        let (code, msg) = err("[tasks.dev]\ncmd = \"true\"\n");
        assert_eq!(code, "bad_config");
        assert!(msg.ends_with("collie.toml: task \"dev\" clashes with node target \"dev\""), "{msg}");
    }

    #[test]
    fn xcodebuild_test_failures_are_errors_and_everything_else_info() {
        let levels = |name: &str| -> Vec<(Level, String)> {
            let text = std::fs::read_to_string(fixture(name)).unwrap();
            text.lines().map(|l| Xcode.classify(Source::Stdout, l)).collect()
        };
        // An app's test target on an iOS 26.5 simulator, all passing (trimmed to the test runner's lines; names neutral).
        let pass = levels("xcodebuild-test-pass.txt");
        assert!(pass.iter().all(|(l, _)| *l == Level::Info), "{:#?}", pass.iter().filter(|(l, _)| *l != Level::Info).collect::<Vec<_>>());
        // One failing XCTest and one failing Swift Testing test on an iOS simulator.
        let fail = levels("xcodebuild-test-fail.txt");
        let errors: Vec<&str> = fail.iter().filter(|(l, _)| *l == Level::Error).map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            errors,
            [
                "/tmp/xfail/Tests/SumTests/SumTests.swift:10: error: -[SumTests.SumXCTests testFails] :",
                "Test Case '-[SumTests.SumXCTests testFails]' failed (1.194 seconds).",
                "✘ Test failsSwiftTesting() recorded an issue at SumTests.swift:6:34: Expectation failed: Sum.add(1, 2) == 4",
                "✘ Test failsSwiftTesting() failed after 0.017 seconds with 1 issue.",
                "✘ Test run with 2 tests in 0 suites failed after 0.020 seconds with 1 issue.",
            ]
        );
        assert!(fail.iter().all(|(l, _)| matches!(l, Level::Error | Level::Info)));
        let outcomes = |name: &str| {
            let text = std::fs::read_to_string(fixture(name)).unwrap();
            let all: Vec<_> = text.lines().filter_map(|l| Xcode.outcome(l)).collect();
            ["passed", "failed", "skipped"].map(|o| all.iter().filter(|a| **a == o).count())
        };
        assert_eq!(outcomes("xcodebuild-test-fail.txt"), [2, 2, 0], "an XCTest and a Swift Testing test each; not `recorded an issue`");
        assert_eq!(outcomes("xcodebuild-test-pass.txt"), [33, 0, 0], "not the suite lines or `Test run with 33 tests`");
        assert_eq!(Cargo.outcome("test tests::ignored ... ignored, slow"), Some("skipped"));
        assert_eq!(Cargo.outcome("test result: ok. 1 passed; 0 failed"), None);
        assert_eq!(Xcode.classify(Source::Stderr, "** TEST EXECUTE FAILED **").0, Level::Error, "printed on stderr");
        assert_eq!(heuristic(Source::Stdout, "✔ Test \"A decoding error maps to the empty feed\" passed after 0.001 seconds."), Level::Info);
        assert_eq!(heuristic(Source::Stdout, "􁁛 Test addsSwiftTesting() passed after 0.001 seconds."), Level::Info, "macOS");
        // XCTest's format, with "error" in passing names (made up; the captures had none).
        for l in [
            "Test Case '-[FeedTests testDecodingErrorMapsToEmpty]' started.",
            "Test Case '-[FeedTests testDecodingErrorMapsToEmpty]' passed (0.001 seconds).",
            "Test Suite 'ErrorHandlingTests' started at 2026-10-28 22:29:57.792.",
            "Test Suite 'ErrorHandlingTests' passed at 2026-10-28 22:29:58.995.",
        ] {
            assert_eq!(heuristic(Source::Stdout, l), Level::Info, "{l}");
        }
        assert_eq!(heuristic(Source::Stdout, "Test Case '-[FeedTests testDecodingErrorMapsToEmpty]' failed (0.002 seconds)."), Level::Error);
    }

    #[test]
    fn xcode_rules_keep_names_that_mention_errors_info() {
        // Lines from a day of two apps' builds.
        let infos = [
            "[IssueReporting] Compiling ErrorReporting.swift",
            "[WidgetsUI] Compiling ErrorView.swift",
            r"    export __DIAGNOSE_INVALID_DEPLOYMENT_TARGET_AS_ERROR\=YES",
            "    ✔ \"parse returns unknown route error for unsupported path\" (0.001 seconds)",
            "LLVM Profile Error: Failed to write file \"default.profraw\": Operation not permitted",
            "   ▸ If the error is actionable, address it",
        ];
        for l in infos {
            assert_eq!(xcode_level(l), Level::Info, "{l}");
        }
        assert_eq!(Xcode.classify(Source::Build, "Linting 'ErrorView.swift' (3/4)").0, Level::Info);
        assert_eq!(XcodeTool.classify(Source::Stderr, "Linting 'ErrorView.swift' (3/4)").0, Level::Info, "a tuist exec's stderr too");
        let warns = [
            "[!]  /x/SpeechService.swift:132:20: Todo Violation: TODOs should be resolved (Propagate error in…)",
            "/x/ProfileClient.swift:64:30: warning: Simplify error composition…",
            "error: the following command failed with exit code 0 but produced no further output",
        ];
        for l in warns {
            assert_eq!(xcode_level(l), Level::Warn, "{l}");
        }
        let errors = [
            "[x] xcodebuild: error: The workspace named \"Demo\" does not contain a scheme named \"Demo-Dev\".",
            "  xcodebuild: error: …",
            "✖ Error ",
            "  ** TEST FAILED ** ",
            "** BUILD FAILED **",
            "error: Build input files cannot be found: '…'",
            "/tmp/xfail/Tests/SumTests/SumTests.swift:10: error: -[SumTests.SumXCTests testFails] :",
            "✘ Test failsSwiftTesting() recorded an issue at SumTests.swift:6:34: Expectation failed",
            "Test Case '-[SumTests.SumXCTests testFails]' failed (1.194 seconds).",
            "MyApp/File.swift:12: Fatal error: Unexpectedly found nil",
        ];
        for l in errors {
            assert_eq!(xcode_level(l), Level::Error, "{l}");
        }
    }

    #[test]
    fn the_heuristic_reads_error_words_not_substrings() {
        let not = [
            (Source::Stderr, "2026-10-29 15:58:16.192767+0100 Demo_Dev[41709:12033784] [connection] nw_endpoint_flow_failed_with_error [C5.1.1.1 …]"),
            (Source::Stdout, "/x/UI/Scenes/Error/ErrorView.swift"),
        ];
        for (src, l) in not {
            assert_ne!(heuristic(src, l), Level::Error, "{l}");
        }
        for l in ["thread 'tests::fails' panicked at src/lib.rs:15:9:", "error[E0425]: cannot find value", "TypeError: x is undefined", "Underlying error: unexpected end of file", "Error: boom"] {
            assert_eq!(heuristic(Source::Stdout, l), Level::Error, "{l}");
        }
        assert_eq!(heuristic(Source::Stdout, "[!] careful"), Level::Warn);
    }

    #[test]
    fn simslim_persists_from_ios_18_5_and_gets_its_args() {
        assert!(slim_persists("iOS 27.0") && slim_persists("iOS 18.5") && slim_persists("iOS 26.5"));
        assert!(!slim_persists("iOS 18.0") && !slim_persists("iOS 17.5") && !slim_persists("iOS 18.3"));
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(slim_args("U", &[], true), ["on", "U"], "persistent: simslim boots it once, slim");
        assert_eq!(slim_args("U", &s(&["icloud", "web"]), false), ["on", "U", "--no-reboot", "--except", "icloud,web"]);
        assert_eq!(verify_args("U", &[]), ["verify", "U", "--json"]);
        assert_eq!(verify_args("U", &s(&["store"])), ["verify", "U", "--except", "store", "--json"]);
        // Recorded before slimming existed: stock.
        let old: Sim = serde_json::from_str(r#"{"udid": "U", "name": "collie 1", "device": "iPhone 17 Pro", "runtime": "iOS 27.0"}"#).unwrap();
        assert_eq!(old.slim, None);
        assert_eq!(serde_json::to_value(&old).unwrap()["slim"], Value::Null, "always in the JSON");
    }

    #[test]
    fn a_device_apps_mirrored_log_keeps_its_level_and_its_lines() {
        // Real stderr of an app's run on a phone (names neutral): entries with their `Time:`/`Details:`/… lines.
        let text = std::fs::read_to_string(fixture("device-stderr.txt")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let recs = crate::daemon::group(&lines, |h, l| XcodeDevice.continues(h, l));
        let shape: Vec<(Level, usize)> = recs.iter().map(|r| (XcodeDevice.classify(Source::Stderr, r.lines().next().unwrap()).0, r.lines().count())).collect();
        assert_eq!(shape, [(Level::Info, 1), (Level::Info, 4), (Level::Info, 6), (Level::Warn, 7)], "no level word reads as info: {recs:#?}");
        // Not seen from that app on a device yet (it logs debug only once this ships): a probe app's line, and the
        // app's own markers.
        let c = |l: &str| XcodeDevice.classify(Source::Stderr, l).0;
        let at = |msg: &str| c(&format!("2026-10-30 16:45:41.006930+0100 Demo_Dev[41709:12033783] {msg}"));
        assert_eq!(at("[system] DEBUG: probe"), Level::Debug);
        assert_eq!(c("2026-10-30 16:45:41.006930+0100 Demo Dev[1:2] [system] 🐛 DEBUG: x"), Level::Debug, "a process name with a space");
        assert_eq!(at("[system] ℹ️ INFO: x"), Level::Info);
        assert_eq!(at("[system] ⚠️ WARN: x"), Level::Warn);
        assert_eq!(at("[system] 🚨 ERROR: x"), Level::Error);
        assert_eq!(at("<Notice> fault-tolerant"), Level::Info, "the leading word wins over one further on");
        // From the device logs: a level word must lead; an error word anywhere still makes an error.
        assert_eq!(at("CoreText performance note: Set a breakpoint on CTFontLogSuboptimalRequest to debug."), Level::Info);
        assert_eq!(at("[[FirebaseAnalytics]] 12.19.2 - [FirebaseAnalytics][I-ACS023008] To enable debug logging set the following application argument: -FIRAnalyticsDebugEnabled"), Level::Info);
        assert_eq!(at("[[GULNetwork]] 12.19.2 - [GULNetwork][I-NET000000] <Google/Utilities/Network/ERROR> Encounter network error. Code, error: -1009"), Level::Error);
        assert_eq!(at("[connection] nw_endpoint_flow_failed_with_error [C3.1.1 ::.0 in_progress channel-flow"), Level::Info, "`failed_with_error` is no whole word");
        // A crash or abort line starts its own record, at error, and keeps the lines after it up to the next mirrored one.
        let shape = |ls: &[&str]| -> Vec<(Level, usize)> {
            let recs = crate::daemon::group(ls, |h, l| XcodeDevice.continues(h, l));
            recs.iter().map(|r| (XcodeDevice.classify(Source::Stderr, r.lines().next().unwrap()).0, r.lines().count())).collect()
        };
        assert_eq!(shape(&[lines[1], "X.swift:12: Fatal error: boom"]), [(Level::Info, 1), (Level::Error, 1)]);
        assert_eq!(shape(&[lines[1], "Precondition failed: bad state", "  at frame 1", lines[0]]), [(Level::Info, 1), (Level::Error, 2), (Level::Info, 1)]);
        assert!(crash_line("*** Terminating app due to uncaught exception 'NSInvalidArgumentException'") && crash_line("libc++abi: terminating due to uncaught exception"));
        assert!(!crash_line("Details: Error Domain=NSURLErrorDomain Code=-999") && !crash_line("  \"error\": .string("));
        // An app's own multi-line error message (an app on a phone) is one record, error words and all.
        let text = std::fs::read_to_string(fixture("device-error-dump.txt")).unwrap();
        let dump: Vec<&str> = text.lines().collect();
        assert_eq!(shape(&dump), [(Level::Error, dump.len() - 1), (Level::Debug, 1)]);
        // devicectl's own lines (stdout, before the app's) stay records of their own, as do the app's prints.
        let devicectl = ["App installed:", "• bundleID: com.example.demo.dev", "Launched application with com.example.demo.dev bundle identifier.", "Waiting for the application to terminate…"];
        assert_eq!(crate::daemon::group(&devicectl, |h, l| XcodeDevice.continues(h, l)).len(), 4);
        assert_eq!(c("Waiting for the application to terminate…"), Xcode.classify(Source::Stderr, "Waiting for the application to terminate…").0, "unprefixed: Xcode's rules");
        assert!(XcodeDevice.launched(Source::Stdout, devicectl[2]));
        // Xcode's own rules are untouched: a simulator run's ndjson, and xctest's prefixed NSLog lines in a test run.
        let ndjson = r#"{"eventMessage":"hello","messageType":"Debug","subsystem":"com.example.demo"}"#;
        assert_eq!(Xcode.classify(Source::Stdout, ndjson), (Level::Debug, "hello".into()));
        assert!(!Xcode.continues(lines[1], lines[2]), "only a device run joins unprefixed lines");
        let nslog = "2026-10-30 12:57:21.976292+0100 xctest[44560:63578734] [authentication] x";
        assert_eq!(mirrored(nslog).map(|at| &nslog[at..]), Some("[authentication] x"));
        assert_eq!(mirrored("2026-10-30 12:57:21 xctest[44560:63578734] x"), None);
        // A device launch asks for debug-level mirroring, over the caller's env.
        let t = Target { name: "A".into(), ecosystem: "xcode".into(), dir: "/w".into(), kind: "app".into(), package: None, needs_port: false, port: None, target_dir: None };
        let settings = json!({"TARGET_BUILD_DIR": "/b", "WRAPPER_NAME": "A.app", "PRODUCT_BUNDLE_IDENTIFIER": "a.b"});
        let mut xb = Command::new("xcodebuild");
        xb.env("DEVICECTL_CHILD_OS_ACTIVITY_MODE", "info");
        let env = |ctx: &Ctx| -> HashMap<String, String> {
            let cmd = Xcode::launch_script(&t, ctx, &settings, &xb);
            cmd.get_envs().filter_map(|(k, v)| Some((k.to_str()?.into(), v?.to_str()?.into()))).collect()
        };
        let phone = env(&Ctx { device: Some("P".into()), ..Default::default() });
        assert_eq!((phone["DEVICECTL_CHILD_OS_ACTIVITY_MODE"].as_str(), phone["DEVICECTL_CHILD_OS_ACTIVITY_DT_MODE"].as_str()), ("debug", "enable"));
        assert_eq!(env(&Ctx { simulator: Some("U".into()), ..Default::default() })["DEVICECTL_CHILD_OS_ACTIVITY_MODE"], "info", "a simulator's: untouched");
    }

    #[test]
    fn swiftc_excerpt_lines_continue_their_diagnostic() {
        // From two apps' xcodebuild logs.
        for yes in [
            " 85 |                 geometry.size.height * (activity.ignoresVerticalContainerSafeArea",
            "589 | ",
            "588 | }",
            "    |                                    `- warning: main actor-isolated property 'isOnInnerDisplay' can not be referenced",
            "    |         `- note: property declared here",
            "   |   |           `- warning: type 'Status' does not conform to the 'Sendable' protocol",
            "    +--- macro expansion @CasePathable ---------------------------------",
            "     +----------------------------------------------------------------",
            "   :",
            "`- /Users/dev/myapp/Projects/Settings/Sources/Settings.swift:29:29: note: expanded code originates here",
            "/Users/dev/demo/Packages/StepFlow/Module/Sources/Views/StepPagerView.swift:587:9: note: property declared here",
        ] {
            assert!(Xcode.continues("", yes), "{yes}");
            assert!(Task.continues("", yes) && XcodeTool.continues("", yes), "an exec's `swift build` too: {yes}");
        }
        for no in [
            "/Users/dev/demo/Packages/StepFlow/Module/Sources/Views/StepContainerView.swift:85:95: warning: main actor-isolated property",
            "/x/A.swift:3:9: error: cannot find 'x' in scope",
            "macro expansion @CasePathable:48:74: error: cannot find type 'ClientSequenceUpdate' in scope",
            "SwiftCompile normal arm64 Compiling\\ StepContainerView.swift /x/StepContainerView.swift (in target 'StepFlow' from project 'Demo')",
            "Compiling StepContainerView.swift",
            "** BUILD FAILED **",
            "",
            "note: Removed stale file '/x/build'",
            "[#DeprecatedDeclaration]: <https://docs.swift.org/compiler/documentation/diagnostics/deprecated-declaration>",
            "Test Case '-[SumTests.SumXCTests testAdds]' passed (0.001 seconds).",
        ] {
            assert!(!Xcode.continues("", no), "{no}");
        }
        assert!(!Cargo.continues("", "  |") && !Node.continues("", " 85 | x"), "their own output is grouped already, or not swiftc's");
        // A task's `cargo build`: rustc's human-readable block (from a backend task's log) is one record; the next
        // `warning:` header, and xcodebuild's own unindented notes, start their own.
        let rustc = [
            "warning: variant `NotPremium` is never constructed",
            "  --> crates/rest/src/security.rs:69:5",
            "   |",
            "53 | pub enum AuthorizationError {",
            "   |          ------------------ variant in this enum",
            "...",
            "69 |     NotPremium,",
            "   |     ^^^^^^^^^^",
            "   |",
            "   = note: `AuthorizationError` has a derived impl for the trait `Debug`, but this is intentionally ignored during dead code analysis",
            "   = help: remove it",
            "",
            "warning: `rest` (lib) generated 1 warning",
            "   Compiling items v0.26.1 (/x/crates/items)",
            "note: Removed stale file '/x/build'",
        ];
        let recs = crate::daemon::group(&rustc, |h, l| Task.continues(h, l));
        assert_eq!(recs.len(), 4, "{recs:#?}");
        assert_eq!(recs[0].lines().count(), 11, "the whole block, its trailing blank trimmed");
        assert!(recs[1].starts_with("warning: `rest`") && recs[2].trim_start().starts_with("Compiling items"));
        assert!(!Task.continues("", "warning: unused variable: `x`") && !Task.continues("", "error[E0425]: cannot find value `x` in this scope"));
        assert!(!Task.continues("", "--> no indent") && !Task.continues("", "note: Removed stale file '/x/build'"));
    }

    #[test]
    fn diagnostics_are_read_from_each_tools_lines() {
        let d = |l: &str| diagnostic(l).map(|v| (v["kind"].as_str().unwrap().to_string(), v["path"].as_str().unwrap().to_string(), v["line"].as_u64().unwrap(), v["col"].as_u64().unwrap(), v["rule"].as_str().map(String::from)));
        let is = |l: &str, kind: &str, path: &str, line: u64, col: u64, rule: Option<&str>| {
            assert_eq!(d(l), Some((kind.into(), path.into(), line, col, rule.map(String::from))), "{l}");
        };
        // Seen in two apps' builds.
        is("/Users/dev/a/ProfileClient.swift:64:30: warning: Simplify error composition", "warning", "/Users/dev/a/ProfileClient.swift", 64, 30, None);
        is("/Users/dev/a/SettingsClientTests.swift:12:28: error: 'Cache' requires the types 'String' a", "error", "/Users/dev/a/SettingsClientTests.swift", 12, 28, None);
        is("/x/ApplyAPIKey.swift:24:46: warning: Empty String Violation: Prefer checking `isEmpty` over comparing `string` to an empty string literal (empty_string)", "lint", "/x/ApplyAPIKey.swift", 24, 46, Some("empty_string"));
        is("/x/NetworkClientTests.swift:18:1: error: (emptyBraces)", "lint", "/x/NetworkClientTests.swift", 18, 1, Some("emptyBraces"));
        is("[!]  DemoSupport/TimelineManager.swift:94:33: 'difficulty' is deprecated: will be removed in future [#DeprecatedDeclaration]", "warning", "DemoSupport/TimelineManager.swift", 94, 33, Some("DeprecatedDeclaration"));
        let never = "/x/A.swift:3:9: warning: variable 'x' was never mutated; consider changing to 'let' constant [#VariableNeverMutated]";
        is(never, "warning", "/x/A.swift", 3, 9, Some("VariableNeverMutated"));
        assert_eq!(diagnostic(never).unwrap()["message"], "variable 'x' was never mutated; consider changing to 'let' constant", "the group off the message");
        is("[!]  /x/SpeechService.swift:132:20: Todo Violation: TODOs should be resolved (Propagate error in…)", "lint", "/x/SpeechService.swift", 132, 20, None);
        let rustc = "warning: struct `PremiumAuthentication` is never constructed\n  --> crates/rest/src/security.rs:45:12\n   |\n45 | struct PremiumAuthentication;";
        is(rustc, "warning", "crates/rest/src/security.rs", 45, 12, None);
        assert_eq!(diagnostic(rustc).unwrap()["message"], "struct `PremiumAuthentication` is never constructed");
        // From tsc's docs, not seen live.
        is("src/a.ts(12,5): error TS2322: Type 'string' is not assignable to type 'number'.", "error", "src/a.ts", 12, 5, None);
        is("src/a.ts:12:5 - error TS2322: Type 'string' is not assignable to type 'number'.", "error", "src/a.ts", 12, 5, None);
        for none in [
            "macro expansion @Test:9:11: warning: unused",
            "/x/SumTests.swift:10: error: -[SumTests.SumXCTests testFails] :",
            "[x] xcodebuild: error: The workspace named \"Demo\" does not contain a scheme named \"Demo-Dev\".",
            "error: could not compile `rest` (lib) due to 1 previous error",
            "2026-10-29 16:04:10.946737+0100 Demo_Dev[41709:12037739] [system] INFO: nav",
        ] {
            assert_eq!(d(none), None, "{none}");
        }
    }

    #[test]
    fn xcodebuild_and_tuist_commands_are_recognised() {
        let argv = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        for yes in ["xcodebuild -scheme X", "/usr/bin/xcodebuild", "xcrun xcodebuild -list", "tuist", "tuist test --foo"] {
            assert!(runs_xcode(&argv(yes)), "{yes}");
        }
        for no in ["xcrun simctl list", "cargo test", "sh -c 'xcodebuild'", ""] {
            assert!(!runs_xcode(&argv(no)), "{no}");
        }
        let exec = Target { name: "b".into(), ecosystem: "task".into(), dir: "/w".into(), kind: "exec".into(), package: None, needs_port: false, port: None, target_dir: None };
        assert_eq!(command_of(&exec, &argv("tuist test")), argv("tuist test"));
    }

    #[test]
    fn failed_tests_are_errors_and_lock_waits_warnings() {
        let level = |l: &str| heuristic(Source::Stdout, l);
        // Captured: node 22 `node --test` piped, libtest, pytest 9 (plain and --color=yes), go 1.27.
        let captured = [
            "not ok 1 - fails",
            "    not ok 1 - sub fails",
            "test bad ... FAILED",
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s",
            "FAILED tests/test_x.py::test_bad - assert 1 == 2",
            "========================= 1 failed, 1 passed in 0.02s ==========================",
            "\x1b[31mFAILED\x1b[0m tests/test_x.py::\x1b[1mtest_bad\x1b[0m - assert 1 == 2",
            "\x1b[31m======================== \x1b[31m\x1b[1m1 failed\x1b[0m, \x1b[32m1 passed\x1b[0m\x1b[31m in 0.03s\x1b[0m\x1b[31m ========================\x1b[0m",
            "--- FAIL: TestBad (0.00s)",
            "    --- FAIL: TestBad/sub (0.00s)",
            "FAIL\texample.com/gop\t0.227s",
            "FAIL",
            // xcodebuild 27 test-without-building, macOS destination (more in xcodebuild-test-fail.txt).
            "􀢄 Test failsSwiftTesting() recorded an issue at SumTests.swift:6:34: Expectation failed: Sum.add(1, 2) == 4",
            "** TEST EXECUTE FAILED **",
        ];
        // From the brief or memory: jest, vitest, xcodebuild's `test` action.
        let recalled = ["  ✕ adds (3 ms)", " × src/sum.test.ts > adds", " FAIL  src/sum.test.ts > adds", "** TEST FAILED **"];
        for l in captured.iter().chain(&recalled) {
            assert_eq!(level(l), Level::Error, "{l:?}");
        }
        for l in ["ok 1 - passes", "test good ... ok", "==== 2 passed in 0.01s ====", "GET /tests/failed 200", "--- PASS: TestGood (0.00s)", "Failover to replica"] {
            assert_eq!(level(l), Level::Info, "{l:?}");
        }
        let locks = [
            "    Blocking waiting for file lock on build directory",
            // `CARGO_TERM_COLOR=always`, captured from cargo 1.96.
            "\x1b[1m\x1b[92m    Blocking\x1b[0m waiting for file lock on package cache",
            "warning Waiting for the other yarn instance to finish (pid 4242, inside \"/r\")",
            r#"error: unable to attach DB: error: accessing build database "/d/XCBuildData/build.db": database is locked Possibly there are two concurrent builds running in the same filesystem location."#,
        ];
        for l in locks {
            assert_eq!(heuristic(Source::Stderr, l), Level::Warn, "{l:?}");
        }
        assert_eq!(Xcode.classify(Source::Build, locks[3]).0, Level::Warn);
        assert!(!waits_for_lock("SQLite error: database is locked"), "an app's own lock error stays an error");
    }

    #[test]
    fn cargo_build_warning_is_a_build_warn_record() {
        let t = detect_all(&fixture("cargo-ws")).unwrap().into_iter().find(|t| t.name == "beta").unwrap();
        let out = Cargo
            .build(&t, &Ctx::default())
            .unwrap()
            .env("CARGO_TARGET_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/target/fixtures"))
            .output()
            .unwrap();
        let levels: Vec<_> = String::from_utf8_lossy(&out.stdout).lines().map(|l| Cargo.classify(Source::Build, l)).collect();
        assert!(levels.iter().any(|(l, s)| *l == Level::Warn && s.contains("unused variable")), "{levels:?}");
    }

    #[test]
    fn cargo_run_does_not_replay_build_warnings() {
        let mut t = detect_all(&fixture("cargo-ws")).unwrap().into_iter().find(|t| t.name == "beta").unwrap();
        // Its own target dir: another test's concurrent build of beta in target/fixtures can replace the binary while
        // this one spawns it (seen as NotFound).
        let target_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/fixtures-run");
        assert!(Cargo.build(&t, &Ctx::default()).unwrap().env("CARGO_TARGET_DIR", target_dir).output().unwrap().status.success());
        t.target_dir = Some(target_dir.into());
        let out = Cargo.run(&t, &Ctx::default()).output().unwrap();
        assert_eq!((&*String::from_utf8_lossy(&out.stdout), &*String::from_utf8_lossy(&out.stderr)), ("beta\n", ""));
    }

    #[test]
    fn node_scripts_and_port_needs() {
        let targets = Node.detect(&fixture("node-app")).unwrap();
        let needs: Vec<_> = targets.iter().map(|t| (t.name.as_str(), !Node.needs(t).is_empty())).collect();
        assert_eq!(needs, [("dev", true), ("lint", false), ("test", false)]);
    }

    #[test]
    fn broken_ecosystem_does_not_hide_others() {
        let targets = detect_all(&fixture("broken-cargo")).unwrap();
        assert_eq!(targets.iter().map(|t| (t.ecosystem.as_str(), t.name.as_str())).collect::<Vec<_>>(), [("node", "dev")]);
    }

    #[test]
    fn node_pm_from_nearest_lockfile() {
        let pm = |dir| Node.run(&Node.detect(&fixture(dir)).unwrap()[0], &Ctx::default()).get_program().to_owned();
        assert_eq!([pm("pnpm-ws/packages/web"), pm("bun-app"), pm("node-app")], ["pnpm", "bun", "npm"]);
    }

    #[test]
    fn collie_toml_overrides_port() {
        let t = detect_all(&fixture("cargo-ws")).unwrap().into_iter().find(|t| t.name == "alpha").unwrap();
        assert_eq!(Cargo.needs(&t), [Resource::Port { preferred: Some(3999) }]);
    }

    #[test]
    fn xcode_detects_app_schemes_through_nested_groups() {
        let ts = Xcode.detect(&fixture("xcode-ws/Projects/My App")).unwrap();
        // Widget's Run action launches an app extension and it has no tests: it only builds.
        assert_eq!(ts.iter().map(|t| (t.name.as_str(), t.kind.as_str())).collect::<Vec<_>>(), [("My App", "app"), ("My App (test)", "test"), ("Widget", "build")]);
        assert_eq!(ts[0].dir, fixture("xcode-ws"));
        assert!(ts[0].package.as_deref().unwrap().ends_with("xcode-ws/App.xcworkspace"));
        assert_eq!(Xcode.needs(&ts[0]), [Resource::Simulator]);
        assert!(ts[2].package.as_deref().unwrap().ends_with("xcode-ws/App.xcworkspace"));
        assert_eq!(Xcode.needs(&ts[2]), [], "no simulator, device or port");
        // No device to name: a generic destination, or none (the fixture has no project to ask for its platforms).
        let args = Xcode.build(&ts[2], &Ctx::default()).unwrap().get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert!(args.contains("-scheme Widget") && args.ends_with("build -quiet") && !args.contains("id="), "{args}");
        assert!(!builds("<Scheme><BuildAction><BuildActionEntries></BuildActionEntries></BuildAction></Scheme>"), "nothing to build: skipped");
        assert_eq!(generic_for(&["iphoneos", "iphonesimulator"]), Some("generic/platform=iOS Simulator"));
        assert_eq!(generic_for(&["macosx", "iphonesimulator"]), Some("generic/platform=iOS Simulator"));
        assert_eq!(generic_for(&["macosx"]), Some("generic/platform=macOS"));
        assert_eq!(generic_for(&["driverkit"]), None);
    }

    #[test]
    fn xcode_test_schemes_come_from_the_users_xcuserdata_too_and_run_their_tests() {
        // A copy, so the scheme can sit under this machine's $USER without a per-user file in the repo.
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/xcode-user");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(Command::new("cp").arg("-R").arg(fixture("xcode-ws")).arg(&tmp).status().unwrap().success());
        let schemes = tmp.join(format!("xcode-ws/Projects/My App/My App.xcodeproj/xcuserdata/{}.xcuserdatad/xcschemes", std::env::var("USER").unwrap()));
        std::fs::create_dir_all(&schemes).unwrap();
        std::fs::copy(fixture("xcode-user-scheme.xcscheme"), schemes.join("CoreTests.xcscheme")).unwrap();
        // A user copy of a shared scheme doesn't list it twice.
        std::fs::copy(fixture("xcode-ws/Projects/My App/My App.xcodeproj/xcshareddata/xcschemes/My App.xcscheme"), schemes.join("My App.xcscheme")).unwrap();
        let ts = Xcode.detect(&tmp.join("xcode-ws")).unwrap();
        let kinds: Vec<_> = ts.iter().map(|t| (t.name.as_str(), t.kind.as_str())).collect();
        assert_eq!(kinds, [("My App", "app"), ("My App (test)", "test"), ("CoreTests", "test"), ("Widget", "build")]);
        let ctx = Ctx { simulator: Some("U".into()), log: "/logs/p1.log".into(), ..Default::default() };
        let args = |c: Command| c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        let (app_tests, core) = (&ts[1], &ts[2]);
        assert!(args(Xcode.build(app_tests, &ctx).unwrap()).ends_with("-scheme My App -destination id=U build-for-testing -quiet"), "the app scheme's own name");
        let run = args(Xcode.run(core, &ctx));
        let want = "-scheme CoreTests -destination id=U test-without-building -test-timeouts-enabled YES -maximum-test-execution-time-allowance 600 -collect-test-diagnostics never -resultBundlePath /logs/p1.xcresult";
        assert!(run.ends_with(want), "{run}");
        assert!(run.contains("bootstatus") && run.contains(" sh U xcodebuild -workspace "), "the simulator is booted first: {run}");
        let phone = Ctx { device: Some("P".into()), ..Default::default() };
        assert_eq!(Xcode.run(core, &phone).get_program(), "xcodebuild", "a physical device isn't");
        assert!(args(Xcode.build(&ts[0], &ctx).unwrap()).ends_with("build -quiet"), "the app still builds");
        std::fs::copy(fixture("xcode-user-scheme.xcscheme"), schemes.join("My App (test).xcscheme")).unwrap();
        let e = Xcode.detect(&tmp.join("xcode-ws")).unwrap_err();
        assert_eq!(e.downcast_ref::<crate::daemon::Coded>().unwrap().0, "bad_config");
        assert!(e.to_string().ends_with("App.xcworkspace: scheme \"My App (test)\" clashes with the tests of app scheme \"My App\"; rename it"), "{e}");
    }

    #[test]
    fn xcode_classify_and_launched() {
        let c = |src, l: &str| Xcode.classify(src, l).0;
        assert_eq!(c(Source::Build, "/p/Feed.swift:3:9: warning: initialization of 'x' was never used"), Level::Warn);
        assert_eq!(c(Source::Build, "/p/Feed.swift:3:9: error: cannot find 'y' in scope"), Level::Error);
        assert_eq!(c(Source::Build, "** BUILD FAILED **"), Level::Error);
        // Bare build-system lines, Xcode 27: an app's build-for-testing, a scratch package's failed build.
        assert_eq!(c(Source::Build, "warning: 'SFSymbolsMacroImpl' is missing a dependency on 'SwiftParser' because dependency scan of Swift module 'SFSymbolsMacroImpl' discovered a dependency on 'SwiftParser'"), Level::Warn);
        assert_eq!(c(Source::Build, "error: the following command failed with exit code 1 but produced no further output"), Level::Error);
        assert_eq!(c(Source::Build, "error: the following command failed with exit code 0 but produced no further output"), Level::Warn, "a build that succeeded");
        assert_eq!(c(Source::Build, "collie: error: .xcode-version wants Xcode 99.0"), Level::Error);
        let log = |kind: &str, sub: &str| format!(r#"{{"messageType":"{kind}","subsystem":"{sub}","eventMessage":"feed loaded"}}"#);
        assert_eq!(Xcode.classify(Source::Stdout, &log("Default", "com.example.myapp")), (Level::Info, "feed loaded".into()));
        assert_eq!(c(Source::Stdout, &log("Error", "com.example.myapp")), Level::Error);
        assert_eq!(c(Source::Stdout, &log("Fault", "com.example.myapp")), Level::Error);
        assert_eq!(c(Source::Stdout, &log("Debug", "com.example.myapp")), Level::Debug);
        assert_eq!(c(Source::Stdout, &log("Error", "com.apple.UIKit")), Level::Debug);
        assert!(Xcode.launched(Source::Stdout, "com.example.myapp.dev: 7592"));
        assert!(!Xcode.launched(Source::Stdout, "cfg v1.2: 3"));
        assert!(!Xcode.launched(Source::Stderr, "com.example.myapp.dev: 7592"));
        assert!(!Xcode.launched(Source::Stdout, "Migration Elapsed:00:00 seconds"));
        assert!(!Xcode.launched(Source::Stdout, "[2026-10-28 13:58:30 +0000] Status=0, isTerminal=NO, Elapsed=00:00."));
        assert!(!Xcode.launched(Source::Stdout, &log("Default", "com.example.myapp")));
        // devicectl 651.13.5, `device process launch --console`, after `device install app`'s `• options: ` line.
        assert!(Xcode.launched(Source::Stdout, "Launched application with com.example.myapp.dev bundle identifier."));
        assert!(!Xcode.launched(Source::Stderr, "Launched application with com.example.myapp.dev bundle identifier."));
        assert!(!Xcode.launched(Source::Stdout, "• bundleID: com.example.myapp.dev"));
        assert!(!Xcode.launched(Source::Stdout, "Waiting for the application to terminate…"));
        assert_eq!(c(Source::Stdout, "App terminated due to signal 11."), Level::Error, "a crash");
        assert_eq!(c(Source::Stdout, "App terminated due to signal 15."), Level::Info, "collie stop");
        // simctl failing to launch the app, on the run phase's stderr.
        let launch = "An error was encountered processing the command (domain=FBSOpenApplicationServiceErrorDomain, code=4):";
        assert_eq!(c(Source::Stderr, launch), Level::Error);
        assert_eq!(c(Source::Stderr, "The request to open \"com.x\" failed."), Level::Error);
        // A command running xcodebuild reads its run output by the build rules: names stay info, on stderr too.
        assert_eq!(XcodeTool.classify(Source::Stderr, "Linting 'ErrorView.swift' (3/4)").0, Level::Info);
        assert_eq!(XcodeTool.classify(Source::Stdout, "  ** TEST FAILED ** ").0, Level::Error);
    }

    // `simctl list runtimes available --json`, trimmed (oldest first, as simctl lists them); an iPad is moved first in 27.0.
    const RUNTIMES: &str = r#"{"runtimes":[
        {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-18-5","name":"iOS 18.5","version":"18.5","platform":"iOS","isAvailable":true,
         "supportedDeviceTypes":[{"identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro","name":"iPhone 16 Pro","productFamily":"iPhone"}]},
        {"identifier":"com.apple.CoreSimulator.SimRuntime.watchOS-11-0","name":"watchOS 11.0","version":"11.0","platform":"watchOS","isAvailable":true,
         "supportedDeviceTypes":[]},
        {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-27-0","name":"iOS 27.0","version":"27.0","platform":"iOS","isAvailable":true,
         "supportedDeviceTypes":[
           {"identifier":"com.apple.CoreSimulator.SimDeviceType.iPad-Pro-13-inch-M5","name":"iPad Pro 13-inch (M5)","productFamily":"iPad"},
           {"identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-18-Pro","name":"iPhone 18 Pro","productFamily":"iPhone"},
           {"identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro","name":"iPhone 16 Pro","productFamily":"iPhone"}]}
    ]}"#;

    #[test]
    fn sim_kind_resolves_names_versions_and_defaults() {
        let rts = ios_runtimes(&serde_json::from_str(RUNTIMES).unwrap());
        assert_eq!(rts.iter().map(|r| r["version"].as_str().unwrap()).collect::<Vec<_>>(), ["27.0", "18.5"]);
        let names = |k: SimKind| (k.device_name, k.runtime_name);
        assert_eq!(names(resolve(&rts, &Value::Null, None, None).unwrap()), ("iPhone 18 Pro".into(), "iOS 27.0".into()));
        let k = resolve(&rts, &Value::Null, Some("iPhone 16 Pro"), Some("18.5")).unwrap();
        assert_eq!((k.device.as_str(), k.runtime.as_str()), ("com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro", "com.apple.CoreSimulator.SimRuntime.iOS-18-5"));
        assert_eq!(resolve(&rts, &Value::Null, Some("iphone 16 pro"), Some("iOS 18.5")).unwrap(), k);
        let base = json!({"device": k.device, "runtime": k.runtime});
        assert_eq!(names(resolve(&rts, &base, None, Some("27.0")).unwrap()), ("iPhone 16 Pro".into(), "iOS 27.0".into()));
        let code = |r: Result<SimKind>| r.unwrap_err().downcast_ref::<crate::daemon::Coded>().unwrap().0.clone();
        assert_eq!(code(resolve(&rts, &Value::Null, None, Some("26.5"))), "not_installed");
        assert_eq!(code(resolve(&rts, &Value::Null, Some("iPhone 18 Pro"), Some("18.5"))), "bad_request");
        assert_eq!(sims_json(&rts)["runtimes"][0]["devices"][1], json!({"id": "com.apple.CoreSimulator.SimDeviceType.iPhone-18-Pro", "name": "iPhone 18 Pro"}));
    }

    #[test]
    fn devicectl_lists_physical_devices_only() {
        // `devicectl list devices -j -` with Xcode 27.0, trimmed: the simulator `collie 3` is dropped.
        let mut list: Value = serde_json::from_slice(&std::fs::read(fixture("devicectl-devices.json")).unwrap()).unwrap();
        let ds = physical(&list);
        let row = |d: &Physical| (d.name.clone(), d.model.clone(), d.os.clone(), d.state.clone(), d.developer_mode);
        let s = |n: &str, m: &str, o: &str, st: &str| (n.to_string(), m.to_string(), o.to_string(), st.to_string(), true);
        assert_eq!(
            ds.iter().map(row).collect::<Vec<_>>(),
            [
                s("iPad", "iPad (9th generation)", "iOS 18.3.1", "disconnected"),
                s("iPhone 13 mini", "iPhone 13 mini", "iOS 27.0.1", "unavailable"),
                s("Test iPhone", "iPhone 13 Pro", "iOS 27.0", "connected"),
            ]
        );
        assert_eq!(json!(ds[2]), json!({"udid": "00000000-0000000000000001", "name": "Test iPhone", "model": "iPhone 13 Pro", "os": "iOS 27.0", "state": "connected", "developer_mode": true}));
        let code = |r: Result<&Physical>| r.unwrap_err().downcast_ref::<crate::daemon::Coded>().unwrap().0.clone();
        assert_eq!(find_physical(&ds, "test iphone", true).unwrap().udid, "00000000-0000000000000001");
        assert_eq!(find_physical(&ds, "00000000-0000000000000002", true).unwrap().name, "iPad", "disconnected is reachable");
        assert_eq!(code(find_physical(&ds, "iPhone 13 mini", true)), "unavailable");
        assert!(find_physical(&ds, "iPhone 13 mini", false).is_ok(), "a choice can be stored while it's away");
        assert_eq!(code(find_physical(&ds, "collie 3", false)), "not_found");
        assert_eq!(code(find_physical(&ds, "collie 3", true)), "unavailable");
        let phone =&mut list["result"]["devices"][3]["properties"];
        phone["state"]["name"] = json!("Dev\u{2019}s iPhone");
        let ds = physical(&list);
        assert_eq!(find_physical(&ds, "dev's iphone", true).unwrap().udid, "00000000-0000000000000001");
        assert_eq!(find_physical(&ds, "Dev\u{2019}s iPhone", true).unwrap().udid, "00000000-0000000000000001");
        let phone =&mut list["result"]["devices"][3]["properties"];
        phone["state"]["developerModeStatus"] = json!({"disabled": {}});
        phone["connection"]["pairingState"] = json!("unpaired");
        let ds = physical(&list);
        assert!(!ds[2].developer_mode);
        assert_eq!(code(find_physical(&ds, "00000000-0000000000000001", false)), "unavailable", "unpaired");
    }

    #[test]
    fn pending_names_find_only_those_devices() {
        let list = json!({"devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-27-0": [
                {"name": "iPhone 18 Pro", "udid": "USERS_OWN"},
                {"name": "collie 7 creating 4242-1793200000000", "udid": "MINE"},
                {"name": "collie 7", "udid": "OTHER_DAEMONS_COLLIE_7"},
                {"name": "collie 7 creating 999-1793200000000", "udid": "OTHER_DAEMONS_PENDING"}]
        }});
        assert_eq!(named(&list, &["collie 7 creating 4242-1793200000000".into()]), ["MINE"]);
        assert!(named(&list, &[]).is_empty());
    }

    #[test]
    fn xcode_version_picks_the_matching_xcode() {
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/xcode-version");
        let _ = std::fs::remove_dir_all(&tmp);
        let (apps, proj) = (tmp.join("Applications"), tmp.join("proj/sub"));
        for (app, v) in [("Xcode-27.0.0-release.candidate.app", "27.0"), ("Xcode-27.1.0.app", "27.1")] {
            std::fs::create_dir_all(apps.join(app).join("Contents")).unwrap();
            let plist = format!("<dict>\n\t<key>CFBundleShortVersionString</key>\n\t<string>{v}</string>\n</dict>");
            std::fs::write(apps.join(app).join("Contents/version.plist"), plist).unwrap();
        }
        std::fs::create_dir_all(&proj).unwrap();
        assert_eq!(developer_dir(&proj, &apps).unwrap(), None);
        // The caller's env reaches showBuildSettings and simctl; a pinned Xcode wins over its DEVELOPER_DIR.
        let env: HashMap<String, String> = [("DEVELOPER_DIR", "/caller"), ("FOO", "1")].map(|(k, v)| (k.into(), v.into())).into();
        let ctx = Ctx { simulator: Some("U".into()), env: env.clone(), ..Default::default() };
        let t = Target { dir: proj.clone(), package: Some("A.xcworkspace".into()), ..Xcode.detect(&fixture("xcode-ws")).unwrap()[0].clone() };
        let envs = |t: &Target| -> HashMap<String, String> {
            let cmd = Xcode::xcodebuild(t, &ctx, &apps).unwrap();
            cmd.get_envs().map(|(k, v)| (k.to_string_lossy().into(), v.unwrap().to_string_lossy().into())).collect()
        };
        assert_eq!((envs(&t)["DEVELOPER_DIR"].as_str(), envs(&t)["FOO"].as_str()), ("/caller", "1"));
        assert_eq!(caller_developer_dir(&proj, &env).as_deref(), Some("/caller"));
        std::fs::write(tmp.join("proj/.xcode-version"), "27.0\n").unwrap();
        assert_eq!(developer_dir(&proj, &apps).unwrap(), Some(apps.join("Xcode-27.0.0-release.candidate.app/Contents/Developer")));
        let pinned = apps.join("Xcode-27.0.0-release.candidate.app/Contents/Developer");
        assert_eq!(envs(&t)["DEVELOPER_DIR"], pinned.to_string_lossy());
        assert_eq!(caller_developer_dir(&proj, &env), None);
        std::fs::write(tmp.join("proj/.xcode-version"), "99.0").unwrap();
        let e = developer_dir(&proj, &apps).unwrap_err().to_string();
        for want in ["Xcode 99.0", "27.0 at ", "27.1 at ", "Xcode-27.1.0.app", "xcodes install 99.0"] {
            assert!(e.contains(want), "{e}");
        }
    }
}
