mod daemon;
mod eco;
mod logs;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use daemon::{Coded, coded};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

#[derive(Parser)]
#[command(name = "collie", version, about = "Run, watch and stop project processes shared by a human and coding agents")]
struct Cli {
    /// Machine-readable output; streams are NDJSON
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the daemon if it is not running (idempotent)
    Daemon {
        #[arg(long)]
        foreground: bool,
        #[command(subcommand)]
        action: Option<DaemonCmd>,
    },
    /// Targets in the current directory's repo, with ecosystem and declared needs
    Targets,
    /// Call dibs on what the target needs, build it (if it has a build step) and launch it
    Run {
        target: String,
        /// Take the target over from another holder, stopping their process
        #[arg(long)]
        take: bool,
        /// Simulator device type (id or name) for this run only; the stored choice is unchanged
        #[arg(long)]
        device: Option<String>,
        /// Simulator runtime (id, name or version, e.g. 26.5) for this run only
        #[arg(long)]
        runtime: Option<String>,
        /// Kill limit, milliseconds: STOPS the process (state `timed-out`) if its run phase (not the build) runs longer;
        /// 0 = none. Default: none, 30 min for test targets. Unlike `wait --timeout`, which only stops waiting
        #[arg(long)]
        timeout: Option<u64>,
        /// Run only this test or group (an id from `collie tests`; a group's ends in `::` or `/`); repeatable.
        /// Cargo and Xcode test targets only
        #[arg(long)]
        only: Vec<String>,
        /// Tasks only: lease a fresh simulator (booted) for this run; its UDID replaces `{udid}` in the args and is
        /// $COLLIE_UDID
        #[arg(long, conflicts_with = "physical")]
        sim: bool,
        /// Tasks only: lease this physical device (UDID or name), as --sim does a simulator
        #[arg(long)]
        physical: Option<String>,
        /// Fail with `busy` instead of queueing while someone else holds the physical device
        #[arg(long)]
        no_queue: bool,
        /// Slim this run's simulator with simslim (a new one takes about a minute more): `--slim` everything,
        /// `--slim=icloud,web` all but those categories; over the target's stored setting (`sims set --slim`)
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "on", conflicts_with = "stock")]
        slim: Option<String>,
        /// This run on a stock simulator, over the target's stored setting
        #[arg(long)]
        stock: bool,
        /// Appended to the run phase's command (not the build's); refused for iOS apps
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run a one-off command in this directory with your env, as a process like any target's
    Exec {
        /// Process name for stop, wait and logs (default: the command's first two words)
        #[arg(long)]
        name: Option<String>,
        /// Take the name over from another holder, stopping their process
        #[arg(long)]
        take: bool,
        /// Kill limit, milliseconds: STOPS the command (state `timed-out`) if it runs longer; default none. Unlike
        /// `wait --timeout`, which only stops waiting
        #[arg(long)]
        timeout: Option<u64>,
        /// Lease a fresh simulator (booted) for the command; its UDID replaces `{udid}` in it and is $COLLIE_UDID.
        /// A rerun of the same --name gets the same simulator back until `collie stop <name>`
        #[arg(long, conflicts_with = "physical")]
        sim: bool,
        /// Simulator device type for --sim (id or name)
        #[arg(long)]
        device: Option<String>,
        /// Simulator runtime for --sim (id, name or version, e.g. 27.0)
        #[arg(long)]
        runtime: Option<String>,
        /// Lease this physical device (UDID or name) for the command, as --sim does a simulator
        #[arg(long)]
        physical: Option<String>,
        /// Fail with `busy` instead of queueing while someone else holds the physical device
        #[arg(long)]
        no_queue: bool,
        /// Slim --sim's simulator with simslim (about a minute more): `--slim` everything, `--slim=icloud,web` all but
        /// those categories
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "on", conflicts_with = "stock")]
        slim: Option<String>,
        /// --sim's simulator stock (the default for exec)
        #[arg(long)]
        stock: bool,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Installed iOS runtimes and the device types each supports, and physical iOS devices
    Sims {
        #[command(subcommand)]
        cmd: Option<SimsCmd>,
    },
    /// Stop <target>, or every process held by this caller
    Stop { target: Option<String> },
    /// Processes and dibs
    Status,
    /// Log records of a target's latest process (or of --run); without --since, the last 200
    Logs {
        /// Only records after this cursor (from --mark or a previous call); ends with a new cursor
        #[arg(long)]
        since: Option<String>,
        /// An earlier run's id, from `collie history`, instead of the latest
        #[arg(long, conflicts_with_all = ["since", "follow"])]
        run: Option<String>,
        #[arg(long)]
        follow: bool,
        /// Minimum level
        #[arg(long, value_parser = ["error", "warn", "info", "debug"])]
        level: Option<String>,
        #[arg(long, value_parser = ["build", "stdout", "stderr"])]
        source: Option<String>,
        /// Print a cursor for --since and nothing else
        #[arg(long)]
        mark: bool,
        /// Every record of the run, not just the last 200
        #[arg(long, conflicts_with_all = ["since", "follow", "mark"])]
        all: bool,
        target: Option<String>,
    },
    /// Ended runs, newest first, of this worktree (every worktree with --all), of <target> when given. Their logs are
    /// kept for 7 days at most: read one with `logs --run <id>`
    History {
        target: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 20)]
        limit: u64,
    },
    /// This worktree's problems: build warnings, errors and lint from its targets' runs, and TODO/FIXME/HACK comments.
    /// Display only: the exit code never depends on them
    Problems {
        /// Only those this branch added (on lines added since it left the default branch)
        #[arg(long)]
        new: bool,
        /// Scan the TODOs again
        #[arg(long)]
        rescan: bool,
    },
    /// Per-test results of <target>'s ended test runs (the target in the dir nearest above cwd), each test's newest;
    /// or of one run (--run).
    /// Cargo and Xcode test targets only
    Tests {
        target: Option<String>,
        /// A run's id (from `history`)
        #[arg(long)]
        run: Option<String>,
    },
    /// Block until the target's latest process reaches a state
    Wait {
        /// `exited`: any end (exited, crashed, timed-out); `crashed`: a nonzero exit or signal, not `timed-out`
        #[arg(long, value_parser = ["built", "running", "launched", "exited", "crashed", "port-open"])]
        until: String,
        /// Give up waiting after this many milliseconds (code `timeout`); the process keeps running. Never kills it
        #[arg(long)]
        timeout: Option<u64>,
        target: Option<String>,
    },
    /// Call dibs on a port, simulator or device
    #[command(name = "dibs")]
    Lease {
        #[command(subcommand)]
        cmd: LeaseCmd,
    },
    /// The pane UI
    Tui,
    /// Print instructions for coding agents (Markdown)
    Skill,
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Stop the daemon and every process it runs
    Stop,
}

