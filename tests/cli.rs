use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::Duration;

/// Each test daemon leases from its own port pool, so two of them would hand out the same port at once.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A daemon with its own state dir and no herdr; stopped on drop.
struct Daemon(PathBuf, #[allow(dead_code)] Option<MutexGuard<'static, ()>>);

impl Daemon {
    fn new(name: &str) -> Self {
        let turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let mut d = Daemon::beside(name);
        d.1 = Some(turn);
        d
    }

    /// A second daemon within a test that holds `ONE_AT_A_TIME` already (its first one's).
    fn beside(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Daemon(dir, None)
    }

    /// `collie --json <args>` in the node fixture as herdr pane `holder`.
    fn cmd(&self, holder: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_collie"));
        cmd.arg("--json")
            .args(args)
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/node-app"))
            .env("COLLIE_STATE_DIR", &self.0)
            .env("HERDR_SOCKET_PATH", self.0.join("no-herdr.sock"))
            .env("HERDR_PANE_ID", holder)
            // Inside a GitHub Actions job, it would stop the client starting a daemon.
            .env_remove("RUNNER_TRACKING_ID");
        cmd
    }

    fn collie(&self, holder: &str, args: &[&str]) -> (bool, Value) {
        let out = self.cmd(holder, args).output().unwrap();
        let text = if out.status.success() { out.stdout } else { out.stderr };
        (out.status.success(), serde_json::from_slice(&text).unwrap_or(Value::Null))
    }

    /// `collie` as holder `t:p1`, in `dir` rather than the node fixture.
    fn collie_in(&self, dir: &Path, args: &[&str]) -> (bool, Value) {
        let out = self.cmd("t:p1", args).current_dir(dir).output().unwrap();
        let text = if out.status.success() { out.stdout } else { out.stderr };
        (out.status.success(), serde_json::from_slice(&text).unwrap_or(Value::Null))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.collie("t:p0", &["daemon", "stop"]);
    }
}

#[test]
fn node_dev_reaches_port_open() {
    let d = Daemon::new("port-open");
    let (ok, run) = d.collie("t:p1", &["run", "dev"]);
    assert!(ok, "{run}");
    assert!(run["port"].as_u64().is_some(), "{run}");
    let (ok, waited) = d.collie("t:p1", &["wait", "--until", "port-open", "--timeout", "15000", "dev"]);
    assert!(ok, "{waited}");
    assert_eq!(waited["state"], "running");
}

#[test]
fn second_holder_is_refused_unless_take() {
    let d = Daemon::new("held");
    let (ok, first) = d.collie("t:p1", &["run", "dev"]);
    assert!(ok, "{first}");
    let logs = || std::fs::read_dir(d.0.join("logs")).unwrap().count();
    let before = logs();
    let (ok, err) = d.collie("t:p2", &["run", "dev"]);
    assert!(!ok);
    assert_eq!(err["error"]["code"], "held", "{err}");
    assert_eq!(logs(), before, "a refused run leaves no log file");
    let (ok, taken) = d.collie("t:p2", &["run", "dev", "--take"]);
    assert!(ok, "{taken}");
    let (_, status) = d.collie("t:p2", &["status"]);
    let holders: Vec<_> = status["processes"].as_array().unwrap().iter().map(|p| p["holder"].as_str().unwrap()).collect();
    assert_eq!(holders, ["t:p2"]);
}

/// The log lines of `target`'s latest process (`logs --json` prints a record per line, then the cursor).
fn lines(d: &Daemon, target: &str) -> Vec<String> {
    let out = d.cmd("t:p1", &["logs", target]).output().unwrap();
    let recs = out.stdout.split(|&b| b == b'\n').filter_map(|l| serde_json::from_slice::<Value>(l).ok());
    recs.filter_map(|r| r["line"].as_str().map(String::from)).collect()
}

#[test]
fn prepare_runs_before_a_build_only_when_a_watched_path_changed() {
    let d = Daemon::new("prepare");
    let repo = d.0.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    // The generated "project" is a package.json: its script exists only once prepare has run.
    let prep = r#"[prepare]
cmd = "echo gen >> generated.txt && printf '{\"scripts\":{\"hi\":\"cat generated.txt\"}}' > package.json"
watch = ["src"]
"#;
    std::fs::write(repo.join("collie.toml"), prep).unwrap();
    let run = || {
        let out = d.cmd("t:p1", &["run", "hi"]).current_dir(&repo).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let (ok, waited) = d.collie_in(&repo, &["wait", "--until", "exited", "--timeout", "10000", "hi"]);
        assert!(ok, "{waited}");
        std::fs::read_to_string(repo.join("generated.txt")).unwrap().lines().count()
    };
    assert_eq!(run(), 1, "a fresh checkout has no `hi` until prepare runs");
    assert_eq!(run(), 1, "nothing watched changed");
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(repo.join("src/new.swift"), "").unwrap();
    assert_eq!(run(), 2, "a file added under a watched dir");
    let out = d.cmd("t:p1", &["logs", "hi"]).current_dir(&repo).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("collie: prepare"), "logged with the build: {}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn builds_beyond_the_cap_queue_for_a_slot_in_arrival_order() {
    let d = Daemon::new("build-slots");
    std::fs::write(d.0.join("config.toml"), "[builds]\nmax = 1\n").unwrap();
    let (ok, a) = d.collie("t:p1", &["exec", "--name", "a", "--", "sleep", "2"]);
    assert!(ok, "{a}");
    let (ok, b) = d.collie("t:p1", &["exec", "--name", "b", "--", "true"]);
    assert!(ok, "{b}");
    let b_id = b["id"].as_str().unwrap();
    let (_, status) = d.collie("t:p1", &["status"]);
    let proc_b = status["processes"].as_array().unwrap().iter().find(|p| p["id"] == b_id).unwrap();
    assert_eq!((&proc_b["state"], &proc_b["awaits_slot"]), (&"queued".into(), &true.into()), "{status}");
    assert_eq!(status["builds"]["cap"], 1, "{status}");
    assert_eq!(status["builds"]["queued"], serde_json::json!([b_id]), "{status}");
    let (ok, err) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "300", "b"]);
    assert!(!ok);
    assert!(err["error"]["message"].as_str().unwrap().contains("still queued for a build slot (1 of 1 build slots busy: a"), "{err}");
    let (ok, b) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "15000", "b"]);
    assert!(ok, "{b}");
    let (_, status) = d.collie("t:p1", &["status"]);
    let proc_a = status["processes"].as_array().unwrap().iter().find(|p| p["target"] == "a").unwrap();
    assert!(proc_a["ended_at"].as_u64() <= b["ended_at"].as_u64(), "b ran only after a ended: {status}");
    assert_eq!(status["builds"]["running"], serde_json::json!([]), "both gave their slot back: {status}");
}

#[test]
fn a_ci_build_dib_holds_a_slot_goes_first_and_ends_with_its_pid() {
    let d = Daemon::new("build-dibs");
    std::fs::write(d.0.join("config.toml"), "[builds]\nmax = 1\n").unwrap();
    // Stand-ins for CI job workers; reaped once killed, as a zombie still shows in `ps`.
    let job = || Command::new("sleep").arg("60").spawn().unwrap();
    let (mut a, mut b, mut c) = (job(), job(), job());
    let pid = |p: &std::process::Child| p.id().to_string();
    let status = || d.collie("t:p1", &["status"]).1;
    let (ok, la) = d.collie("t:ci", &["dibs", "acquire", "build", "--pid", &pid(&a), "--name", "A"]);
    assert!(ok, "{la}");
    let s = status();
    assert_eq!(s["leases"][0]["pid"], a.id(), "{s}");
    assert_eq!(s["builds"]["running"], serde_json::json!([format!("dibs:{}", la["id"])]), "{s}");
    let (ok, x) = d.collie("t:p1", &["exec", "--name", "x", "--", "true"]);
    assert!(ok, "{x}");
    assert_eq!(status()["processes"][0]["awaits_slot"], true);
    let waiter = d.cmd("t:ci", &["dibs", "acquire", "build", "--wait", "--priority", "ci", "--pid", &pid(&b), "--name", "B"]).stdout(Stdio::piped()).spawn().unwrap();
    let t0 = std::time::Instant::now();
    while status()["builds"]["queued"].as_array().map_or(0, Vec::len) < 2 && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let s = status();
    assert!(s["builds"]["queued"][0].as_str().is_some_and(|q| q.starts_with("dibs:")), "B goes ahead of the earlier exec: {s}");
    assert_eq!((&s["queue"][0]["resource"], &s["queue"][0]["priority"]), (&"build".into(), &"ci".into()), "{s}");
    let (ok, err) = d.collie("t:ci", &["dibs", "acquire", "build", "--pid", &pid(&c)]);
    assert_eq!((ok, &err["error"]["code"]), (false, &"busy".into()), "{err}");
    assert!(err["error"]["message"].as_str().unwrap().contains("1 of 1 busy (A), 2 queued ahead"), "{err}");
    let (ok, released) = d.collie("t:ci", &["dibs", "release", "--pid", &pid(&a)]);
    assert!(ok, "{released}");
    let lb: Value = serde_json::from_slice(&waiter.wait_with_output().unwrap().stdout).unwrap_or_default();
    assert_eq!((&lb["pid"], &lb["priority"], &lb["name"]), (&b.id().into(), &"ci".into(), &"B".into()), "{lb}");
    assert_eq!(status()["processes"][0]["awaits_slot"], true, "the exec still waits behind B");
    b.kill().unwrap();
    b.wait().unwrap();
    // One 5 s sample notices; the rest is slack for a loaded machine.
    let (ok, ran) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "15000", "x"]);
    assert!(ok, "B's slot is freed once its pid is gone: {ran}");
    assert_eq!(status()["leases"], serde_json::json!([]));
    let (ok, err) = d.collie("t:ci", &["dibs", "release", "--pid", &pid(&a)]);
    assert_eq!((ok, &err["error"]["code"]), (false, &"not_found".into()), "{err}");
    for p in [&mut a, &mut c] {
        let _ = p.kill();
        let _ = p.wait();
    }
}

#[test]
fn a_ci_job_never_starts_the_daemon() {
    let d = Daemon::new("ci-no-daemon");
    let out = d.cmd("t:ci", &["dibs", "acquire", "build", "--pid", "1"]).env("RUNNER_TRACKING_ID", "github_1").output().unwrap();
    let err: Value = serde_json::from_slice(&out.stderr).unwrap_or_default();
    assert_eq!(err["error"]["code"], "unavailable", "{err}");
    assert!(!d.0.join("collie.sock").exists(), "no daemon started");
}