#[derive(Subcommand)]
enum SimsCmd {
    /// Store the simulator kind or physical device <target> runs on, for every worktree of this repo
    Set {
        target: String,
        /// Device type, id or name
        #[arg(long)]
        device: Option<String>,
        /// Runtime, id, name or version
        #[arg(long)]
        runtime: Option<String>,
        /// Back to the default (newest runtime, its first iPhone, headless)
        #[arg(long)]
        reset: bool,
        /// Open the device in DeviceHub when it runs (default off: headless)
        #[arg(long, value_parser = ["on", "off"])]
        head: Option<String>,
        /// Run on this physical device (UDID or name) instead; --device or --runtime switches back to the simulator
        #[arg(long)]
        physical: Option<String>,
        /// Slim its simulators with simslim by default: `on`, `off` (stock, the default), or the categories to keep
        /// (`icloud,web,store`; see `simslim profiles`)
        #[arg(long)]
        slim: Option<String>,
    },
    /// Save your simulator of <target> (stopping its process) as the template every new simulator of the same kind
    /// for <target> is cloned from, in every worktree of this repo; replaces an older one
    Template {
        target: String,
        /// Delete the template instead
        #[arg(long)]
        rm: bool,
    },
}

#[derive(Subcommand)]
enum LeaseCmd {
    /// A port, a fresh simulator device (deleted on release), a physical device, or a throwaway database container
    /// (postgres, redis; deleted on release) whose `service.url` the result gives
    Acquire {
        #[arg(value_parser = ["port", "simulator", "device", "postgres", "redis"])]
        resource: String,
        /// Docker image for postgres or redis (default postgres:17, redis:7)
        #[arg(long)]
        image: Option<String>,
        #[arg(long)]
        preferred: Option<u16>,
        /// Queue until the resource (with --preferred: exactly that port) is free
        #[arg(long)]
        wait: bool,
        /// Milliseconds to wait: for --wait, for the grant; for postgres/redis, for a pull and then readiness (default 120000)
        #[arg(long)]
        timeout: Option<u64>,
        /// Simulator device type, id or name; for `device`, the physical device's UDID or name
        #[arg(long)]
        device: Option<String>,
        /// Simulator runtime, id, name or version
        #[arg(long)]
        runtime: Option<String>,
    },
    Release { id: u64 },
    /// Extend its TTL (2h) from now
    Renew { id: u64 },
    List,
}