#[test]
fn logs_all_reads_a_whole_run_and_a_cursor_from_an_old_run_warns() {
    let d = Daemon::new("logs-all");
    let (ok, started) = d.collie("t:p1", &["exec", "--name", "many", "--", "sh", "-c", "seq 300"]);
    assert!(ok, "{started}");
    let (ok, ended) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "10000", "many"]);
    assert!(ok, "{ended}");
    let count = |args: &[&str]| {
        let out = d.cmd("t:p1", args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains("\"line\"")).count()
    };
    assert_eq!(count(&["logs", "--all", "many"]), 300);
    assert_eq!(count(&["logs", "many"]), 200);
    let (ok, err) = d.collie("t:p1", &["logs", "--since", "0"]);
    assert_eq!((ok, &err["error"]["code"]), (false, &"bad_request".into()), "{err}");
    assert!(err["error"]["message"].as_str().unwrap().contains("<process id>@<byte offset>"), "{err}");
    // A cursor taken in the first run of `say`, read after the second started.
    let say = |word: &str| {
        let (ok, v) = d.collie("t:p1", &["exec", "--name", "say", "--", "echo", word]);
        assert!(ok, "{v}");
        let (ok, v) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "10000", "say"]);
        assert!(ok, "{v}");
        v["id"].as_str().unwrap().to_string()
    };
    let first = say("one");
    let mark = String::from_utf8(d.cmd("t:p1", &["logs", "--mark", "say"]).output().unwrap().stdout).unwrap();
    let mark = serde_json::from_str::<Value>(mark.trim()).unwrap()["cursor"].as_str().unwrap().to_string();
    let second = say("two");
    // `cmd` passes --json; the plain form needs its own command.
    let json_out = d.cmd("t:p1", &["logs", "--since", &mark]).output().unwrap();
    let mut plain = Command::new(env!("CARGO_BIN_EXE_collie"));
    plain.args(["logs", "--since", &mark]).env("COLLIE_STATE_DIR", &d.0).env("HERDR_SOCKET_PATH", d.0.join("no-herdr.sock")).env("HERDR_PANE_ID", "t:p1");
    let text = String::from_utf8_lossy(&plain.output().unwrap().stderr).to_string();
    assert!(text.starts_with("warning: cursor is for ended run") && text.contains(&first) && text.contains(&second), "{text}");
    let json: Value = serde_json::from_slice(json_out.stderr.split(|&b| b == b'\n').next().unwrap()).unwrap();
    assert!(json["warning"].as_str().is_some_and(|w| w.contains(&first) && w.contains(&second)), "{json}");
}

#[test]
fn a_targets_build_warning_is_a_problem_and_an_exec_adds_none() {
    let d = Daemon::new("problems-build");
    let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cargo-tests"));
    let run = |args: &[&str]| {
        let out = d.cmd("t:p1", args).current_dir(dir).env("CARGO_TARGET_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/target/fixtures")).output().unwrap();
        let text = if out.status.success() { out.stdout } else { out.stderr };
        (out.status.success(), serde_json::from_slice::<Value>(&text).unwrap_or(Value::Null))
    };
    for (start, name) in [(vec!["run", "cargo test"], "cargo test"), (vec!["exec", "--name", "build", "--", "cargo", "build"], "build")] {
        let (ok, v) = run(&start);
        assert!(ok, "{v}");
        let (ok, v) = run(&["wait", "--until", "exited", "--timeout", "120000", name]);
        assert!(ok, "{v}");
    }
    let (ok, v) = run(&["problems"]);
    assert!(ok, "{v}");
    let warnings: Vec<&Value> = v["items"].as_array().unwrap().iter().filter(|i| i["kind"] == "warning").collect();
    assert_eq!(warnings.len(), 1, "{v}");
    let w = warnings[0];
    assert_eq!((&w["path"], &w["line"]), (&"tests/fixtures/cargo-tests/src/lib.rs".into(), &2.into()), "{w}");
    let sources: Vec<&Value> = w["sources"].as_array().unwrap().iter().map(|s| &s["target"]).collect();
    assert_eq!(sources, [&Value::from("cargo test")], "the exec is no source: {w}");
}

#[test]
fn a_cargo_execs_progress_bar_stays_out_of_its_log_and_history() {
    let d = Daemon::new("cargo-bar");
    let fake_cargo = d.0.join("cargo");
    let script = "#!/bin/sh\n[ \"$CARGO_TERM_PROGRESS_WHEN\" = always ] || exit 9\nprintf '    Building [==>  ] 3/9: a\\r   Compiling x v1.0\\n    Building [===> ] 4/9: b\\r' >&2\n";
    std::fs::write(&fake_cargo, script).unwrap();
    std::fs::set_permissions(&fake_cargo, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = d.cmd("t:p1", &["exec", "--name", "bar", "--", fake_cargo.to_str().unwrap(), "build"]).env_remove("CARGO_TERM_PROGRESS_WHEN").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (ok, v) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "10000", "bar"]);
    assert!(ok && v["code"] == 0, "{v}");
    assert_eq!(lines(&d, "bar"), ["   Compiling x v1.0"], "the bar's segments are dropped, the line after one kept");
    let (ok, h) = d.collie("t:p1", &["history", "bar"]);
    let run = &h["runs"][0];
    assert!(ok && run["progress"].is_null() && run.get("steps").is_none(), "{h}");
}