fn main() {
    let cli = Cli::parse();
    let res = run(&cli);
    if let Err(e) = &res {
        let code = e.downcast_ref::<Coded>().map_or("error", |c| &c.0);
        if cli.json {
            eprintln!("{}", json!({"error": {"code": code, "message": format!("{e:#}")}}));
        } else {
            eprintln!("error: {e:#}");
        }
    }
    // After the result, so `--json` stderr still starts with the error object; the pane shows it itself.
    if let Some(w) = STALE.lock().unwrap().take().filter(|_| !matches!(cli.cmd, Cmd::Tui)) {
        if cli.json {
            eprintln!("{}", json!({"warning": w}));
        } else {
            eprintln!("warning: {w}");
        }
    }
    if res.is_err() {
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<()> {
    let out = |v: Value| print(cli.json, &v);
    match &cli.cmd {
        Cmd::Daemon { action: Some(DaemonCmd::Stop), .. } => match UnixStream::connect(daemon::socket_path()) {
            Ok(_) => {
                let stopped = request("daemon.stop", json!({}))?;
                daemon::wait_gone();
                out(stopped)
            }
            Err(_) => out(json!({"stopped": [], "running": false})),
        },
        Cmd::Daemon { foreground, .. } => {
            daemon::start(*foreground)?;
            if !foreground {
                out(json!({"running": true, "socket": daemon::socket_path()}));
            }
        }
        Cmd::Targets => out(request("targets", json!({}))?),
        Cmd::Run { target, take, device, runtime, timeout, only, sim, physical, no_queue, slim, stock, args } => {
            let env: HashMap<_, _> = std::env::vars().collect();
            let p = json!({"target": target, "take": take, "env": env, "device": device, "runtime": runtime, "args": args, "timeout_ms": timeout, "only": only,
                "simulator": sim, "physical": physical, "queue": !no_queue, "slim": slim_param(slim, *stock)});
            out(request("run", p)?)
        }
        Cmd::Exec { name, take, timeout, sim, device, runtime, physical, no_queue, slim, stock, cmd } => {
            let env: HashMap<_, _> = std::env::vars().collect();
            let p = json!({"name": name, "take": take, "env": env, "args": cmd, "timeout_ms": timeout,
                "simulator": sim, "device": device, "runtime": runtime, "physical": physical, "queue": !no_queue, "slim": slim_param(slim, *stock)});
            out(request("exec", p)?)
        }
        Cmd::Sims { cmd: Some(SimsCmd::Set { target, device, runtime, reset, head, physical, slim }) } => {
            let head = head.as_ref().map(|h| h == "on");
            let p = json!({"target": target, "device": device, "runtime": runtime, "reset": reset, "head": head, "physical": physical, "slim": slim});
            out(request("sim.set", p)?)
        }
        Cmd::Sims { cmd: Some(SimsCmd::Template { target, rm }) } => {
            // The env picks the Xcode (DEVELOPER_DIR) the clone uses, as for `run`.
            let env: HashMap<_, _> = std::env::vars().collect();
            out(request("sim.template", json!({"target": target, "rm": rm, "env": env}))?)
        }
        Cmd::Sims { cmd: None } => {
            let v = request("sims", json!({}))?;
            if cli.json {
                println!("{v}");
                return Ok(());
            }
            for rt in v["runtimes"].as_array().into_iter().flatten() {
                let devices: Vec<_> = rt["devices"].as_array().into_iter().flatten().filter_map(|d| d["name"].as_str()).collect();
                println!("{}: {}", rt["name"].as_str().unwrap_or_default(), devices.join(", "));
            }
            let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
            for d in v["physical"].as_array().into_iter().flatten() {
                let off = if d["developer_mode"] == false { " · Developer Mode off" } else { "" };
                println!("physical: {} · {} · {} · {}{off}  {}", s(&d["name"]), s(&d["model"]), s(&d["os"]), s(&d["state"]), s(&d["udid"]));
            }
            if let Some(e) = v["physical_error"].as_str() {
                println!("physical: {e}");
            }
            for t in v["templates"].as_array().into_iter().flatten() {
                println!("template: {} · {} · {}  {}  ({})", s(&t["target"]), s(&t["device"]), s(&t["runtime"]), s(&t["udid"]), s(&t["repo"]));
            }
        }
        Cmd::Stop { target } => out(request("stop", json!({"target": target}))?),
        Cmd::Status => out(request("status", json!({}))?),
        Cmd::History { target, all, limit } => {
            let v = request("history", json!({"target": target, "here": !all, "limit": limit}))?;
            if cli.json {
                out(v);
                return Ok(());
            }
            let now = daemon::now_ms();
            for r in v["runs"].as_array().into_iter().flatten() {
                let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
                let code = r["code"].as_i64().map(|c| format!("({c})")).or(r["signal"].as_i64().map(|s| format!("(sig {s})"))).unwrap_or_default();
                let (since, end) = (r["since"].as_u64().unwrap_or_default(), r["ended_at"].as_u64().unwrap_or(now));
                let took = tui::ago(end.saturating_sub(since));
                let dir = if *all { format!("  {}", s("dir")) } else { String::new() };
                println!("{}  {}{code}  ended {} ago  took {took}{dir}", s("id"), s("state"), tui::ago(now.saturating_sub(end)));
            }
        }
        Cmd::Problems { new, rescan } => {
            let v = request("problems", json!({"new": new, "rescan": rescan}))?;
            if cli.json {
                out(v);
                return Ok(());
            }
            // In the compiler's shape, so it greps and opens in editors.
            for i in v["items"].as_array().into_iter().flatten() {
                let s = |k: &str| i[k].as_str().unwrap_or_default().to_string();
                let col = i["col"].as_u64().map_or(String::new(), |c| format!(":{c}"));
                let kind = if s("kind") == "todo" { s("tag") } else { s("kind") };
                let from: Vec<String> = i["sources"].as_array().into_iter().flatten().filter_map(|s| s["target"].as_str().map(String::from)).collect();
                let from = if from.is_empty() { String::new() } else { format!("  [{}]", from.join(", ")) };
                let new = if i["new"] == true { " (new)" } else { "" };
                println!("{}:{}{col}: {kind}: {}{from}{new}", s("path"), i["line"], s("message"));
            }
        }
        Cmd::Tests { target, run } => {
            let v = request("tests", json!({"target": target, "process": run}))?;
            if cli.json {
                out(v);
                return Ok(());
            }
            for t in v["tests"].as_array().into_iter().flatten() {
                let mark = match t["status"].as_str() {
                    Some("passed") => "✓",
                    Some("skipped") => "⊘",
                    _ => "✗",
                };
                let took = t["duration_ms"].as_u64().map_or(String::new(), |ms| format!("  {ms}ms"));
                println!("{mark} {}{took}", t["id"].as_str().unwrap_or_default());
                let f = &t["failure"];
                if f.is_object() {
                    let line = f["line"].as_u64().map_or(String::new(), |l| format!(":{l}"));
                    let at = f["file"].as_str().map_or(String::new(), |file| format!("{file}{line}  "));
                    println!("    {at}{}", f["message"].as_str().unwrap_or_default().replace('\n', " "));
                }
            }
        }
        Cmd::Logs { since, run, follow, level, source, mark, all, target } => {
            let p = json!({"since": since, "process": run, "follow": follow, "level": level, "source": source, "mark": mark, "all": all, "target": target});
            if *follow {
                for frame in send("logs", p)? {
                    print_record(cli.json, &frame?["data"]);
                }
                return Ok(());
            }
            let v = request("logs", p)?;
            if let (Some(old), Some(latest)) = (v["stale"]["process"].as_str(), v["stale"]["latest"].as_str()) {
                let w = format!("cursor is for ended run {old}; latest is {latest} (collie logs --all --run {latest} reads it)");
                if cli.json {
                    eprintln!("{}", json!({"warning": w}));
                } else {
                    eprintln!("warning: {w}");
                }
            }
            let cursor = v["cursor"].as_str().unwrap_or_default();
            if !mark {
                for r in v["records"].as_array().into_iter().flatten() {
                    print_record(cli.json, r);
                }
            }
            match (cli.json, mark) {
                (true, _) => println!("{}", json!({"cursor": cursor})),
                (false, true) => println!("{cursor}"),
                (false, false) => println!("cursor: {cursor}"),
            }
        }
        Cmd::Wait { until, timeout, target } => match request("wait", json!({"until": until, "timeout_ms": timeout, "target": target})) {
            Ok(v) => out(v),
            // Also on stdout, where agents read the result; the error still goes to stderr and exits 1.
            Err(e) => {
                if let Some(c) = e.downcast_ref::<Coded>().filter(|c| c.0 == "timeout") {
                    match cli.json {
                        true => println!("{}", json!({"still": c.1})),
                        false => println!("{}", c.1),
                    }
                }
                return Err(e);
            }
        },
        Cmd::Lease { cmd } => out(match cmd {
            LeaseCmd::Acquire { resource, image, preferred, wait, timeout, device, runtime } => request(
                "lease.acquire",
                json!({"resource": resource, "image": image, "preferred": preferred, "wait": wait, "timeout_ms": timeout, "device": device, "runtime": runtime}),
            )?,
            LeaseCmd::Release { id } => request("lease.release", json!({"id": id}))?,
            LeaseCmd::Renew { id } => request("lease.renew", json!({"id": id}))?,
            LeaseCmd::List => request("lease.list", json!({}))?,
        }),
        Cmd::Tui => tui::run()?,
        Cmd::Skill => print!("{}", include_str!("skill.md")),
    }
    Ok(())
}

/// `run`'s `slim` from `--slim[=<categories to keep>]` and `--stock`: null for neither (the stored setting applies).
fn slim_param(slim: &Option<String>, stock: bool) -> Value {
    match slim {
        _ if stock => false.into(),
        Some(keep) if keep.trim() == "off" => false.into(),
        Some(keep) => keep.split(',').map(str::trim).filter(|c| !c.is_empty() && *c != "on").collect::<Vec<_>>().into(),
        None => Value::Null,
    }
}

pub fn holder() -> String {
    holder_from(|k| std::env::var(k).ok(), std::os::unix::process::parent_id())
}

/// The herdr pane first: herdr's pane-close release looks holders up by pane id, so an override mustn't hide it.
/// Claude Code runs each Bash call in a fresh shell, so its session id, not the parent shell, keeps one holder.
// ponytail: other agent harnesses that spawn a fresh shell per call fall back to `host:<ppid>`; they can set COLLIE_HOLDER.
// CLAUDE_CODE_SESSION_ID is observed, not a documented Claude Code contract: without it, holders fall back to `host:<ppid>` (per call).
fn holder_from(env: impl Fn(&str) -> Option<String>, ppid: u32) -> String {
    let set = |k| env(k).filter(|v: &String| !v.is_empty());
    env("HERDR_PANE_ID")
        .or_else(|| set("COLLIE_HOLDER"))
        .or_else(|| set("CLAUDE_CODE_SESSION_ID").map(|s| format!("claude:{s}")))
        .unwrap_or_else(|| format!("host:{ppid}"))
}

/// Set by the last response when the daemon that sent it runs an older build than this binary: what to tell the user.
pub static STALE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The warning for a daemon built at `daemon` (Unix ms of its binary at its start; None: a build from before daemons
/// reported it) talking to this binary, built at `me`.
fn stale(daemon: Option<u64>, me: u64) -> Option<String> {
    let older = daemon.is_none_or(|d| d < me);
    older.then(|| "the collie daemon runs an older build than this collie; restarting it (`collie daemon stop`) stops every process it runs, so it's the user's call".into())
}

/// Sends one request carrying the caller's holder and cwd (unless `params` names one); yields its response frames.
pub fn send(method: &str, params: Value) -> Result<impl Iterator<Item = Result<Value>> + use<>> {
    send_with(method, params, true)
}

/// `send`; without `start`, an error when no daemon runs instead of starting one.
pub fn send_with(method: &str, mut params: Value, start: bool) -> Result<impl Iterator<Item = Result<Value>> + use<>> {
    if params["cwd"].is_null() {
        params["cwd"] = std::env::current_dir()?.to_string_lossy().into();
    }
    params["holder"] = holder().into();
    let mut s = match UnixStream::connect(daemon::socket_path()) {
        Ok(s) => s,
        Err(e) if !start => return Err(e.into()),
        Err(_) => {
            daemon::start(false)?;
            UnixStream::connect(daemon::socket_path())?
        }
    };
    writeln!(s, "{}", json!({"id": 1, "method": method, "params": params}))?;
    let check = method != "daemon.stop";
    Ok(BufReader::new(s).lines().map(move |l| {
        let v: Value = serde_json::from_str(&l?)?;
        if check && v.get("id").is_some() {
            *STALE.lock().unwrap() = stale(v.get("built").and_then(Value::as_u64), daemon::build_time());
        }
        match v.get("error") {
            Some(e) => Err(coded(e["code"].as_str().unwrap_or("error"), e["message"].as_str().unwrap_or_default())),
            None => Ok(v),
        }
    }))
}

pub fn request(method: &str, params: Value) -> Result<Value> {
    request_with(method, params, true)
}

/// `request`; without `start`, an error when no daemon runs instead of starting one.
pub fn request_with(method: &str, params: Value, start: bool) -> Result<Value> {
    let frame = send_with(method, params, start)?.next().context("daemon closed the connection")??;
    Ok(frame["result"].clone())
}

fn print_record(json: bool, r: &Value) {
    if json {
        println!("{r}");
    } else {
        let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
        println!("{:5} {:6} {}", s("level"), s("source"), s("line"));
    }
}

fn print(json: bool, v: &Value) {
    if json {
        return println!("{v}");
    }
    let scalar = |v: &Value| v.as_str().map_or_else(|| v.to_string(), Into::into);
    let Some(obj) = v.as_object() else { return println!("{}", scalar(v)) };
    for (k, val) in obj {
        let Some(items) = val.as_array() else {
            println!("{k}: {}", scalar(val));
            continue;
        };
        println!("{k}:");
        for item in items {
            let row = match item.as_object() {
                Some(o) => o.iter().filter(|(_, x)| !x.is_null()).map(|(k, x)| format!("{k}={}", scalar(x))).collect::<Vec<_>>().join("  "),
                None => scalar(item),
            };
            println!("  {row}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_only_about_a_daemon_older_than_the_client() {
        assert!(stale(None, 5).is_some(), "a daemon from before `built` existed");
        assert!(stale(Some(4), 5).is_some());
        assert!(stale(Some(5), 5).is_none());
        assert!(stale(Some(6), 5).is_none(), "an older client talking to a newer daemon");
    }

    #[test]
    fn holder_precedence() {
        let h = |vars: &[(&str, &str)]| holder_from(|k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()), 42);
        let all = [("HERDR_PANE_ID", "w1:p2"), ("COLLIE_HOLDER", "ci"), ("CLAUDE_CODE_SESSION_ID", "s1")];
        assert_eq!(h(&all), "w1:p2");
        assert_eq!(h(&all[1..]), "ci");
        assert_eq!(h(&[("COLLIE_HOLDER", ""), ("CLAUDE_CODE_SESSION_ID", "s1")]), "claude:s1", "empty doesn't pin");
        assert_eq!(h(&[("CLAUDE_CODE_SESSION_ID", "")]), "host:42");
    }
}