#[test]
fn todos_are_scanned_and_those_a_branch_added_are_new() {
    let d = Daemon::new("problems-todo");
    let repo = d.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    // Apart from the user's and the system's git config, which could sign, hook or rename things.
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs([("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")])
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.rs"), "// TODO: old\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    git(&["checkout", "-qb", "feat"]);
    std::fs::write(repo.join("a.rs"), "// TODO: old\n// FIXME: new\nTODOS lowercase todo\n").unwrap();
    git(&["commit", "-qam", "work"]);
    let todos = |args: &[&str]| {
        let (ok, v) = d.collie_in(&repo, args);
        assert!(ok, "{v}");
        v["items"].as_array().unwrap().iter().map(|i| (i["tag"].as_str().unwrap().to_string(), i["new"].clone())).collect::<Vec<_>>()
    };
    assert_eq!(todos(&["problems", "--rescan"]), [("TODO".to_string(), false.into()), ("FIXME".to_string(), true.into())]);
    assert_eq!(todos(&["problems", "--new"]), [("FIXME".to_string(), true.into())]);
    let (_, v) = d.collie_in(&repo, &["problems"]);
    assert_eq!(v["base"]["ref"], "main", "no origin: the local default branch");
    // A fresh branch, no commits yet, with an uncommitted edit: that edit is new.
    git(&["checkout", "-q", "main"]);
    git(&["checkout", "-qb", "fresh"]);
    std::fs::write(repo.join("a.rs"), "// TODO: old\n// FIXME: wip\n").unwrap();
    assert_eq!(todos(&["problems", "--rescan", "--new"]), [("FIXME".to_string(), true.into())]);
}

#[test]
fn slimming_is_asked_for_a_simulator_only_and_needs_simslim() {
    let d = Daemon::new("slim");
    let code = |v: &Value| v["error"]["code"].clone();
    // The daemon, started by this first call, sees a PATH without simslim (xcrun is in /usr/bin).
    let out = d.cmd("t:p1", &["exec", "--sim", "--slim", "--name", "slim", "--", "true"]).env("PATH", "/usr/bin:/bin").output().unwrap();
    let err: Value = serde_json::from_slice(&out.stderr).unwrap_or_default();
    match code(&err).as_str() {
        Some("no_xcode" | "not_installed") => eprintln!("skipped simslim_missing: no iOS runtime here"),
        _ => assert_eq!(code(&err), "simslim_missing", "{err}"),
    }
    let out = d.cmd("t:p1", &["run", "dev", "--slim", "--stock"]).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("cannot be used with"), "a clap error");
    let (ok, err) = d.collie("t:p1", &["exec", "--physical", "x", "--slim", "--", "true"]);
    assert_eq!((ok, code(&err)), (false, "bad_request".into()), "{err}");
    let (ok, err) = d.collie("t:p1", &["sims", "set", "dev", "--slim", "on"]);
    assert_eq!((ok, code(&err)), (false, "bad_request".into()), "dev runs on no simulator: {err}");
}

#[test]
fn a_destination_is_asked_for_and_only_for_commands() {
    let d = Daemon::new("dest");
    let code = |v: &Value| v["error"]["code"].clone();
    let (ok, err) = d.collie("t:p1", &["exec", "--", "echo", "{udid}"]);
    assert_eq!((ok, code(&err)), (false, "bad_request".into()), "{{udid}} without --sim: {err}");
    let (ok, err) = d.collie("t:p1", &["exec", "--physical", "no-such-device", "--", "true"]);
    assert_eq!((ok, code(&err)), (false, "not_found".into()), "{err}");
    let (ok, err) = d.collie_in(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cargo-tests")), &["run", "--sim", "cargo test"]);
    assert_eq!((ok, code(&err)), (false, "bad_request".into()), "{err}");
}

#[test]
fn a_wait_that_times_out_says_on_stdout_what_still_runs() {
    let d = Daemon::new("wait-still");
    let (ok, started) = d.collie("t:p1", &["exec", "--name", "nap", "--", "sleep", "3"]);
    assert!(ok, "{started}");
    let out = d.cmd("t:p1", &["wait", "--until", "exited", "--timeout", "300", "nap"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("still running") && stdout.contains("elapsed"), "{stdout}");
}

#[test]
fn an_earlier_runs_log_stays_readable_through_history() {
    let d = Daemon::new("history");
    for word in ["first", "second"] {
        let (ok, started) = d.collie("t:p1", &["exec", "--name", "say", "--", "echo", word]);
        assert!(ok, "{started}");
        let (ok, waited) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "10000", "say"]);
        assert!(ok, "{waited}");
    }
    let (ok, history) = d.collie("t:p1", &["history", "say"]);
    assert!(ok, "{history}");
    let ids: Vec<_> = history["runs"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2, "{history}");
    assert_eq!(lines(&d, "say"), ["second"]);
    let out = d.cmd("t:p1", &["logs", "--run", &ids[1]]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"first\""), "{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn a_stopped_run_reaches_the_history() {
    let d = Daemon::new("stopped");
    let (ok, started) = d.collie("t:p1", &["exec", "--name", "nap", "--", "sleep", "30"]);
    assert!(ok, "{started}");
    let (ok, stopped) = d.collie("t:p1", &["stop", "nap"]);
    assert!(ok, "{stopped}");
    let (ok, history) = d.collie("t:p1", &["history", "nap"]);
    assert!(ok, "{history}");
    let runs = history["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{history}");
    assert_eq!((&runs[0]["id"], &runs[0]["state"], &runs[0]["signal"]), (&started["id"], &"crashed".into(), &15.into()), "{history}");
    assert!(runs[0]["ended_at"].is_u64(), "{history}");
}

#[test]
fn a_cargo_test_runs_totals_reach_the_history_and_its_tests_are_listed() {
    let d = Daemon::new("cargo-tests");
    let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cargo-tests"));
    let run = |args: &[&str]| {
        let out = d.cmd("t:p1", args).current_dir(dir).env("CARGO_TARGET_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/target/fixtures")).output().unwrap();
        let text = if out.status.success() { out.stdout } else { out.stderr };
        (out.status.success(), serde_json::from_slice::<Value>(&text).unwrap_or(Value::Null))
    };
    let (ok, started) = run(&["run", "cargo test"]);
    assert!(ok, "{started}");
    let (ok, ended) = run(&["wait", "--until", "exited", "--timeout", "120000", "cargo test"]);
    assert!(ok, "{ended}");
    let (ok, history) = run(&["history", "cargo test"]);
    assert!(ok, "{history}");
    let totals = &history["runs"][0]["tests"];
    assert_eq!((&totals["passed"], &totals["failed"], &totals["skipped"]), (&1.into(), &1.into(), &1.into()), "{history}");
    let (ok, tests) = run(&["tests", "cargo test"]);
    assert!(ok, "{tests}");
    assert_eq!(tests["tests"].as_array().map(Vec::len), Some(3), "{tests}");
    assert_eq!(tests["sep"], "::");
    let (ok, one) = run(&["tests", "--run", started["id"].as_str().unwrap()]);
    assert!(ok && one["totals"] == *totals, "{one}");
    // Another project's `cargo test` is another target.
    let (ok, err) = d.collie("t:p1", &["tests", "cargo test"]);
    assert_eq!((ok, &err["error"]["code"]), (false, &"not_found".into()), "from the node fixture: {err}");
    let (ok, err) = d.collie("t:p1", &["tests", "--run", "../x"]);
    assert_eq!((ok, &err["error"]["code"]), (false, &"bad_request".into()), "{err}");
    // One test only: it passes, and the merged list still holds all three.
    let (ok, started) = run(&["run", "cargo test", "--only", "tests::passes"]);
    assert!(ok, "{started}");
    let (ok, ended) = run(&["wait", "--until", "exited", "--timeout", "120000", "cargo test"]);
    assert_eq!((ok, &ended["state"]), (true, &"exited".into()), "{ended}");
    assert_eq!((&ended["tests"]["passed"], &ended["tests"]["failed"], &ended["only"]), (&1.into(), &0.into(), &true.into()), "{ended}");
    let (_, tests) = run(&["tests", "cargo test"]);
    assert_eq!(tests["tests"].as_array().map(Vec::len), Some(3), "{tests}");
    let (ok, err) = d.collie("t:p1", &["run", "test", "--only", "x"]);
    assert_eq!((ok, &err["error"]["code"]), (false, &"bad_request".into()), "npm: {err}");
}

#[test]
fn a_python_childs_output_arrives_before_it_ends() {
    if Command::new("python3").arg("--version").output().is_err() {
        return eprintln!("skipped: no python3");
    }
    let d = Daemon::new("unbuffered");
    let (ok, started) = d.collie("t:p1", &["exec", "--name", "py", "--", "python3", "-c", "import time; print('a'); time.sleep(5); print('b')"]);
    assert!(ok, "{started}");
    let t0 = std::time::Instant::now();
    while lines(&d, "py").is_empty() && t0.elapsed() < Duration::from_secs(4) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(lines(&d, "py"), ["a"], "'a' while python still sleeps");
}

#[test]
fn exec_runs_in_the_callers_dir_and_env_and_ends_with_its_code() {
    let d = Daemon::new("exec");
    let out = d.cmd("t:p1", &["exec", "--name", "env", "--", "sh", "-c", "echo \"$(pwd -P) $COLLIE_PROBE\"; exit 3"]).env("COLLIE_PROBE", "from-caller").output().unwrap();
    let started: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!((&started["target"], &started["kind"], &started["ended_at"]), (&"env".into(), &"exec".into(), &Value::Null));
    // A second name in the same dir runs alongside; the default name is the command's first two words.
    let (ok, other) = d.collie("t:p2", &["exec", "--", "sleep", "0.2"]);
    assert!(ok, "{other}");
    assert_eq!(other["target"], "sleep 0.2");
    let (ok, ended) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "15000", "env"]);
    assert!(ok, "{ended}");
    assert_eq!((&ended["state"], &ended["code"]), (&"crashed".into(), &3.into()), "{ended}");
    assert!(ended["ended_at"].as_u64().unwrap() >= ended["since"].as_u64().unwrap(), "{ended}");
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/node-app");
    assert_eq!(lines(&d, "env"), [format!("{dir} from-caller")]);
    let (ok, done) = d.collie("t:p2", &["wait", "--until", "exited", "--timeout", "15000", "sleep 0.2"]);
    assert_eq!((ok, &done["state"], &done["code"]), (true, &"exited".into(), &0.into()), "{done}");
    let (ok, err) = d.collie("t:p1", &["exec", "--name", "sleep 0.2", "--", "true"]);
    assert!(ok, "an ended process holds nothing: {err}");
}

#[test]
fn database_dibs_neither_queue_nor_take_a_preferred_port() {
    let d = Daemon::new("db-dibs");
    for extra in [["--preferred", "3000"], ["--wait", "--timeout"]] {
        let mut args = vec!["dibs", "acquire", "postgres"];
        args.extend(extra);
        if extra[0] == "--wait" {
            args.push("1000");
        }
        let (ok, err) = d.collie("t:p1", &args);
        assert_eq!((ok, &err["error"]["code"]), (false, &"bad_request".into()), "{args:?}: {err}");
    }
}

#[test]
fn an_exec_past_its_kill_limit_ends_timed_out() {
    let d = Daemon::new("timeout");
    let t0 = std::time::Instant::now();
    // Ignores the TERM, so it lives on through kill_group's 3 s grace until the KILL.
    let (ok, started) = d.collie("t:p1", &["exec", "--timeout", "300", "--name", "hang", "--", "sh", "-c", "trap '' TERM; sleep 30"]);
    assert!(ok, "{started}");
    // wait's own --timeout only bounds the waiting; the kill limit ends the process.
    let (ok, ended) = d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "10000", "hang"]);
    assert!(ok, "{ended}");
    assert!(t0.elapsed() < Duration::from_secs(6), "{:?}", t0.elapsed());
    // `exited` returns only once it is really gone: ended_at set, killed by the KILL after the ignored TERM.
    assert_eq!((&ended["state"], &ended["signal"], &ended["pid"]), (&"timed-out".into(), &9.into(), &Value::Null), "{ended}");
    assert!(ended["ended_at"].as_u64().unwrap() - ended["since"].as_u64().unwrap() >= 3000, "{ended}");
    let (ok, err) = d.collie("t:p1", &["wait", "--until", "crashed", "--timeout", "1000", "hang"]);
    assert_eq!((ok, &err["error"]["code"]), (false, &"ended".into()), "timed-out is no crash: {err}");
    assert!(lines(&d, "hang").iter().any(|l| l.starts_with("collie: killed: its run phase outlived its kill limit of 300 ms")));
}

#[test]
fn run_args_reach_the_script_and_an_app_refuses_them() {
    let d = Daemon::new("run-args");
    let (ok, run) = d.collie("t:p1", &["run", "lint", "--", "--fix", "a b"]);
    assert!(ok, "{run}");
    d.collie("t:p1", &["wait", "--until", "exited", "--timeout", "30000", "lint"]);
    assert!(lines(&d, "lint").contains(&"lint ok --fix a b".into()), "{:?}", lines(&d, "lint"));
    let xcode = d.cmd("t:p1", &["run", "My App", "--", "-x"]).current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/xcode-ws")).output().unwrap();
    let err: Value = serde_json::from_slice(&xcode.stderr).unwrap();
    assert_eq!(err["error"]["code"], "bad_request", "{err}");
}

#[test]
fn follow_moves_on_to_the_restarted_process() {
    let d = Daemon::new("follow");
    let (ok, first) = d.collie("t:p1", &["run", "dev"]);
    assert!(ok, "{first}");
    let mut follow = d.cmd("t:p1", &["logs", "--follow", "dev"]).stdout(Stdio::piped()).spawn().unwrap();
    let (tx, rx) = mpsc::channel();
    let out = follow.stdout.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let seen = |want: &str| loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(l) if l.contains(want) => return true,
            Ok(l) => assert!(!l.contains("listening on"), "{want} was skipped: {l}"),
            Err(_) => return false,
        }
    };
    assert!(seen("listening on"), "the first process's start");
    assert!(d.collie("t:p1", &["run", "dev"]).0, "same holder restarts it");
    // The old process's last line comes after the new process is registered, and must still be followed.
    assert!(seen("shutting down"), "the old process's log is read to its end");
    assert!(seen("listening on"), "follow streams the restarted process");
    let _ = follow.kill();
    let _ = follow.wait();
}

fn status(d: &Daemon) -> Value {
    d.collie("t:p0", &["status"]).1
}

/// `target`'s newest process in a `status`, or null.
fn proc_of(status: &Value, target: &str) -> Value {
    status["processes"].as_array().and_then(|ps| ps.iter().rfind(|p| p["target"] == target)).cloned().unwrap_or_default()
}

/// Polls `f` for up to 20 s: the remote is read every 5 s.
fn until(what: &str, f: impl Fn() -> bool) {
    let t0 = std::time::Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(20), "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A "laptop" daemon A and a "mini" daemon B on this machine, one build slot each; A's `[remote]` is B, with
/// `<A>/work` here being `<B>/mirror` there. Returns once A has read B's slots.
fn pair(name: &str) -> (Daemon, Daemon) {
    let a = Daemon::new(&format!("{name}-a"));
    let b = Daemon::beside(&format!("{name}-b"));
    std::fs::write(b.0.join("config.toml"), "[builds]\nmax = 1\n").unwrap();
    let (sock, work, mirror) = (b.0.join("collie.sock"), a.0.join("work"), b.0.join("mirror"));
    let cfg = format!("[builds]\nmax = 1\n\n[remote]\nsocket = {sock:?}\n\n[remote.paths]\n{work:?} = {mirror:?}\n");
    std::fs::write(a.0.join("config.toml"), cfg).unwrap();
    let (ok, v) = b.collie("t:b", &["status"]);
    assert!(ok, "B starts: {v}");
    until("A reads B's slots", || {
        let r = &status(&a)["builds"]["remote"];
        r["error"].is_null() && r["cap"] == 1
    });
    (a, b)
}

/// `<A>/work/<name>`, a git repo of its own (a worktree is synced from its git root), with `fixture`'s files.
fn work_repo(a: &Daemon, name: &str, fixture: Option<&str>) -> PathBuf {
    let dir = a.0.join("work").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(f) = fixture {
        let src = format!("{}/tests/fixtures/{f}/.", env!("CARGO_MANIFEST_DIR"));
        assert!(Command::new("cp").args(["-R", &src]).arg(&dir).status().unwrap().success());
    }
    let git = Command::new("git").args(["init", "-q"]).current_dir(&dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").status();
    assert!(git.unwrap().success());
    dir
}

#[test]
fn a_run_without_a_slot_here_runs_on_the_remote_and_reports_back() {
    let (a, b) = pair("offload");
    let repo = work_repo(&a, "proj", None);
    let run = |args: &[&str]| a.collie_in(&repo, args);
    let (ok, v) = run(&["exec", "--name", "hold", "--", "sleep", "60"]);
    assert!(ok, "{v}");
    // The remote's own state dir: the forwarded env drops this machine's.
    let (ok, v) = run(&["exec", "--name", "test-x", "--", "sh", "-c", "pwd; echo \"$COLLIE_STATE_DIR\""]);
    assert!(ok, "{v}");
    assert_eq!(v["remote"]["name"], "remote", "{v}");
    let rid = v["remote"]["id"].as_str().expect("a remote process id").to_string();
    let (ok, ended) = run(&["wait", "--until", "exited", "--timeout", "20000", "test-x"]);
    assert_eq!((ok, &ended["state"], &ended["code"]), (true, &"exited".into(), &0.into()), "{ended}");
    let there = &b.collie("t:p0", &["history", "--all", "test-x"]).1["runs"][0];
    assert_eq!((&there["id"], &there["dir"]), (&rid.as_str().into(), &b.0.join("mirror/proj").to_str().unwrap().into()), "{there}");
    assert!(there["holder"].as_str().unwrap().starts_with("t:p1@"), "held by A's holder at A's host: {there}");
    let out = lines(&a, "test-x");
    assert!(out.contains(&repo.display().to_string()), "B's path mapped back to A's: {out:?}");
    assert!(out.contains(&b.0.display().to_string()), "it ran on B: {out:?}");
    let (_, history) = run(&["history", "test-x"]);
    assert_eq!(history["runs"][0]["remote"]["id"], rid.as_str(), "{history}");

    // Live: A shows B's slot taken; a stop here stops it there.
    let (ok, v) = run(&["exec", "--name", "test-z", "--", "sleep", "60"]);
    assert!(ok && v["remote"]["id"].is_string(), "{v}");
    until("B runs test-z", || proc_of(&status(&b), "test-z")["state"] == "running");
    until("A sees B's slot taken", || status(&a)["builds"]["remote"]["running"] == 1);
    assert!(run(&["stop", "test-z"]).0);
    until("B stopped test-z", || proc_of(&status(&b), "test-z").is_null());
    until("A sees B's slot free", || status(&a)["builds"]["remote"]["running"] == 0);

    // --local waits here although B is free; --remote goes there although a slot here is free.
    let (ok, v) = run(&["exec", "--local", "--name", "test-l", "--", "true"]);
    assert!(ok && v["remote"].is_null(), "{v}");
    until("test-l queues here", || proc_of(&status(&a), "test-l")["awaits_slot"] == true);
    assert!(run(&["stop", "hold"]).0);
    let (ok, ended) = run(&["wait", "--until", "exited", "--timeout", "15000", "test-l"]);
    assert!(ok && ended["remote"].is_null(), "{ended}");
    let (ok, v) = run(&["exec", "--remote", "--name", "anything", "--", "true"]);
    assert!(ok && v["remote"]["id"].is_string(), "{v}");
}

#[test]
fn an_offloaded_cargo_test_reports_its_tests_here_unless_kept_here() {
    let (a, _b) = pair("offload-tests");
    let repo = work_repo(&a, "cargo", Some("cargo-tests"));
    let run = |args: &[&str]| a.collie_in(&repo, args);
    assert!(run(&["exec", "--name", "hold", "--", "sleep", "300"]).0);
    let (ok, v) = run(&["run", "cargo test"]);
    assert!(ok && v["remote"]["id"].is_string(), "{v}");
    let (ok, ended) = run(&["wait", "--until", "exited", "--timeout", "180000", "cargo test"]);
    assert!(ok, "{ended}");
    let totals = &ended["tests"];
    assert_eq!((&totals["passed"], &totals["failed"], &totals["skipped"]), (&1.into(), &1.into(), &1.into()), "{ended}");
    let (ok, tests) = run(&["tests", "cargo test"]);
    assert!(ok, "{tests}");
    assert_eq!(tests["tests"].as_array().map(Vec::len), Some(3), "{tests}");
    std::fs::write(repo.join("collie.toml"), "[targets.\"cargo test\"]\noffload = false\n").unwrap();
    let (ok, v) = run(&["run", "cargo test"]);
    assert!(ok && v["remote"].is_null(), "{v}");
    until("it queues here", || proc_of(&status(&a), "cargo test")["awaits_slot"] == true);
}

#[test]
fn a_queued_process_moves_to_the_remote_once_it_frees_up() {
    let (a, b) = pair("reroute");
    let repo = work_repo(&a, "proj", None);
    let run = |args: &[&str]| a.collie_in(&repo, args);
    let (ok, v) = b.collie_in(&b.0, &["exec", "--name", "busy", "--", "sleep", "60"]);
    assert!(ok, "{v}");
    until("A sees B busy", || status(&a)["builds"]["remote"]["running"] == 1);
    assert!(run(&["exec", "--name", "hold", "--", "sleep", "60"]).0);
    let (ok, v) = run(&["exec", "--name", "test-y", "--", "sh", "-c", "echo \"$COLLIE_STATE_DIR\""]);
    assert!(ok && v["remote"].is_null(), "B is busy too: it queues here: {v}");
    until("test-y queues here", || proc_of(&status(&a), "test-y")["awaits_slot"] == true);
    assert!(b.collie_in(&b.0, &["stop", "busy"]).0);
    let (ok, ended) = run(&["wait", "--until", "exited", "--timeout", "20000", "test-y"]);
    assert_eq!((ok, &ended["state"]), (true, &"exited".into()), "{ended}");
    assert!(ended["remote"]["id"].is_string(), "{ended}");
    let out = lines(&a, "test-y");
    let at = out.iter().position(|l| l.starts_with("collie: moved to remote")).unwrap_or_else(|| panic!("a moved record: {out:?}"));
    assert!(out[at..].contains(&b.0.display().to_string()), "it ran on B: {out:?}");
    assert!(!out.contains(&a.0.display().to_string()), "and never here: {out:?}");
}

#[test]
fn without_the_remote_runs_stay_here_and_say_why() {
    let (a, b) = pair("fallback");
    let repo = work_repo(&a, "proj", None);
    let run = |args: &[&str]| a.collie_in(&repo, args);
    assert!(b.collie("t:p0", &["daemon", "stop"]).0);
    until("A notices B is gone", || status(&a)["builds"]["remote"]["error"].as_str().is_some_and(|e| e.starts_with("unreachable")));
    assert!(run(&["exec", "--name", "hold", "--", "sleep", "60"]).0);
    let (ok, v) = run(&["exec", "--name", "test-f", "--", "true"]);
    assert!(ok && v["remote"].is_null(), "{v}");
    until("test-f queues here", || proc_of(&status(&a), "test-f")["awaits_slot"] == true);
    assert!(lines(&a, "test-f").iter().any(|l| l.contains("takes no work now (unreachable")), "{:?}", lines(&a, "test-f"));
    let (ok, err) = run(&["exec", "--remote", "--name", "test-g", "--", "true"]);
    assert_eq!((ok, &err["error"]["code"]), (false, &"unavailable".into()), "{err}");
}

#[test]
fn a_run_the_remote_refuses_runs_here_with_the_reason() {
    let (a, b) = pair("refused");
    let repo = work_repo(&a, "proj", None);
    let run = |args: &[&str]| a.collie_in(&repo, args);
    // Someone's own files where the copy would go: no copy of collie's, so it never syncs over them.
    let theirs = b.0.join("mirror/proj");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(theirs.join("notes.txt"), "mine").unwrap();
    assert!(run(&["exec", "--name", "hold", "--", "sleep", "60"]).0);
    let (ok, v) = run(&["exec", "--name", "test-r", "--", "true"]);
    assert!(ok && v["remote"].is_null(), "{v}");
    let out = lines(&a, "test-r");
    assert!(out.iter().any(|l| l.starts_with("collie: couldn't run on remote") && l.contains("no copy of collie's") && l.ends_with("runs here")), "{out:?}");
    // It stays here, although B is idle: it doesn't try again every sample.
    until("test-r queues here", || proc_of(&status(&a), "test-r")["awaits_slot"] == true);
    std::thread::sleep(Duration::from_secs(6));
    assert!(proc_of(&status(&a), "test-r")["remote"].is_null());
    assert!(run(&["stop", "hold"]).0);
    let (ok, ended) = run(&["wait", "--until", "exited", "--timeout", "15000", "test-r"]);
    assert!(ok && ended["code"] == 0 && ended["remote"].is_null(), "{ended}");
    assert_eq!(std::fs::read_dir(&theirs).unwrap().count(), 1, "their dir is as it was");
}

#[test]
fn stopping_the_daemon_here_stops_its_runs_on_the_remote() {
    let (a, b) = pair("stop-daemon");
    let repo = work_repo(&a, "proj", None);
    let (ok, v) = a.collie_in(&repo, &["exec", "--remote", "--name", "test-long", "--", "sleep", "300"]);
    assert!(ok && v["remote"]["id"].is_string(), "{v}");
    until("B runs test-long", || proc_of(&status(&b), "test-long")["state"] == "running");
    assert!(a.collie("t:p0", &["daemon", "stop"]).0);
    until("B stopped test-long", || proc_of(&status(&b), "test-long").is_null());
    let there = &b.collie("t:p0", &["history", "--all", "test-long"]).1["runs"][0];
    assert_eq!(there["state"], "crashed", "stopped, not ended on its own: {there}");
}

#[test]
fn a_worktree_removed_here_is_deleted_on_the_remote_unless_something_runs_there() {
    let (a, b) = pair("cleanup");
    let (gone, busy) = (work_repo(&a, "gone", None), work_repo(&a, "busy", None));
    let (ok, v) = a.collie_in(&gone, &["exec", "--remote", "--name", "test-a", "--", "true"]);
    assert!(ok && v["remote"]["id"].is_string(), "{v}");
    let (ok, v) = a.collie_in(&gone, &["wait", "--until", "exited", "--timeout", "20000", "test-a"]);
    assert!(ok, "{v}");
    let (ok, v) = a.collie_in(&busy, &["exec", "--remote", "--name", "test-b", "--", "sleep", "60"]);
    assert!(ok && v["remote"]["id"].is_string(), "{v}");
    let ledger = || std::fs::read_to_string(a.0.join("remote.json")).unwrap_or_default();
    assert!(ledger().contains(gone.to_str().unwrap()) && ledger().contains(busy.to_str().unwrap()), "{}", ledger());
    assert!(b.0.join("mirror/gone").is_dir() && b.0.join("mirror/busy").is_dir());
    std::fs::remove_dir_all(&gone).unwrap();
    std::fs::remove_dir_all(&busy).unwrap();
    until("B's copy is deleted", || !b.0.join("mirror/gone").exists());
    until("the ledger forgets it", || !ledger().contains(gone.to_str().unwrap()));
    // Two samples more: test-b still runs in its copy.
    std::thread::sleep(Duration::from_secs(11));
    assert!(b.0.join("mirror/busy").is_dir() && ledger().contains(busy.to_str().unwrap()), "{}", ledger());
}
