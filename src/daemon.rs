use crate::eco::{self, Ctx, Ecosystem, Level, Platform, Resource, Sim, SimKind, Source, Target};
use crate::logs::{self, Filter, LogFile, Record};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::sleep;

const TTL_MS: u64 = 2 * 60 * 60 * 1000;
const PORTS: std::ops::RangeInclusive<u16> = 3000..=3999;
const TAIL: usize = 200;
/// Run logs older than this go, then the oldest while they take more than `LOG_BYTES`; see `prune`.
const LOG_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
const LOG_BYTES: u64 = 500 * 1024 * 1024;
/// How often a finished run may trigger `prune`.
const PRUNE_EVERY_MS: u64 = 60 * 60 * 1000;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("COLLIE_STATE_DIR").map_or_else(|| home().join(".local/state/collie"), PathBuf::from)
}

/// Run logs. Disposable, so in the user's cache dir, which cleaners (and macOS under storage pressure) may empty; beside
/// the state when `COLLIE_STATE_DIR` isolates a daemon (tests).
pub fn logs_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("COLLIE_STATE_DIR") {
        return PathBuf::from(d).join("logs");
    }
    let cache = match std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        _ if cfg!(target_os = "macos") => home().join("Library/Caches"),
        Some(d) => PathBuf::from(d),
        None => home().join(".cache"),
    };
    cache.join("collie/logs")
}

pub fn socket_path() -> PathBuf {
    state_dir().join("collie.sock")
}

/// When this binary was built: its file's modification time, Unix ms; 0 when unreadable. A daemon takes it at start,
/// so a rebuild since shows as a newer client.
pub fn build_time() -> u64 {
    let modified = std::env::current_exe().and_then(std::fs::metadata).and_then(|m| m.modified());
    modified.map_or(0, |t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// An error carrying a protocol error code such as `held`.
#[derive(Debug)]
pub struct Coded(pub String, pub String);

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Coded {}

pub fn coded(code: &str, msg: impl Into<String>) -> anyhow::Error {
    Coded(code.into(), msg.into()).into()
}

/// True when something accepts TCP connections on the loopback port.
pub fn port_open(port: u16) -> bool {
    ["127.0.0.1", "::1"].iter().any(|ip| {
        let addr = SocketAddr::new(ip.parse().unwrap(), port);
        TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok()
    })
}

// ---- leases

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub id: u64,
    pub holder: String,
    pub agent: Option<String>,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulator: Option<Sim>,
    /// A physical device: never created or retired by collie, so kept apart from `simulator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<Sim>,
    /// A throwaway database container (`postgres`, `redis`); `port` is its host port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<Service>,
    pub acquired_at: u64,
    pub expires_at: u64,
    /// Held by a process `run` started; dropped when the daemon starts, since processes don't outlive it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub run: bool,
    /// The queued process this was handed to, until it takes it (`adopt`): released with that process if it ends first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Service {
    pub image: String,
    /// Docker container name, `collie-<lease id>-<start ms>`.
    pub container: String,
    /// What to connect to, e.g. `postgres://collie:collie@127.0.0.1:3001/collie`.
    pub url: String,
}

#[derive(Serialize)]
pub struct Waiter {
    #[serde(skip)]
    id: u64,
    pub holder: String,
    pub agent: Option<String>,
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<Sim>,
    pub preferred: Option<u16>,
    pub since: u64,
    /// The process queued for the device (`run`'s `queue`); none for `dibs acquire --wait`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(skip)]
    tx: Option<oneshot::Sender<Lease>>,
}

#[derive(Default)]
pub struct Leases {
    pub leases: Vec<Lease>,
    pub queue: Vec<Waiter>,
    next: u64,
    snapshot: Option<PathBuf>,
    /// Receives the UDID of every simulator lease removed, to shut its device down and delete it.
    retire: Option<mpsc::UnboundedSender<String>>,
    /// UDIDs of the devices this daemon created and has not deleted yet (`devices.json`); the only ones it ever retires
    /// on its own, so it never touches another daemon's devices.
    devices: Vec<String>,
    /// Devices whose retirement failed this session; a graceful stop doesn't wait for them (the next start retries).
    retire_failed: Vec<String>,
    /// Devices being created right now, not yet granted.
    creating: usize,
    /// Receives the container of every service lease removed, to `docker rm -f` it.
    remove_containers: Option<mpsc::UnboundedSender<String>>,
    /// Containers sent there and not removed yet; a graceful stop waits for them.
    removing: usize,
    /// Ports picked for containers still starting, so no other lease picks them meanwhile.
    binding: Vec<u16>,
    /// Unique temporary names of devices being created (`pending.json`): a daemon killed during `simctl create` can't
    /// have recorded the UDID yet, so the next start finds the device by this name.
    pending: Vec<String>,
    /// Process groups this daemon spawned and their leaders' start times (`groups.json`): a daemon killed with SIGKILL
    /// can't stop them, so the next start does.
    groups: Vec<(u32, String)>,
}

impl Leases {
    /// Loads `leases.json` and `devices.json` from `dir`, and retires what died with the previous daemon: leases of
    /// processes, and created devices no surviving lease holds.
    fn load(dir: &Path, retire: mpsc::UnboundedSender<String>) -> Self {
        let read = |f: &str| std::fs::read(dir.join(f)).ok();
        let all: Vec<Lease> = read("leases.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut devices: Vec<String> = read("devices.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        // A template queued for deletion just before a kill that sims.json still stores stays a template.
        let templates = template_udids(&read("sims.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default());
        devices.retain(|u| !templates.contains(u));
        let pending: Vec<String> = read("pending.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let groups: Vec<(u32, String)> = read("groups.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let next = all.iter().map(|l| l.id).max().unwrap_or(0) + 1;
        let leases: Vec<Lease> = all.into_iter().filter(|l| !l.run && l.process.is_none()).collect();
        for u in devices.iter().filter(|u| !leases.iter().any(|l| l.simulator.as_ref().is_some_and(|s| s.key() == **u))) {
            let _ = retire.send(u.clone());
        }
        let snapshot = Some(dir.join("leases.json"));
        let ls = Leases { leases, next, snapshot, retire: Some(retire), devices, pending, groups, ..Default::default() };
        ls.save();
        ls
    }

    fn save(&self) {
        let Some(path) = &self.snapshot else { return };
        // write-then-rename: a crash mid-write must not leave a truncated snapshot
        let write = |path: &Path, bytes: Vec<u8>| {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        };
        write(path, serde_json::to_vec_pretty(&self.leases).unwrap());
        write(&path.with_file_name("devices.json"), serde_json::to_vec_pretty(&self.devices).unwrap());
        write(&path.with_file_name("pending.json"), serde_json::to_vec_pretty(&self.pending).unwrap());
        write(&path.with_file_name("groups.json"), serde_json::to_vec_pretty(&self.groups).unwrap());
    }

    /// Whether a device this daemon created is waiting to be retired (its lease is gone, and no retirement of it has
    /// failed yet), or one is still being created.
    fn retiring(&self) -> bool {
        let waits = |u: &String| !self.retire_failed.contains(u) && !self.leases.iter().any(|l| l.simulator.as_ref().is_some_and(|s| s.key() == *u));
        self.creating > 0 || self.removing > 0 || self.devices.iter().any(waits)
    }

    fn forget_deleted(&mut self, udid: &str) {
        self.devices.retain(|u| u != udid);
        self.save();
    }

    /// `preferred` if free, else any free port in the range, unless `exact`.
    fn pick(&self, preferred: Option<u16>, exact: bool) -> Option<u16> {
        let free = |p: u16| !self.leases.iter().any(|l| l.port == Some(p)) && !self.binding.contains(&p) && !port_open(p);
        match preferred {
            Some(p) if free(p) => Some(p),
            Some(_) if exact => None,
            _ => PORTS.into_iter().find(|&p| free(p)),
        }
    }

    /// The lease on physical device `udid`, if any; the device is free without one.
    fn device_lease(&self, udid: &str) -> Option<&Lease> {
        self.leases.iter().find(|l| l.device.as_ref().is_some_and(|s| s.udid == udid))
    }

    /// Takes the next lease id; a simulator lease is granted under it once its device exists.
    fn reserve(&mut self) -> u64 {
        self.next += 1;
        self.next - 1
    }

    /// `simulator` or `device` when given, else a port lease on `port`; `run` when a `run` takes it for its process.
    #[allow(clippy::too_many_arguments)]
    fn grant(&mut self, id: u64, holder: &str, agent: Option<String>, port: Option<u16>, simulator: Option<Sim>, device: Option<Sim>, run: bool, now: u64) -> Lease {
        let resource = if simulator.is_some() { "simulator" } else if device.is_some() { "device" } else { "port" }.into();
        if let Some(s) = &simulator {
            self.devices.push(s.key());
        }
        let l = Lease { id, holder: holder.into(), agent, resource, port, simulator, device, service: None, acquired_at: now, expires_at: now + TTL_MS, run, process: None };
        self.leases.push(l.clone());
        self.save();
        l
    }

    /// A port lease, or with `device` a lease on that physical device while no one else holds it (the holder's own,
    /// renewed, if it has one).
    pub fn acquire(&mut self, holder: &str, agent: Option<String>, preferred: Option<u16>, exact: bool, device: Option<Sim>, now: u64) -> Option<Lease> {
        let port = match device.as_ref().map(|dev| self.device_lease(&dev.udid).map(|l| (l.id, l.holder == holder))) {
            Some(Some((id, true))) => return self.renew(id, now),
            Some(Some(_)) => return None,
            Some(None) => None,
            None => Some(self.pick(preferred, exact)?),
        };
        let id = self.reserve();
        Some(self.grant(id, holder, agent, port, None, device, false, now))
    }

    /// Queues a waiter, for `process` when a run queues; it is handed a lease as soon as one is released. FIFO among
    /// waiters that fit.
    pub fn enqueue(&mut self, holder: &str, agent: Option<String>, preferred: Option<u16>, device: Option<Sim>, process: Option<String>, now: u64) -> (u64, oneshot::Receiver<Lease>) {
        let (tx, rx) = oneshot::channel();
        let id = self.next;
        self.next += 1;
        let resource = if device.is_some() { "device" } else { "port" }.into();
        self.queue.push(Waiter { id, holder: holder.into(), agent, resource, device, preferred, since: now, process, tx: Some(tx) });
        (id, rx)
    }

    fn hand_to_waiters(&mut self, now: u64) {
        let mut i = 0;
        while i < self.queue.len() {
            let fits = match &self.queue[i].device {
                Some(dev) => self.device_lease(&dev.udid).is_none().then_some(None),
                None => self.pick(self.queue[i].preferred, true).map(Some),
            };
            let Some(port) = fits else {
                i += 1;
                continue;
            };
            let mut w = self.queue.remove(i);
            let id = self.reserve();
            let mut lease = self.grant(id, &w.holder, w.agent.clone(), port, None, w.device.take(), false, now);
            if let Some(p) = w.process.take() {
                lease.process = Some(p);
                self.leases.last_mut().expect("just granted").process = lease.process.clone();
                self.save();
            }
            if w.tx.take().is_some_and(|tx| tx.send(lease.clone()).is_err()) {
                // the waiter gave up meanwhile
                self.leases.retain(|l| l.id != lease.id);
                self.save();
            }
        }
    }

    fn remove(&mut self, pred: impl Fn(&Lease) -> bool, now: u64) -> Vec<Lease> {
        let (gone, keep) = std::mem::take(&mut self.leases).into_iter().partition(|l| pred(l));
        self.leases = keep;
        if !gone.is_empty() {
            self.save();
            self.hand_to_waiters(now);
        }
        for s in gone.iter().filter_map(|l| l.simulator.as_ref()) {
            if let Some(tx) = &self.retire {
                let _ = tx.send(s.key());
            }
        }
        for s in gone.iter().filter_map(|l| l.service.as_ref()) {
            if let Some(tx) = &self.remove_containers {
                self.removing += usize::from(tx.send(s.container.clone()).is_ok());
            }
        }
        gone
    }

    pub fn release(&mut self, id: u64, now: u64) -> Vec<Lease> {
        self.remove(|l| l.id == id, now)
    }

    pub fn expire(&mut self, now: u64) -> Vec<Lease> {
        self.remove(|l| l.expires_at <= now, now)
    }

    pub fn release_holder(&mut self, holder: &str, now: u64) -> Vec<Lease> {
        self.queue.retain(|w| w.holder != holder);
        self.remove(|l| l.holder == holder, now)
    }

    fn renew(&mut self, id: u64, now: u64) -> Option<Lease> {
        let l = self.leases.iter_mut().find(|l| l.id == id)?;
        l.expires_at = now + TTL_MS;
        let l = l.clone();
        self.save();
        Some(l)
    }
}

// ---- processes

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum St {
    Building,
    /// Waiting for a physical device another holder has, before its run phase; built already if it has a build.
    Queued,
    Running,
    Exited,
    Crashed,
    /// Killed by collie when its run phase outlived its `timeout_ms`; not a crash.
    #[serde(rename = "timed-out")]
    TimedOut,
}

#[derive(Clone, Serialize)]
struct Proc {
    id: String,
    target: String,
    /// The target's kind; `exec` can't be rerun by name.
    kind: String,
    dir: PathBuf,
    state: St,
    code: Option<i32>,
    /// The signal that killed it; then `code` is null.
    signal: Option<i32>,
    pid: Option<u32>,
    holder: String,
    agent: Option<String>,
    leases: Vec<u64>,
    port: Option<u16>,
    simulator: Option<Sim>,
    device: Option<Sim>,
    since: u64,
    /// Unix ms; null while it runs.
    ended_at: Option<u64>,
    built: bool,
    launched: bool,
    /// Its newest output line says it waits on a lock another build holds (`eco::waits_for_lock`).
    blocked: bool,
    /// A test run's totals, `{"passed", "failed", "skipped", "duration_ms"}`: counted from its output while it runs
    /// (approximate), then the adapter's results when it has any. Null for other kinds and runners it can't count.
    tests: Option<Value>,
    /// Ran some of its tests only (`run`'s `only`), so `tests` aren't the whole suite's.
    only: bool,
    /// The git branch of its dir when it started; null when detached or outside git.
    branch: Option<String>,
    /// Its build problems' counts, `{"error", "warning", "lint"}` (the items are in `<id>.problems.json`); null for
    /// exec and a run stopped before it ended.
    problems: Option<Value>,
    /// Its run phase outlived its kill limit and is being killed; `finish` makes it `timed-out`. Not the state
    /// itself before then, which would count as ended while the process still lives.
    #[serde(skip)]
    timed_out: bool,
    #[serde(skip)]
    t: Target,
}

impl Proc {
    fn ended(&self) -> bool {
        matches!(self.state, St::Exited | St::Crashed | St::TimedOut)
    }
}

struct State {
    procs: Vec<Proc>,
    leases: Leases,
    /// Set once shutdown starts; runs claimed after it are refused.
    stopping: bool,
    /// Processes whose log is still being written (a phase's output not yet drained), so `logs --follow` knows when
    /// an old process's log is complete.
    writing: HashSet<String>,
    /// When `prune` last started, Unix ms.
    pruned_at: u64,
}

struct Daemon {
    st: Mutex<State>,
    tx: broadcast::Sender<Value>,
    dir: PathBuf,
    /// `logs_dir()`.
    logs: PathBuf,
    /// `build_time()` at start.
    built: u64,
}

type D = Arc<Daemon>;

impl Daemon {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }

    fn snapshot(&self) -> Value {
        let st = self.lock();
        json!({"processes": st.procs, "leases": st.leases.leases, "queue": st.leases.queue})
    }

    fn changed(&self) {
        let _ = self.tx.send(json!({"event": "status", "data": self.snapshot()}));
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.logs.join(format!("{id}.log"))
    }
}

/// Guards `history.jsonl`: `finish` appends while `prune` rewrites it.
static HISTORY: Mutex<()> = Mutex::new(());

/// Ended runs, oldest first, from `history.jsonl` in the state dir: one `Proc` (as `status` shows it) per line.
fn read_history(dir: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(dir.join("history.jsonl")).unwrap_or_default();
    // A line cut short by a crash mid-append is skipped.
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Deletes run logs older than `LOG_AGE`, then the oldest while the rest exceed `LOG_BYTES`, sparing `keep` (live
/// runs) and each target's newest run, with their test results; also result bundles of runs no longer live. Then
/// drops the history lines whose log is gone.
fn prune(dir: &Path, logs: &Path, keep: &HashSet<String>) {
    let _g = HISTORY.lock().unwrap();
    let runs = read_history(dir);
    let mut newest: HashMap<(&str, &str), (u64, &str)> = HashMap::new();
    for r in &runs {
        let key = (r["target"].as_str().unwrap_or_default(), r["dir"].as_str().unwrap_or_default());
        let since = r["since"].as_u64().unwrap_or_default();
        if newest.get(&key).is_none_or(|(s, _)| since >= *s) {
            newest.insert(key, (since, r["id"].as_str().unwrap_or_default()));
        }
    }
    let spared: HashSet<&str> = keep.iter().map(String::as_str).chain(newest.values().map(|(_, id)| *id)).collect();
    let mut files: Vec<(SystemTime, u64, PathBuf)> = std::fs::read_dir(logs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .filter(|(_, _, p)| p.extension().is_some_and(|x| x == "log"))
        .collect();
    files.sort_by_key(|(m, ..)| *m);
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    for (modified, len, path) in &files {
        let id = path.file_stem().unwrap_or_default().to_string_lossy();
        if spared.contains(id.as_ref()) {
            continue;
        }
        let old = SystemTime::now().duration_since(*modified).is_ok_and(|age| age > LOG_AGE);
        if (old || total > LOG_BYTES) && std::fs::remove_file(path).is_ok() {
            total -= len;
        }
    }
    // A run's test results and problems go with its log; a result bundle outlives its run only when the daemon died
    // mid-run. ponytail: so a target's problems from runs older than LOG_AGE disappear with them.
    for e in std::fs::read_dir(logs).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let stale = match (name.strip_suffix(".tests.json").or_else(|| name.strip_suffix(".problems.json")), name.strip_suffix(".xcresult")) {
            (Some(id), _) => !logs.join(format!("{id}.log")).exists(),
            (_, Some(id)) => !keep.contains(id),
            _ => false,
        };
        if stale {
            let _ = std::fs::remove_dir_all(e.path()).or_else(|_| std::fs::remove_file(e.path()));
        }
    }
    let kept: Vec<String> = runs.iter().filter(|r| logs.join(format!("{}.log", r["id"].as_str().unwrap_or_default())).exists()).map(Value::to_string).collect();
    if kept.len() != runs.len() {
        let tmp = dir.join("history.jsonl.tmp");
        let body: String = kept.iter().map(|l| format!("{l}\n")).collect();
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("history.jsonl"));
        }
    }
}

/// Runs `prune` on a thread, sparing the live runs, unless it ran in the last `PRUNE_EVERY_MS`.
fn prune_soon(d: &D) {
    let keep: HashSet<String> = {
        let mut st = d.lock();
        let now = now_ms();
        if now.saturating_sub(st.pruned_at) < PRUNE_EVERY_MS {
            return;
        }
        st.pruned_at = now;
        st.procs.iter().filter(|p| !p.ended()).map(|p| p.id.clone()).collect()
    };
    let (dir, logs) = (d.dir.clone(), d.logs.clone());
    std::thread::spawn(move || prune(&dir, &logs, &keep));
}

pub fn who(holder: &str, agent: &Option<String>) -> String {
    match agent {
        Some(a) => format!("{holder} ({a})"),
        None => holder.into(),
    }
}

/// Processes named `target` (without one: held by `holder`), narrowed to cwd's repo when any are in it. Oldest first.
fn select<'a>(procs: &'a [Proc], cwd: &Path, target: Option<&str>, holder: &str) -> Vec<&'a Proc> {
    let mut v: Vec<_> = procs.iter().filter(|p| target.map_or(p.holder == holder, |n| p.t.name == n)).collect();
    if v.iter().any(|p| cwd.starts_with(&p.t.dir)) {
        v.retain(|p| cwd.starts_with(&p.t.dir));
    }
    v
}

fn newest(d: &D, cwd: &Path, target: Option<&str>, holder: &str) -> Result<String> {
    let st = d.lock();
    let p = select(&st.procs, cwd, target, holder).pop();
    p.map(|p| p.id.clone()).ok_or_else(|| coded("not_found", format!("no process for {}", target.unwrap_or(holder))))
}

async fn kill_group(pgid: u32) {
    let sig = |s: &str| {
        tokio::process::Command::new("kill")
            .args(["-s", s, "--", &format!("-{pgid}")])
            .stderr(Stdio::null())
            .status()
    };
    let _ = sig("TERM").await;
    for _ in 0..30 {
        if !sig("0").await.is_ok_and(|s| s.success()) {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let _ = sig("KILL").await;
}

/// When process `pid` started (`ps`'s `lstart`, to the second); None once it is gone. With the pid it names one
/// process: a reused pid would also need the same start second.
fn started(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps").env("LC_ALL", "C").args(["-o", "lstart=", "-p", &pid.to_string()]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// The recorded groups whose leader is still the process recorded, so a pid reused since is left alone.
// ponytail: a group whose leader exited while its members live on is missed; walk `ps -g` members if that bites
fn survivors(groups: &[(u32, String)], started: impl Fn(u32) -> Option<String>) -> Vec<u32> {
    groups.iter().filter(|(g, at)| started(*g).as_ref() == Some(at)).map(|(g, _)| *g).collect()
}

/// Removes matching processes, records the live ones in the history, releases their leases and kills their process
/// groups.
async fn stop_where(d: &D, pred: impl Fn(&Proc) -> bool) -> Vec<String> {
    let gone: Vec<Proc> = {
        let mut st = d.lock();
        let (gone, keep): (Vec<_>, _) = std::mem::take(&mut st.procs).into_iter().partition(|p| pred(p));
        st.procs = keep;
        // Here, not in `finish`: it finds the process gone. Before `changed`, so a client that sees it vanish finds it
        // in the history. ponytail: says SIGTERM even when `kill_group` escalates to KILL; return its signal if that matters.
        for p in gone.iter().filter(|p| !p.ended()) {
            let mut p = p.clone();
            (p.state, p.code, p.signal, p.pid, p.ended_at, p.blocked) = (St::Crashed, None, Some(15), None, Some(now_ms()), false);
            record(&d.dir, &p);
        }
        let ids: Vec<u64> = gone.iter().flat_map(|p| p.leases.clone()).collect();
        let of_gone = |p: &Option<String>| gone.iter().any(|g| p.as_ref() == Some(&g.id));
        st.leases.queue.retain(|w| !of_gone(&w.process));
        st.leases.remove(|l| ids.contains(&l.id) || of_gone(&l.process), now_ms());
        gone
    };
    if !gone.is_empty() {
        d.changed();
    }
    for p in &gone {
        if let Some(pid) = p.pid {
            kill_group(pid).await;
        }
    }
    gone.into_iter().map(|p| p.id).collect()
}

/// Drops leases (released, expired, holder gone) and stops the processes that held them.
async fn drop_leases(d: &D, f: impl FnOnce(&mut Leases) -> Vec<Lease>) -> Vec<Lease> {
    let gone = f(&mut d.lock().leases);
    if !gone.is_empty() {
        let ids: Vec<u64> = gone.iter().map(|l| l.id).collect();
        stop_where(d, |p| p.leases.iter().any(|id| ids.contains(id))).await;
        d.changed();
    }
    gone
}

fn emit(d: &D, id: &str, log: &Mutex<LogFile>, rec: Record) {
    let off = log.lock().unwrap().append(&rec);
    let mut v = serde_json::to_value(&rec).unwrap();
    v["process"] = id.into();
    v["cursor"] = logs::cursor(id, off).into();
    let _ = d.tx.send(json!({"event": "log", "data": v}));
}

/// One stream's record in the making: its first line (with when it came) and the lines that continue it.
#[derive(Default)]
struct Block {
    head: Option<(u64, String)>,
    rest: Vec<String>,
    /// Blank lines after a block's last line: kept only if another of its lines follows.
    blanks: usize,
}

impl Block {
    /// Takes `line`, arrived at `ts`; true when it starts a record, with the record it ends (first line, the rest).
    fn push(&mut self, ts: u64, line: &str, continues: bool) -> (bool, Option<(u64, String, Vec<String>)>) {
        let open = self.head.is_some() && !self.rest.is_empty();
        if open && line.trim().is_empty() {
            self.blanks += 1;
            return (false, None);
        }
        // ponytail: 200 lines a block, against runaway input; past that, lines go on as records of their own.
        if continues && self.head.is_some() && 1 + self.rest.len() + self.blanks < 200 {
            self.rest.extend(std::iter::repeat_n(String::new(), std::mem::take(&mut self.blanks)));
            self.rest.push(line.into());
            return (false, None);
        }
        let done = self.take();
        self.head = Some((ts, line.into()));
        (true, done)
    }

    /// The record so far, if any, without trailing blank lines.
    fn take(&mut self) -> Option<(u64, String, Vec<String>)> {
        self.blanks = 0;
        self.head.take().map(|(ts, head)| (ts, head, std::mem::take(&mut self.rest)))
    }
}

/// `lines` as records, each a first line with the lines `continues` joins to it (`Block`, minus the timing).
#[cfg(test)]
pub(crate) fn group(lines: &[&str], continues: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let mut b = Block::default();
    let join = |(_, head, rest): (u64, String, Vec<String>)| std::iter::once(head).chain(rest).collect::<Vec<_>>().join("\n");
    let mut out = vec![];
    for l in lines {
        let joins = b.head.as_ref().is_some_and(|(_, open)| continues(open, l));
        out.extend(b.push(0, l, joins).1.map(join));
    }
    out.extend(b.take().map(join));
    out
}

/// Emits a finished record, classified by its first line.
fn emit_block(d: &D, id: &str, log: &Mutex<LogFile>, e: &dyn Ecosystem, src: Source, (ts, head, rest): (u64, String, Vec<String>)) {
    let (level, mut line) = e.classify(src, &head);
    for l in rest {
        line.push('\n');
        line.push_str(&l);
    }
    emit(d, id, log, Record { ts, source: src, level, line });
}

/// With `watch`, asks the adapter whether each record's first line shows the app launched, until one does. Lines the
/// adapter says continue a record (`Ecosystem::continues`) join it; a record goes out when the next one starts, the
/// stream ends, or 200 ms pass without a line, so live output lags by that at most.
async fn pump(d: &D, id: &str, log: &Mutex<LogFile>, e: &dyn Ecosystem, r: impl AsyncRead + Unpin, src: Source, mut watch: bool) {
    let mut lines = BufReader::new(r).split(b'\n');
    let mut counted_at = 0;
    let mut block = Block::default();
    loop {
        let next = match block.head.is_some() {
            // `next_segment` is cancellation safe: a timeout loses no input.
            true => match tokio::time::timeout(Duration::from_millis(200), lines.next_segment()).await {
                Ok(next) => next,
                Err(_) => {
                    if let Some(b) = block.take() {
                        emit_block(d, id, log, e, src, b);
                    }
                    continue;
                }
            },
            false => lines.next_segment().await,
        };
        let Ok(Some(l)) = next else { break };
        let raw = String::from_utf8_lossy(&l);
        let raw = raw.trim_end_matches('\r');
        let joins = block.head.as_ref().is_some_and(|(_, open)| e.continues(open, raw));
        let (starts, done) = block.push(now_ms(), raw, joins);
        if let Some(b) = done {
            emit_block(d, id, log, e, src, b);
        }
        if !starts {
            continue;
        }
        if watch && e.launched(src, raw) {
            watch = false;
            if let Some(p) = d.lock().procs.iter_mut().find(|p| p.id == id) {
                p.launched = true;
            }
            d.changed();
        }
        let blocked = eco::waits_for_lock(raw);
        let flipped = d.lock().procs.iter_mut().find(|p| p.id == id).is_some_and(|p| std::mem::replace(&mut p.blocked, blocked) != blocked);
        if flipped {
            d.changed();
        }
        if let Some(outcome) = e.outcome(raw).filter(|_| src != Source::Build) {
            let counted = d.lock().procs.iter_mut().find(|p| p.id == id && p.kind == "test").map(|p| count(&mut p.tests, outcome)).is_some();
            // ponytail: each snapshot goes to every subscriber, so counts go out at most every 500 ms per stream; the
            // last ones with `finish`'s.
            if counted && now_ms() >= counted_at + 500 {
                counted_at = now_ms();
                d.changed();
            }
        }
    }
    if let Some(b) = block.take() {
        emit_block(d, id, log, e, src, b);
    }
}

/// Serialises `[prepare]` runs and `prepared.json`: two builds in one repo mustn't generate its project at once.
// ponytail: one lock for every repo; per repo if prepares in different repos queue behind each other noticeably.
static PREPARE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `[prepare]` covering `dir` when it is due: it never succeeded, has no `watch`, or a watched path changed after
/// it last did (`prepared.json` in the state dir, by the declaring dir).
fn prepare_due(state: &Path, dir: &Path) -> Result<Option<(PathBuf, eco::Prepare)>> {
    let Some((root, prep)) = eco::prepare_of(dir)? else { return Ok(None) };
    let stamps: HashMap<String, u64> = std::fs::read(state.join("prepared.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let changed = |at: u64| {
        let newest = eco::newest_change(&root, &prep.watch);
        newest.is_some_and(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64 > at)
    };
    let due = match stamps.get(&*root.to_string_lossy()) {
        Some(&at) => prep.watch.is_empty() || changed(at),
        None => true,
    };
    Ok(due.then_some((root, prep)))
}

/// Records that `root`'s `[prepare]` succeeded now. Taken at the end, not the start: the command itself may write
/// under a watched dir (`tuist generate` writes each module's `.xcodeproj`).
fn prepared(state: &Path, root: &Path) {
    let path = state.join("prepared.json");
    let mut stamps: HashMap<String, u64> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    stamps.insert(root.to_string_lossy().into(), now_ms());
    if let Err(e) = std::fs::write(&path, json!(stamps).to_string()) {
        eprintln!("collie: couldn't write {}: {e}", path.display());
    }
}

/// Runs the `[prepare]` covering `dir` when due, logged as build output of process `id`; Err with its exit (None: it
/// couldn't run) when it fails.
async fn prepare(d: &D, id: &str, log: &Mutex<LogFile>, e: &dyn Ecosystem, dir: &Path, env: &HashMap<String, String>) -> Result<(), Option<ExitStatus>> {
    let _g = PREPARE.lock().await;
    let say = |level, line: String| emit(d, id, log, Record { ts: now_ms(), source: Source::Build, level, line });
    let (root, prep) = match prepare_due(&d.dir, dir) {
        Ok(Some(due)) => due,
        Ok(None) => return Ok(()),
        Err(x) => {
            say(Level::Error, format!("collie: {x:#}"));
            return Err(None);
        }
    };
    say(Level::Info, format!("collie: prepare: {} (in {})", prep.cmd, root.display()));
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", &prep.cmd]).current_dir(&root);
    let status = exec(d, id, log, e, cmd, env, None, St::Building).await;
    if !status.is_some_and(|s| s.success()) {
        say(Level::Error, "collie: prepare failed; the build didn't start".into());
        return Err(status);
    }
    prepared(&d.dir, &root);
    Ok(())
}

/// Runs one phase; None if it could not spawn or the process was stopped meanwhile.
async fn exec(
    d: &D,
    id: &str,
    log: &Mutex<LogFile>,
    e: &dyn Ecosystem,
    cmd: std::process::Command,
    env: &HashMap<String, String>,
    port: Option<u16>,
    phase: St,
) -> Option<ExitStatus> {
    // The adapter's env (DEVELOPER_DIR from .xcode-version) must beat the caller's, so it is applied again last.
    let own: Vec<_> = cmd.get_envs().filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned()))).collect();
    let mut cmd = tokio::process::Command::from(cmd);
    // Line by line through the pipe unless the caller says otherwise. ponytail: C programs on stdio still buffer a
    // pipe; only a pty covers them (dropped for launches in cac2a18).
    cmd.envs([("PYTHONUNBUFFERED", "1"), ("NSUnbufferedIO", "YES")]);
    cmd.envs(env).envs(own).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    if let Some(p) = port {
        cmd.env("PORT", p.to_string());
    }
    let (out, err) = if phase == St::Building { (Source::Build, Source::Build) } else { (Source::Stdout, Source::Stderr) };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(x) => {
            emit(d, id, log, Record { ts: now_ms(), source: err, level: Level::Error, line: format!("spawn failed: {x}") });
            return None;
        }
    };
    let pid = child.id();
    let watch = {
        let mut st = d.lock();
        let watch = match st.procs.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.pid = pid;
                p.state = phase;
                p.built |= phase == St::Running;
                p.launched |= phase == St::Running && p.simulator.is_none() && p.device.is_none();
                // A test run launches no app of its own; its output mustn't pass for the launch line either.
                Some(phase == St::Running && !p.launched && p.t.kind != "test")
            }
            None => None,
        };
        if watch.is_some() {
            // ponytail: a pipe inherited by a process outside the group keeps the id here; close fds on spawn if that bites
            st.writing.insert(id.to_string());
        }
        watch
    };
    let Some(watch) = watch else {
        if let Some(pid) = pid {
            kill_group(pid).await;
        }
        return None;
    };
    d.changed();
    // After `p.pid` is set, so a stop meanwhile still finds the group to kill.
    // ponytail: a SIGKILL between spawn and this record orphans the group; record before spawn (pre_exec) if that bites
    if let Some(p) = pid
        && let Ok(Some(at)) = tokio::task::spawn_blocking(move || started(p)).await
    {
        let mut st = d.lock();
        st.leases.groups.push((p, at));
        st.leases.save();
    }
    let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (status, _, _) = tokio::join!(child.wait(), pump(d, id, log, e, stdout, out, watch), pump(d, id, log, e, stderr, err, watch));
    {
        let mut st = d.lock();
        st.writing.remove(id);
        st.leases.groups.retain(|(g, _)| Some(*g) != pid);
        st.leases.save();
    }
    d.changed();
    status.ok()
}

fn finish(d: &D, id: &str, status: Option<ExitStatus>) {
    {
        let mut st = d.lock();
        let State { procs, leases, .. } = &mut *st;
        // A run that ends while queued (its build failed) mustn't be handed the device later, nor keep one handed to it
        // during the build.
        leases.queue.retain(|w| w.process.as_deref() != Some(id));
        leases.remove(|l| l.process.as_deref() == Some(id), now_ms());
        let Some(p) = procs.iter_mut().find(|p| p.id == id) else { return };
        use std::os::unix::process::ExitStatusExt;
        (p.code, p.signal) = (status.and_then(|s| s.code()), status.and_then(|s| s.signal()));
        p.state = match (p.timed_out, status.is_some_and(|s| s.success())) {
            (true, _) => St::TimedOut,
            (false, true) => St::Exited,
            (false, false) => St::Crashed,
        };
        (p.pid, p.ended_at, p.blocked) = (None, Some(now_ms()), false);
        // A simulator whose app never launched (failed build or launch, and every test run, which never sets
        // `launched`) stays leased to the ended process, so the holder's next run (of any test target in the same dir,
        // for a test) carries it; stop, pane close, TTL, release or another holder's run still retire it. Not after a
        // timeout: a hung run may have left the device wedged, so the next run gets a fresh one.
        let kept: Vec<u64> = match p.simulator.is_some() && !p.launched && !p.timed_out {
            true => leases.leases.iter().filter(|l| l.simulator.is_some() && p.leases.contains(&l.id)).map(|l| l.id).collect(),
            false => vec![],
        };
        let ids: Vec<u64> = std::mem::take(&mut p.leases).into_iter().filter(|id| !kept.contains(id)).collect();
        p.leases = kept;
        leases.remove(|l| ids.contains(&l.id), now_ms());
        record(&d.dir, p);
    }
    d.changed();
    prune_soon(d);
}

/// Appends the ended `p` to `history.jsonl`.
fn record(dir: &Path, p: &Proc) {
    let _g = HISTORY.lock().unwrap();
    let line = format!("{}\n", json!(p));
    let appended = File::options().create(true).append(true).open(dir.join("history.jsonl")).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    if let Err(e) = appended {
        eprintln!("collie: couldn't record {} in history.jsonl: {e}", p.id);
    }
}

#[allow(clippy::too_many_arguments)]
async fn lifecycle(
    d: D,
    id: String,
    t: Target,
    env: HashMap<String, String>,
    port: Option<u16>,
    udid: Option<String>,
    device: Option<String>,
    head: bool,
    queued: Option<oneshot::Receiver<Lease>>,
    args: Vec<String>,
    limit: Option<Duration>,
    log: Mutex<LogFile>,
) {
    let e = eco::get(&t.ecosystem);
    // Before build(), which may read the generated project (xcodebuild -showBuildSettings). Not for exec: the agent's
    // command is its own.
    if t.kind != "exec"
        && let Err(status) = prepare(&d, &id, &log, e, &t.dir, &env).await
    {
        save_problems(&d, &id, &t, true).await;
        return finish(&d, &id, status);
    }
    // build() and run() may shell out (xcodebuild -showBuildSettings takes seconds).
    let (bt, bu, bd) = (t.clone(), udid.clone(), device.clone());
    let (be, ba) = (env.clone(), args.clone());
    let log_path = d.log_path(&id);
    let bl = log_path.clone();
    let Ok(build) = tokio::task::spawn_blocking(move || e.build(&bt, &Ctx { simulator: bu, device: bd, head, env: be, args: ba, log: bl })).await else { return finish(&d, &id, None) };
    let builds = build.is_some();
    if let Some(cmd) = build {
        let status = exec(&d, &id, &log, e, cmd, &env, port, St::Building).await;
        if !status.is_some_and(|s| s.success()) {
            save_problems(&d, &id, &t, true).await;
            return finish(&d, &id, status);
        }
        // A `build` target (a framework's scheme) has nothing to run: built, it is done.
        if t.kind == "build" {
            if let Some(p) = d.lock().procs.iter_mut().find(|p| p.id == id) {
                (p.built, p.pid) = (true, None);
            }
            save_problems(&d, &id, &t, true).await;
            return finish(&d, &id, status);
        }
    }
    // Built (if it builds) while it waits for its device: before the run phase, and before the kill limit starts.
    if let Some(rx) = queued {
        if let Some(p) = d.lock().procs.iter_mut().find(|p| p.id == id) {
            // The build's group is gone: a stop while queued (hours, maybe) mustn't kill whatever reuses the pid.
            (p.state, p.pid) = (St::Queued, None);
        }
        d.changed();
        let Ok(l) = rx.await else {
            save_problems(&d, &id, &t, builds).await;
            return finish(&d, &id, None);
        };
        let adopted = adopt(&mut d.lock(), &id, &l);
        d.changed();
        if !adopted {
            if d.lock().procs.iter().any(|p| p.id == id) {
                let line = "collie: its dibs on the device expired while it built; run it again".to_string();
                emit(&d, &id, &log, Record { ts: now_ms(), source: Source::Stderr, level: Level::Error, line });
            }
            save_problems(&d, &id, &t, builds).await;
            return finish(&d, &id, None);
        }
    }
    let to = device.clone().or(udid.clone());
    let args = match with_udid(args, to.as_deref()) {
        Ok(a) => a,
        Err(x) => {
            emit(&d, &id, &log, Record { ts: now_ms(), source: Source::Stderr, level: Level::Error, line: format!("collie: {x:#}") });
            save_problems(&d, &id, &t, builds).await;
            return finish(&d, &id, None);
        }
    };
    let (rt, re, rl, on_device) = (t.clone(), env.clone(), log_path.clone(), device.is_some());
    let Ok(mut cmd) = tokio::task::spawn_blocking(move || e.run(&rt, &Ctx { simulator: udid, device, head, env: re, args: vec![], log: rl })).await else {
        save_problems(&d, &id, &t, builds).await;
        return finish(&d, &id, None);
    };
    let reader: &dyn Ecosystem = match (eco::runs_xcode(&eco::command_of(&t, &args)), on_device && t.ecosystem == "xcode" && t.kind == "app") {
        (true, _) => &eco::XcodeTool,
        (false, true) => &eco::XcodeDevice,
        (false, false) => e,
    };
    cmd.args(args);
    if let Some(u) = to {
        cmd.env("COLLIE_UDID", u);
    }
    // Started with the run phase only: a cold build may legitimately take long.
    let (ended, ended_rx) = oneshot::channel();
    let guard = limit.map(|limit| tokio::spawn(time_out(d.clone(), id.clone(), limit, ended_rx)));
    let status = exec(&d, &id, &log, reader, cmd, &env, port, St::Running).await;
    // Awaited, not aborted: a guard that has fired finishes its kill (a TERM-ignoring group member can outlive the leader).
    let _ = ended.send(());
    if let Some(g) = guard {
        let _ = g.await;
    }
    if let Some(limit) = limit.filter(|_| d.lock().procs.iter().any(|p| p.id == id && p.timed_out)) {
        let line = format!("collie: killed: its run phase outlived its kill limit of {} ms (`--timeout`); a simulator it ran on is replaced", limit.as_millis());
        emit(&d, &id, &log, Record { ts: now_ms(), source: Source::Stderr, level: Level::Error, line });
    }
    // Its group is gone: a stop while the results are read mustn't kill whatever reuses the pid. Gone from `procs`
    // (stopped, taken over, rerun) or timed out, its tests didn't all run.
    let partial = match d.lock().procs.iter_mut().find(|p| p.id == id) {
        Some(p) => {
            p.pid = None;
            p.only || p.timed_out
        }
        None => true,
    };
    // Before `finish`, whose history line must carry the final totals and counts.
    save_problems(&d, &id, &t, builds).await;
    if t.kind == "test" {
        save_results(&d, &id, e, t, env, log_path, partial).await;
    }
    finish(&d, &id, status);
}

/// One target's runs' results (oldest first), merged into each test's newest result, and their `sep`. A full run drops
/// the tests gone from it (renamed, deleted); a run of some tests only (`"only": true`) drops none, and the tests it
/// adds stay until the next full run.
fn merge_results(runs: impl Iterator<Item = Value>) -> (Value, Vec<Value>) {
    let (mut sep, mut tests, mut at) = (Value::Null, Vec::<Value>::new(), HashMap::new());
    let mut kept: Option<HashSet<String>> = None;
    for v in runs {
        sep = v["sep"].clone();
        let ids = v["tests"].as_array().into_iter().flatten().map(|t| t["id"].to_string());
        match &mut kept {
            Some(k) if v["only"] == true => k.extend(ids),
            _ if v["only"] == true => {}
            _ => kept = Some(ids.collect()),
        }
        for t in v["tests"].as_array().into_iter().flatten() {
            match at.get(&t["id"].to_string()) {
                Some(&i) => tests[i] = t.clone(),
                None => {
                    at.insert(t["id"].to_string(), tests.len());
                    tests.push(t.clone());
                }
            }
        }
    }
    if let Some(kept) = kept {
        tests.retain(|t| kept.contains(&t["id"].to_string()));
    }
    (sep, tests)
}

/// Process `id`'s log records, each with its cursor.
fn records(log: &Path, id: &str) -> Vec<(String, Record)> {
    let bytes = std::fs::read(log).unwrap_or_default();
    let (mut recs, mut off) = (vec![], 0);
    for l in bytes.split(|&b| b == b'\n') {
        if let Ok(r) = serde_json::from_slice::<Record>(l) {
            recs.push((logs::cursor(id, off), r));
        }
        off += l.len() as u64 + 1;
    }
    recs
}

// ---- problems

/// A problem across branches: its file, kind and message, not its line, which the branch's edits shift.
fn identity(i: &Value) -> (String, String, String) {
    let s = |k: &str| i[k].as_str().unwrap_or_default().to_string();
    (s("path"), s("kind"), s("message"))
}

/// A problem within a run and across targets, whose builds of one version of a file agree on lines: `identity` and
/// where. The same message on two lines (`Force Unwrapping Violation`) is two problems.
fn spot(i: &Value) -> ((String, String, String), Option<u64>, Option<u64>) {
    (identity(i), i["line"].as_u64(), i["col"].as_u64())
}

/// Where `raw`, a path a tool printed, is in worktree `root` (resolved): relative to `root`, and whether it was found.
/// Tried as is or against the run's `dir` (an absolute path either way), against `root`, then as the suffix of one
/// tracked file (tuist prints paths relative to a module). Unfound, it stays as printed. None outside the worktree
/// (DerivedData, SourcePackages).
fn resolve(raw: &str, dir: &Path, root: &Path, tracked: &mut Option<Vec<String>>) -> Option<(String, bool)> {
    let p = Path::new(raw);
    for c in [dir.join(p), root.join(p)] {
        if let Ok(c) = std::fs::canonicalize(&c) {
            return c.strip_prefix(root).ok().map(|r| (r.to_string_lossy().into_owned(), true));
        }
    }
    if p.is_absolute() {
        return None;
    }
    let files = tracked.get_or_insert_with(|| git(root, &["ls-files"]).map(|s| s.lines().map(String::from).collect()).unwrap_or_default());
    let mut hits = files.iter().filter(|f| *f == raw || f.ends_with(&format!("/{raw}")));
    match (hits.next(), hits.next()) {
        (Some(f), None) => Some((f.clone(), true)),
        _ => Some((raw.into(), false)),
    }
}

/// The build problems in a run's records (`eco::diagnostic`), once each (xcodebuild repeats a warning per
/// architecture and in its summary), in worktree `root` (resolved). With `build_only`, the build's records only: an
/// app's or a test run's own output reports no build problems.
fn build_problems(recs: &[(String, Record)], dir: &Path, root: &Path, build_only: bool) -> Vec<Value> {
    let (mut tracked, mut seen, mut items) = (None, HashSet::new(), vec![]);
    for (cursor, r) in recs.iter().filter(|(_, r)| r.level >= Level::Warn && (!build_only || r.source == Source::Build)) {
        let Some(mut i) = eco::diagnostic(&r.line) else { continue };
        let Some((path, resolved)) = resolve(i["path"].as_str().unwrap_or_default(), dir, root, &mut tracked) else { continue };
        (i["path"], i["resolved"], i["cursor"]) = (path.into(), resolved.into(), cursor.as_str().into());
        if seen.insert(spot(&i)) {
            items.push(i);
        }
    }
    items
}

/// Writes ended run `id`'s build problems to `<id>.problems.json` beside its log, and their counts to the process;
/// written when there are none too, since a clean build is what retires a file's warnings (`merge_problems`). Not for
/// exec (targets only), nor a run already gone (stopped: incomplete). Drops the worktree's TODO scan too, so the next
/// `problems` scans again after the build, as the pane does.
async fn save_problems(d: &D, id: &str, t: &Target, build_only: bool) {
    if t.kind == "exec" {
        return;
    }
    let (log, pid, dir) = (d.log_path(id), id.to_string(), t.dir.clone());
    let found = tokio::task::spawn_blocking(move || {
        let root = worktree(&dir);
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        SCANS.lock().unwrap().retain(|(r, _)| *r != root);
        build_problems(&records(&log, &pid), &dir, &root, build_only)
    });
    let Ok(items) = found.await else { return };
    let mut counts = json!({"error": 0, "warning": 0, "lint": 0});
    for i in &items {
        let k = i["kind"].as_str().unwrap_or("warning");
        counts[k] = (counts[k].as_u64().unwrap_or_default() + 1).into();
    }
    {
        let mut st = d.lock();
        let Some(p) = st.procs.iter_mut().find(|p| p.id == id) else { return };
        p.problems = Some(counts);
    }
    if let Err(x) = std::fs::write(d.logs.join(format!("{id}.problems.json")), json!({"process": id, "items": items}).to_string()) {
        eprintln!("collie: couldn't write {id}'s problems: {x}");
    }
}

/// The current build problems from targets' runs (`runs`: each with its problems file, oldest first), one item per
/// `spot` with each run reporting it in `sources`, newest first. Per target and file, the newest run that reported the
/// file stands: an incremental Xcode build doesn't re-emit the warnings of files it didn't compile. They go once the
/// file is gone, or changed (`mtime` of a run's file, Unix ms) with a later build of the target started after it.
// ponytail: a warning that goes away because another file changed stays until its own file changes; a full rebuild
// or reading xcactivitylog would settle it.
fn merge_problems(runs: &[(Value, Value)], mtime: impl Fn(&Value, &str) -> Option<u64>) -> Vec<Value> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let target = |r: &Value| (s(&r["target"]), s(&r["dir"]));
    let mut latest: HashMap<(String, String, String), usize> = HashMap::new();
    for (i, (r, f)) in runs.iter().enumerate() {
        for item in f["items"].as_array().into_iter().flatten() {
            let (name, dir) = target(r);
            latest.insert((name, dir, s(&item["path"])), i);
        }
    }
    let (mut out, mut at): (Vec<Value>, HashMap<_, usize>) = (vec![], HashMap::new());
    for ((name, dir, path), i) in latest {
        let (run, file) = &runs[i];
        let items: Vec<&Value> = file["items"].as_array().into_iter().flatten().filter(|it| it["path"] == path.as_str()).collect();
        if items.iter().any(|it| it["resolved"] == true) {
            let since = run["since"].as_u64().unwrap_or_default();
            let rebuilt = |m: u64| runs[i + 1..].iter().any(|(r, _)| target(r) == (name.clone(), dir.clone()) && r["built"] == true && r["since"].as_u64().is_some_and(|s| s > m));
            match mtime(run, &path) {
                None => continue,
                Some(m) if m > since && rebuilt(m) => continue,
                _ => {}
            }
        }
        for it in items {
            let src = json!({"target": name, "process": run["id"], "cursor": it["cursor"], "ended_at": run["ended_at"]});
            match at.get(&spot(it)) {
                Some(&k) => out[k]["sources"].as_array_mut().unwrap().push(src),
                None => {
                    let mut item = it.clone();
                    item.as_object_mut().unwrap().remove("cursor");
                    item["sources"] = json!([src]);
                    at.insert(spot(it), out.len());
                    out.push(item);
                }
            }
        }
    }
    for item in &mut out {
        item["sources"].as_array_mut().unwrap().sort_by_key(|s| std::cmp::Reverse(s["ended_at"].as_u64()));
    }
    out.sort_by_key(|i| (s(&i["path"]), i["line"].as_u64()));
    out
}

/// The branch a worktree's work is compared against: origin's default (`origin_head`), else the first of
/// `origin/main`, `origin/master`, `main`, `master` that `exists`.
fn pick_base(origin_head: Option<String>, exists: impl Fn(&str) -> bool) -> Option<String> {
    origin_head.or_else(|| ["origin/main", "origin/master", "main", "master"].into_iter().find(|b| exists(b)).map(String::from))
}

/// Whether a worktree is on the base branch itself, where nothing is new: its `branch` is the base's `short` name, or,
/// detached, its HEAD is the base's (`head_is_base`). A fresh branch at the merge base isn't: its uncommitted edits
/// are new.
fn is_base(branch: Option<&str>, short: Option<&str>, head_is_base: impl FnOnce() -> bool) -> bool {
    match branch {
        Some(b) => short == Some(b),
        None => head_is_base(),
    }
}

/// The lines `git diff -U0` adds, by path: from its `+++ b/<path>` and `@@ … +<start>[,<count>] @@` lines, as
/// (start, count) ranges.
fn added_lines(diff: &str) -> HashMap<String, Vec<(u64, u64)>> {
    let mut out: HashMap<String, Vec<(u64, u64)>> = HashMap::new();
    let mut path: Option<String> = None;
    for l in diff.lines() {
        if let Some(p) = l.strip_prefix("+++ ") {
            path = p.strip_prefix("b/").map(String::from);
        } else if let (Some(p), Some(h)) = (&path, l.strip_prefix("@@ ")) {
            let Some(plus) = h.split(' ').find_map(|w| w.strip_prefix('+')) else { continue };
            let (start, count) = plus.split_once(',').unwrap_or((plus, "1"));
            if let (Ok(s), Ok(c)) = (start.parse(), count.parse()) {
                out.entry(p.clone()).or_default().push((s, c));
            }
        }
    }
    out
}

/// A `git grep -n` hit (`path:line:text`) as a todo: its first `TODO`, `FIXME` or `HACK` as a whole word, and the text
/// after it without leading `:` and spaces.
fn todo_item(hit: &str) -> Option<Value> {
    let (path, rest) = hit.split_once(':')?;
    let (line, text) = rest.split_once(':')?;
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let whole = |i: usize, tag: &str| !word(text[..i].chars().next_back()) && !word(text[i + tag.len()..].chars().next());
    let hits = ["TODO", "FIXME", "HACK"].into_iter().flat_map(|tag| text.match_indices(tag).filter(move |(i, _)| whole(*i, tag)));
    let (col, tag) = hits.min_by_key(|(i, _)| *i)?;
    let message = text[col + tag.len()..].trim_start_matches(|c: char| c == ':' || c.is_whitespace()).trim_end_matches("*/").trim();
    let line: u64 = line.parse().ok()?;
    Some(json!({"kind": "todo", "tag": tag, "rule": null, "path": path, "resolved": true, "line": line, "col": col + 1, "message": message, "sources": "scan"}))
}

/// `TODO`/`FIXME`/`HACK` in worktree `root`'s tracked text files: `{"scanned_at", "items", "error"}`.
fn scan_todos(root: &Path) -> Value {
    let out = std::process::Command::new("git").arg("-C").arg(root).args(["grep", "-n", "-I", "-w", "-E", "TODO|FIXME|HACK", "--"]).output();
    match out {
        // 1: no match.
        Ok(o) if o.status.success() || o.status.code() == Some(1) => {
            let items: Vec<Value> = String::from_utf8_lossy(&o.stdout).lines().filter_map(todo_item).collect();
            json!({"scanned_at": now_ms(), "items": items, "error": null})
        }
        Ok(o) => {
            let why = String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or_default().trim_start_matches("fatal: ").to_string();
            json!({"scanned_at": now_ms(), "items": [], "error": why})
        }
        Err(e) => json!({"scanned_at": now_ms(), "items": [], "error": e.to_string()}),
    }
}

/// Each worktree's TODO scan this session, by its root; `problems` scans when there is none or on `rescan`.
// ponytail: a static beside HISTORY rather than a `State` field; move it there if anything else needs it under the lock.
static SCANS: Mutex<Vec<(PathBuf, Value)>> = Mutex::new(Vec::new());

/// `problems`: the build problems of cwd's worktree's targets (`merge_problems`), its TODOs (`scan_todos`), and
/// which of them this branch added: on lines added since its merge base with the default branch, uncommitted ones too.
/// Per target, also what it reports here against the base branch's builds of it in this repo (any worktree), merged
/// the same way and compared without lines, which the branch's edits shift; TODO lints, which the scan stands in for,
/// on neither side.
fn problems(d: &D, cwd: &Path, rescan: bool, only_new: bool) -> Value {
    let root = worktree(cwd);
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let history = {
        let _g = HISTORY.lock().unwrap();
        read_history(&d.dir)
    };
    let file = |id: &Value| -> Option<Value> { serde_json::from_slice(&std::fs::read(d.logs.join(format!("{}.problems.json", id.as_str()?))).ok()?).ok() };
    let inside = |r: &Value| r["dir"].as_str().is_some_and(|dir| std::fs::canonicalize(dir).is_ok_and(|dir| dir.starts_with(&root)));
    let runs: Vec<(Value, Value)> = history.iter().filter(|r| r["kind"] != "exec" && inside(r)).filter_map(|r| Some((r.clone(), file(&r["id"])?))).collect();
    let mtime = |p: &Path| Some(std::fs::metadata(p).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64);
    let mut items = merge_problems(&runs, |_, path| mtime(&root.join(path)));

    let cached = SCANS.lock().unwrap().iter().find(|(r, _)| *r == root).map(|(_, s)| s.clone());
    let scan = match cached {
        Some(s) if !rescan => s,
        _ => {
            let s = scan_todos(&root);
            let mut scans = SCANS.lock().unwrap();
            scans.retain(|(r, _)| *r != root);
            scans.push((root.clone(), s.clone()));
            s
        }
    };
    let todos = scan["items"].as_array().cloned().unwrap_or_default();
    let at = |i: &Value| (i["path"].clone(), i["line"].clone());
    // SwiftLint's TODO rule, at a TODO the scan has: the scan's item stands.
    let linted = |i: &Value| i["kind"] == "lint" && (i["rule"] == "todo" || i["message"].as_str().is_some_and(|m| m.starts_with("Todo Violation")));
    items.retain(|i| !(linted(i) && todos.iter().any(|t| at(t) == at(i))));
    items.extend(todos);

    let base = pick_base(git(&root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]), |b| git(&root, &["rev-parse", "--verify", "--quiet", b]).is_some());
    let merge_base = base.as_ref().and_then(|b| git(&root, &["merge-base", b, "HEAD"]));
    let short = base.as_deref().map(|b| b.strip_prefix("origin/").unwrap_or(b).to_string());
    let branch = git(&root, &["branch", "--show-current"]);
    let head_is_base = || base.as_ref().is_some_and(|b| git(&root, &["rev-parse", "HEAD"]) == git(&root, &["rev-parse", b]));
    let on_base = merge_base.is_some() && is_base(branch.as_deref(), short.as_deref(), head_is_base);
    // ponytail: added lines are matched against a problem's line from its build, which edits since may have shifted;
    // and an untracked file isn't in the diff, so its problems never count as new.
    // The prefixes and unquoted paths `added_lines` reads, whatever the user's diff config.
    let diff = |mb: &str| git(&root, &["-c", "core.quotePath=false", "diff", "-U0", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", mb, "--"]);
    let added = match (&merge_base, on_base) {
        (Some(mb), false) => Some(added_lines(&diff(mb).unwrap_or_default())),
        _ => None,
    };
    for i in &mut items {
        let ranges = added.as_ref().map(|a| a.get(i["path"].as_str().unwrap_or_default()));
        let line = i["line"].as_u64().unwrap_or_default();
        i["new"] = ranges.map(|r| r.is_some_and(|r| r.iter().any(|(s, c)| (*s..s + c).contains(&line)))).into();
    }
    let mut builds = serde_json::Map::new();
    let repo = repo_of(&root);
    let mut repos: HashMap<String, String> = HashMap::new();
    let names: HashSet<String> = runs.iter().filter_map(|(r, _)| r["target"].as_str().map(String::from)).collect();
    for name in names {
        let on_base_branch = |r: &&Value| r["target"] == name.as_str() && r["kind"] != "exec" && short.is_some() && r["branch"].as_str() == short.as_deref();
        let mut there_runs = vec![];
        for r in history.iter().filter(on_base_branch) {
            let Some(dir) = r["dir"].as_str() else { continue };
            if *repos.entry(dir.into()).or_insert_with(|| repo_of(Path::new(dir))) == repo
                && let Some(f) = file(&r["id"])
            {
                there_runs.push((r.clone(), f));
            }
        }
        let newest = there_runs.iter().rev().find(|(r, _)| r["built"] == true).map(|(r, _)| r.clone());
        builds.insert(name.clone(), match newest {
            Some(r) => {
                // Each base run's files are in its own worktree.
                let dirs: HashSet<&str> = there_runs.iter().filter_map(|(r, _)| r["dir"].as_str()).collect();
                let roots: HashMap<&str, PathBuf> = dirs.into_iter().map(|d| (d, worktree(Path::new(d)))).collect();
                let there = merge_problems(&there_runs, |r, path| mtime(&roots.get(r["dir"].as_str()?)?.join(path)));
                let here: HashSet<_> = items.iter().filter(|i| !linted(i) && i["sources"].as_array().is_some_and(|s| s.iter().any(|s| s["target"] == name.as_str()))).map(identity).collect();
                let there: HashSet<_> = there.iter().filter(|i| !linted(i)).map(identity).collect();
                let tally = |ids: Vec<&(String, String, String)>| {
                    let mut t = serde_json::Map::new();
                    for (_, k, _) in ids {
                        t.insert(k.clone(), (t.get(k).and_then(Value::as_u64).unwrap_or_default() + 1).into());
                    }
                    Value::Object(t)
                };
                json!({"process": r["id"], "ended_at": r["ended_at"], "added": tally(here.difference(&there).collect()), "fixed": tally(there.difference(&here).collect())})
            }
            None => Value::Null,
        });
    }
    let count = |items: &mut dyn Iterator<Item = &Value>| {
        let mut t = json!({"error": 0, "warning": 0, "lint": 0, "todo": 0});
        for i in items {
            let k = i["kind"].as_str().unwrap_or("warning");
            t[k] = (t[k].as_u64().unwrap_or_default() + 1).into();
        }
        t
    };
    // After the comparison, which needs every item.
    if only_new {
        items.retain(|i| i["new"] == true);
    }
    let totals = count(&mut items.iter());
    let new_totals = added.is_some().then(|| count(&mut items.iter().filter(|i| i["new"] == true)));
    json!({
        "root": root,
        "items": items,
        "totals": totals,
        "new_totals": new_totals,
        "base": {"ref": base, "merge_base": merge_base, "on_base": on_base, "builds": builds},
        "scan": {"scanned_at": scan["scanned_at"], "error": scan["error"]},
    })
}

/// Adds `outcome` to a test run's totals.
fn count(tests: &mut Option<Value>, outcome: &str) {
    let t = tests.get_or_insert_with(|| json!({"passed": 0, "failed": 0, "skipped": 0, "duration_ms": null}));
    t[outcome] = (t[outcome].as_u64().unwrap_or_default() + 1).into();
}

/// The adapter's results for ended test run `id`, written to `<id>.tests.json` beside its log with each test's `path`
/// (its id split on `sep`) and `process`, and their totals; the totals replace the live counts. Without results (a
/// runner the adapter can't read, a failed parse) the live counts stay. `partial` (some tests only, or cut short)
/// marks them `"only": true`, so `merge_results` drops no test for missing from them.
async fn save_results(d: &D, id: &str, e: &'static dyn Ecosystem, t: Target, env: HashMap<String, String>, log: PathBuf, partial: bool) {
    let pid = id.to_string();
    let read = tokio::task::spawn_blocking(move || {
        let recs = records(&log, &pid);
        e.results(&t, &Ctx { env, log, ..Default::default() }, &recs)
    });
    let Ok(Some(mut v)) = read.await else { return };
    let sep = v["sep"].as_str().unwrap_or("/").to_string();
    let (mut totals, mut ms) = (None, None);
    for t in v["tests"].as_array_mut().into_iter().flatten() {
        t["path"] = t["id"].as_str().unwrap_or_default().split(sep.as_str()).collect::<Vec<_>>().into();
        t["process"] = id.into();
        count(&mut totals, t["status"].as_str().unwrap_or("failed"));
        if let Some(d) = t["duration_ms"].as_u64() {
            ms = Some(ms.unwrap_or(0) + d);
        }
    }
    let Some(mut totals) = totals else { return };
    totals["duration_ms"] = ms.into();
    (v["process"], v["totals"]) = (id.into(), totals.clone());
    v["only"] = partial.into();
    if let Err(x) = std::fs::write(d.logs.join(format!("{id}.tests.json")), v.to_string()) {
        eprintln!("collie: couldn't write {id}'s test results: {x}");
    }
    if let Some(p) = d.lock().procs.iter_mut().find(|p| p.id == id) {
        p.tests = Some(totals);
    }
}

/// `run`'s `args` with `only`'s test ids (from `tests`) as `e`, `t`'s ecosystem, selects them. Empty runs it all.
fn only_args(e: &dyn Ecosystem, t: &Target, only: &[String], args: Vec<String>) -> Result<Vec<String>> {
    if only.is_empty() {
        return Ok(args);
    }
    e.select(t, only, args)
}

/// The kill limit of a run phase: `timeout_ms` from `run`/`exec`, 0 for none; without one, a test's default.
fn run_limit(kind: &str, timeout_ms: Option<u64>) -> Option<Duration> {
    // ponytail: 30 min for any test target; a per-target setting in collie.toml if a suite needs more
    const TEST_LIMIT_MS: u64 = 30 * 60 * 1000;
    match timeout_ms.or((kind == "test").then_some(TEST_LIMIT_MS)) {
        Some(0) | None => None,
        Some(ms) => Some(Duration::from_millis(ms)),
    }
}

/// After `limit`, flags process `id` timed out and kills its group, unless `ended` comes first (the run phase ended).
/// `finish` turns the flag into the `timed-out` state.
async fn time_out(d: D, id: String, limit: Duration, mut ended: oneshot::Receiver<()>) {
    tokio::select! {
        _ = sleep(limit) => {}
        _ = &mut ended => return,
    }
    // The run phase may not have spawned yet (a limit of a few ms): wait for it.
    let pid = loop {
        {
            let mut st = d.lock();
            match st.procs.iter_mut().find(|p| p.id == id) {
                Some(p) if p.state == St::Running && p.pid.is_some() => {
                    p.timed_out = true;
                    break p.pid;
                }
                Some(p) if !p.ended() => {}
                _ => return,
            }
        }
        tokio::select! {
            _ = sleep(Duration::from_millis(20)) => {}
            _ = &mut ended => return,
        }
    };
    if let Some(pid) = pid {
        kill_group(pid).await;
    }
}

async fn detect(cwd: &Path) -> Result<Vec<Target>> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || eco::detect_all(&cwd)).await?
}

// ---- simulators

/// The repo a simulator choice is stored under: its git common dir, so all worktrees of a repo share it.
fn repo_of(dir: &Path) -> String {
    git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).unwrap_or_else(|| dir.to_string_lossy().into())
}

/// `git <args>`'s stdout in `dir`, trimmed; None when it fails or prints nothing.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (o.status.success() && !out.is_empty()).then_some(out)
}

/// The worktree `dir` is in (its git toplevel), else `dir`.
fn worktree(dir: &Path) -> PathBuf {
    git(dir, &["rev-parse", "--show-toplevel"]).map_or_else(|| dir.to_path_buf(), PathBuf::from)
}

/// `t`'s repo key in `sims.json`, and its stored choice there (`{"device": id, "runtime": id, "head": bool}`, just
/// `{"head": bool}` before a kind is picked, or null; plus `"physical": udid` while a physical device is chosen).
fn stored(path: &Path, t: &Target) -> (String, Value) {
    let repo = repo_of(&t.dir);
    let all: Value = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let choice = all[&repo][&t.name].clone();
    (repo, choice)
}

/// Held across `sim.set`'s read of a choice and its write back, so concurrent sets don't lose each other's change.
static SIMS: Mutex<()> = Mutex::new(());

/// Held across a whole `sim.template` save or removal, which awaits simctl, hence a tokio one.
static TEMPLATES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn store(path: &Path, repo: &str, target: &str, choice: Option<Value>) -> Result<()> {
    let mut all: Value = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    match choice {
        Some(c) => all[repo][target] = c,
        None => _ = all[repo].as_object_mut().map(|m| m.remove(target)),
    }
    if all[repo].as_object().is_some_and(|m| m.is_empty()) {
        all.as_object_mut().map(|m| m.remove(repo));
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&all)?)?;
    Ok(std::fs::rename(&tmp, path)?)
}

/// What `sim.set` stores: `head`, plus the kind only once one has been picked, so `--head` alone keeps following the
/// default kind; plus `physical` (the UDID `sim.set` resolved, else the stored one unless a kind is picked now).
/// The kind is kept while a device is chosen, so switching back restores it. `k` is the kind resolved from `p`'s
/// device or runtime; needed only when `p` picks one. `slim` (`p`'s `"on"`, `"off"` or an except list) is kept likewise.
fn next_choice(choice: &Value, p: &Value, k: Option<&SimKind>, physical: Option<&str>) -> Value {
    let head = p["head"].as_bool().unwrap_or(choice["head"] == true);
    let picked = p["device"].is_string() || p["runtime"].is_string();
    let mut next = match (picked.then_some(k).flatten(), choice["device"].is_string()) {
        (Some(k), _) => json!({"device": k.device, "runtime": k.runtime, "head": head}),
        (None, true) => json!({"device": choice["device"], "runtime": choice["runtime"], "head": head}),
        (None, false) => json!({"head": head}),
    };
    if let Some(u) = physical.or(choice["physical"].as_str().filter(|_| !picked)) {
        next["physical"] = u.into();
    }
    // The template outlives any change of kind: a run of another kind just doesn't clone it.
    if choice["template"].is_object() {
        next["template"] = choice["template"].clone();
    }
    let slim = next_slim(choice, p);
    if !slim.is_null() {
        next["slim"] = slim;
    }
    next
}

/// The slim setting `sim.set` stores: `p`'s (`"off"`: none), else the stored one.
fn next_slim(choice: &Value, p: &Value) -> Value {
    match p["slim"].as_str() {
        Some("off") => Value::Null,
        Some(set) => json!(except_list(set)),
        None => choice["slim"].clone(),
    }
}

/// An except list as typed (`on`: none, `icloud,web`), sorted so equal settings compare equal.
fn except_list(set: &str) -> Vec<String> {
    let mut l: Vec<String> = set.split(',').map(str::trim).filter(|c| !c.is_empty() && *c != "on").map(String::from).collect();
    l.sort();
    l.dedup();
    l
}

/// A run's slim setting: its own `slim` param (`false`: stock; an array: slim but for those categories) over the
/// target's stored one; stock with neither. On a physical device a stored setting just doesn't apply, and a param
/// is `bad_request`: simslim is simulator-only.
fn slim_for(param: &Value, stored: &Value, physical: bool) -> Result<Option<Vec<String>>> {
    if physical {
        if !param.is_null() {
            bail!(coded("bad_request", "simslim is simulator-only: --slim and --stock don't apply to a physical device"));
        }
        return Ok(None);
    }
    let v = if param.is_null() { stored } else { param };
    let list = v.as_array().map(|a| a.iter().map(|c| c.as_str().map(String::from)).collect::<Option<Vec<_>>>());
    match (v, list) {
        (Value::Null | Value::Bool(false), _) => Ok(None),
        (_, Some(Some(mut l))) => {
            l.sort();
            l.dedup();
            Ok(Some(l))
        }
        _ => bail!(coded("bad_request", format!("slim is {v}: it takes false (stock) or an array of categories to keep ([]: slim everything)"))),
    }
}

/// Checks an except list against the categories simslim knows; code `bad_request` naming them for an unknown one,
/// `simslim_missing` when it isn't installed.
fn check_slim(pf: &dyn Platform, except: &[String]) -> Result<()> {
    let known = pf.slim_categories()?;
    if let Some(bad) = except.iter().find(|c| !known.contains(c)) {
        bail!(coded("bad_request", format!("no simslim category {bad}; there are: {}", known.join(", "))));
    }
    Ok(())
}

/// `targets`' and `sim.set`'s `physical`: the chosen device as `sims` lists it, or a stand-in when it isn't listed.
fn physical_json(list: &[eco::Physical], udid: &str) -> Value {
    match list.iter().find(|d| d.udid == udid) {
        Some(d) => json!(d),
        None => json!({"udid": udid, "name": udid, "state": "unavailable"}),
    }
}

/// `targets`' `sim`: the kind a run without overrides uses. Names fall back to ids when it isn't installed.
fn sim_json(pf: &dyn Platform, rts: &Result<Vec<Value>>, choice: &Value) -> Value {
    match rts.as_ref().map(|r| pf.resolve(r, choice, None, None)) {
        Ok(Ok(k)) => {
            let mut v = json!(k);
            v["default"] = choice["device"].is_null().into();
            v["installed"] = true.into();
            v["head"] = (choice["head"] == true).into();
            v["template"] = choice["template"].is_object().into();
            v["slim"] = choice["slim"].clone();
            v
        }
        _ if choice["device"].is_null() => Value::Null,
        _ => {
            let (dev, rt, head, template) = (&choice["device"], &choice["runtime"], choice["head"] == true, choice["template"].is_object());
            json!({"device": dev, "device_name": dev, "runtime": rt, "runtime_name": rt, "default": false, "installed": false, "head": head, "template": template, "slim": choice["slim"]})
        }
    }
}

/// Where a run of `t` goes: the stored physical device (checked ready) unless the run overrides the simulator kind,
/// else the simulator kind (the overrides, else the stored choice, else the default) and whether it shows.
/// Also the target's template, for a new simulator to clone.
async fn destination(d: &D, pf: &'static dyn Platform, t: &Target, device: Option<&str>, runtime: Option<&str>, dev: Option<String>) -> Result<(Option<SimKind>, Option<Sim>, bool, Option<Sim>)> {
    let (path, t, device, runtime) = (d.dir.join("sims.json"), t.clone(), device.map(String::from), runtime.map(String::from));
    tokio::task::spawn_blocking(move || {
        let choice = stored(&path, &t).1;
        if let (Some(u), None, None) = (choice["physical"].as_str(), &device, &runtime) {
            return Ok((None, Some(pf.find_physical(&pf.physical_devices()?, u, true)?.sim()), false, None));
        }
        let kind = pf.resolve(&pf.runtimes(dev.as_deref())?, &choice, device.as_deref(), runtime.as_deref())?;
        Ok((Some(kind), None, choice["head"] == true, serde_json::from_value(choice["template"].clone()).ok()))
    })
    .await?
}

/// Creates a fresh `collie <lease id>` device of `kind` on `pf` (with `dev`'s toolchain, if given) and leases it to
/// `holder`. With a `template` (the target's) of the same kind, the device is a clone of it; the note says which
/// happened. With `slim`, it is then slimmed (about a minute; a clone too, since a clone of a slim device is stock)
/// and left booted. The last value is how slimming went: when it failed, the lease still holds the device, in
/// whatever state the platform left it.
#[allow(clippy::too_many_arguments)]
async fn new_sim(
    d: &D,
    pf: &'static dyn Platform,
    holder: &str,
    agent: Option<String>,
    kind: SimKind,
    run: bool,
    dev: Option<String>,
    template: Option<Sim>,
    slim: Option<Vec<String>>,
) -> Result<(Lease, Option<(Level, String)>, Result<()>)> {
    // Created under a unique name recorded first, then renamed once its UDID is recorded: a daemon killed in between
    // leaves a device the next start can find by one or the other. Lease ids repeat across daemons, so `collie <id>`
    // alone could name another daemon's device.
    let (id, temp) = {
        let mut st = d.lock();
        st.leases.creating += 1;
        let id = st.leases.reserve();
        let temp = format!("collie {id} creating {}-{}", std::process::id(), now_ms());
        st.leases.pending.push(temp.clone());
        st.leases.save();
        (id, temp)
    };
    let (t, k, dv) = (temp.clone(), kind.clone(), dev.clone());
    let sim = tokio::task::spawn_blocking(move || {
        let (mut sim, note) = match template_for(template.as_ref(), &k) {
            Some(src) => match pf.clone_device(src, &t, dv.as_deref()) {
                Ok(sim) => (sim, Some((Level::Info, format!("collie: simulator cloned from template {} ({})", src.name, src.udid)))),
                Err(e) => {
                    let warn = format!("collie: cloning template {} failed, so this simulator starts empty: {e:#}", src.name);
                    (pf.create(&t, &k, dv.as_deref())?, Some((Level::Warn, warn)))
                }
            },
            None => {
                let warn = template.map(|s| {
                    let want = format!("{} · {}", k.device_name, k.runtime_name);
                    (Level::Warn, format!("collie: the template is {} · {}, this run wants {want}, so this simulator starts empty", s.device, s.runtime))
                });
                (pf.create(&t, &k, dv.as_deref())?, warn)
            }
        };
        let slimmed = slim.as_ref().map_or(Ok(()), |except| pf.slim(&mut sim, except, dv.as_deref()));
        Ok::<_, anyhow::Error>((sim, note, slimmed))
    })
    .await;
    let (note, slimmed);
    let mut l = {
        let mut st = d.lock();
        st.leases.creating -= 1;
        st.leases.pending.retain(|n| *n != temp);
        let mut sim = match sim.map_err(anyhow::Error::from).and_then(|r| r) {
            Ok((sim, n, s)) => {
                (note, slimmed) = (n, s);
                sim
            }
            Err(e) => {
                st.leases.save();
                return Err(e);
            }
        };
        sim.name = format!("collie {id}");
        // One save (in grant) drops the pending name and records the UDID together, so no kill finds it in neither.
        st.leases.grant(id, holder, agent, None, Some(sim), None, run, now_ms())
    };
    let (udid, name) = l.simulator.as_ref().map(|s| (s.udid.clone(), s.name.clone())).unwrap_or_default();
    if let Err(e) = tokio::task::spawn_blocking(move || pf.rename(&udid, &name, dev.as_deref())).await? {
        // The device works under its temporary name and is retired by UDID either way; show the name it really has.
        eprintln!("warn: {e:#}");
        if let Some(s) = l.simulator.as_mut() {
            s.name = temp;
        }
        let mut st = d.lock();
        if let Some(x) = st.leases.leases.iter_mut().find(|x| x.id == l.id) {
            x.simulator = l.simulator.clone();
        }
        st.leases.save();
    }
    d.changed();
    Ok((l, note, slimmed))
}

/// The template a new simulator of `kind` clones: only one of the very same device and runtime, since snapshot tests
/// pin both.
fn template_for<'a>(template: Option<&'a Sim>, kind: &SimKind) -> Option<&'a Sim> {
    template.filter(|s| s.device == kind.device_name && s.runtime == kind.runtime_name)
}

/// Records `template` (None: none) as `t`'s in `sims.json`, keeping the rest of its stored choice; returns the one it
/// replaces, whose device the caller deletes.
fn store_template(path: &Path, t: &Target, template: Option<&Sim>) -> Result<Option<Sim>> {
    let _sims = SIMS.lock().unwrap();
    let (repo, mut choice) = stored(path, t);
    let old = serde_json::from_value(choice["template"].clone()).ok();
    match template {
        Some(s) => choice["template"] = json!(s),
        None => _ = choice.as_object_mut().map(|m| m.remove("template")),
    }
    let empty = choice.as_object().is_none_or(|m| m.is_empty());
    store(path, &repo, &t.name, (!empty).then_some(choice))?;
    Ok(old)
}

/// Saves the simulator of `holder`'s latest process of `t` as `t`'s template: stops that process, clones the device
/// (shut down) into `collie template <t>`, and releases the original. Code `not_found` when `holder` has none.
async fn save_template(d: &D, t: &Target, holder: &str, env: &HashMap<String, String>) -> Result<Value> {
    let (pid, lease) = {
        let mut st = d.lock();
        let p = st.procs.iter().rev().find(|p| p.holder == holder && p.t.dir == t.dir && p.t.name == t.name && p.simulator.is_some());
        let pid = p.map(|p| p.id.clone()).ok_or_else(|| coded("not_found", format!("you hold no simulator of {}: run it first", t.name)))?;
        // Taken out of the process first, so stopping it doesn't retire the device.
        let lease = carry(&mut st, |p| p.id == pid, |l| l.simulator.is_some()).ok_or_else(|| coded("not_found", format!("{} holds no simulator", t.name)))?;
        // Free-standing dibs from here on: a daemon killed mid-save keeps the user's simulator instead of retiring it
        // with the run's leases; a successful save releases it.
        if let Some(l) = st.leases.leases.iter_mut().find(|l| l.id == lease.id) {
            l.run = false;
        }
        st.leases.save();
        (pid, lease)
    };
    stop_where(d, |p| p.id == pid).await;
    let src = lease.simulator.clone().context("a simulator lease")?;
    let pf = eco::platform(&src.platform).with_context(|| format!("no platform {}", src.platform))?;
    let dev = eco::caller_developer_dir(&t.dir, env);
    // Named uniquely first and recorded, as `new_sim` does: a daemon killed mid-clone leaves a device the next start
    // finds by that name and deletes.
    let temp = format!("collie template creating {}-{}", std::process::id(), now_ms());
    {
        let mut st = d.lock();
        st.leases.pending.push(temp.clone());
        st.leases.save();
    }
    let (s, n, dv) = (src.clone(), temp.clone(), dev.clone());
    let cloned = tokio::task::spawn_blocking(move || pf.clone_device(&s, &n, dv.as_deref())).await;
    // Recorded in sims.json while still pending: a kill then leaves it found by either, and the next start's sweep
    // spares a pending device that sims.json stores as a template. The replaced one is queued in `devices` first, so a
    // kill can't orphan it either (the next start retires it unless sims.json still stores it).
    let mut queued = None;
    let stored = match cloned.map_err(anyhow::Error::from).and_then(|r| r) {
        Ok(sim) => {
            let (path, tt, tm) = (d.dir.join("sims.json"), t.clone(), sim.clone());
            queued = tokio::task::spawn_blocking(move || stored(&path, &tt).1["template"]["udid"].as_str().map(String::from)).await?;
            if let Some(u) = &queued {
                queue_retirement(d, u, false);
            }
            let path = d.dir.join("sims.json");
            let tt = t.clone();
            tokio::task::spawn_blocking(move || store_template(&path, &tt, Some(&tm))).await?.map(|old| (sim, old))
        }
        Err(e) => Err(e),
    };
    {
        let mut st = d.lock();
        st.leases.pending.retain(|n| *n != temp);
        st.leases.save();
    }
    let (mut template, old) = match stored {
        Ok(stored) => stored,
        Err(e) => {
            // The user's prepared simulator stays theirs (free-standing since the carry), and the template still stored
            // is no longer queued, or every graceful stop would wait on it.
            let mut st = d.lock();
            if let Some(u) = &queued {
                st.leases.devices.retain(|x| x != u);
            }
            st.leases.save();
            drop(st);
            d.changed();
            let id = lease.id;
            bail!("saving the template failed: {e:#}; your simulator is kept as dibs {id} (`collie dibs release {id}` when done)");
        }
    };
    if let Some(old) = old.filter(|o| o.udid != template.udid) {
        queue_retirement(d, &old.udid, true);
    }
    // Cosmetic: everything goes by UDID.
    let name = format!("collie template {}", t.name);
    let (u, n, dv) = (template.udid.clone(), name.clone(), dev);
    match tokio::task::spawn_blocking(move || pf.rename(&u, &n, dv.as_deref())).await? {
        Ok(()) => {
            template.name = name;
            let (path, tt, tm) = (d.dir.join("sims.json"), t.clone(), template.clone());
            tokio::task::spawn_blocking(move || store_template(&path, &tt, Some(&tm))).await??;
        }
        Err(e) => eprintln!("warn: {e:#}"),
    }
    drop_leases(d, |ls| ls.release(lease.id, now_ms())).await;
    Ok(json!({"target": t.name, "template": template}))
}

/// Queues simulator `udid` (a replaced or removed template) for deletion through `devices`, as created devices are:
/// with `now`, deleted right away; else recorded only, so a kill before the caller's next step leaves it to the next
/// start (which retires it unless sims.json still stores it as a template).
fn queue_retirement(d: &D, udid: &str, now: bool) {
    let mut st = d.lock();
    if !st.leases.devices.iter().any(|u| u == udid) {
        st.leases.devices.push(udid.into());
        st.leases.save();
    }
    if let (true, Some(tx)) = (now, &st.leases.retire) {
        let _ = tx.send(udid.into());
    }
}

/// The UDIDs `sims.json` (its parsed content) stores as templates; restarts never retire them.
fn template_udids(sims: &Value) -> Vec<String> {
    let choices = sims.as_object().into_iter().flat_map(|m| m.values()).flat_map(|t| t.as_object().into_iter().flat_map(|m| m.values()));
    choices.filter_map(|c| c["template"]["udid"].as_str().map(String::from)).collect()
}

// ---- services

/// `docker <args>`'s trimmed stdout; an error with its stderr when it fails.
fn docker(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("docker").args(args).output().map_err(|e| coded("no_docker", format!("docker: {e}; install Docker Desktop")))?;
    ensure!(out.status.success(), "docker {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim().into())
}

/// Starts a throwaway `kind` (`postgres` or `redis`) container of `image` (default `postgres:17`, `redis:7`) and leases
/// its host port to `holder` once it accepts connections. `limit` bounds a pull, then the wait until it is ready.
async fn new_service(d: &D, holder: &str, agent: Option<String>, kind: &str, image: Option<&str>, limit: Duration) -> Result<Lease> {
    // `-h 127.0.0.1`: postgres' init server listens on its Unix socket only, so this waits for the real one.
    let (inner, default, ready): (u16, &str, &'static [&'static str]) = match kind {
        "postgres" => (5432, "postgres:17", &["pg_isready", "-h", "127.0.0.1", "-U", "collie", "-d", "collie"]),
        _ => (6379, "redis:7", &["redis-cli", "ping"]),
    };
    let image = image.unwrap_or(default).to_string();
    if tokio::task::spawn_blocking(|| docker(&["info", "--format", "{{.ServerVersion}}"])).await?.is_err() {
        bail!(coded("no_docker", "Docker isn't running: ask the user to start Docker Desktop (collie never starts it)"));
    }
    let img = image.clone();
    if tokio::task::spawn_blocking(move || docker(&["image", "inspect", "--format", "{{.Id}}", &img])).await?.is_err() {
        let img = image.clone();
        let pull = tokio::task::spawn_blocking(move || docker(&["pull", "--quiet", &img]));
        match tokio::time::timeout(limit, pull).await {
            Err(_) => bail!(coded("timeout", format!("still pulling {image} after {} ms; the pull goes on, so acquire again in a while", limit.as_millis()))),
            Ok(pulled) => _ = pulled?.with_context(|| format!("pulling {image}"))?,
        }
    }
    // The id and port are taken under the lock and granted under it; docker runs outside it.
    let (id, port) = {
        let mut st = d.lock();
        let port = st.leases.pick(None, false).ok_or_else(|| coded("unavailable", "no free port in 3000-3999"))?;
        st.leases.binding.push(port);
        (st.leases.reserve(), port)
    };
    // Lease ids repeat across daemons; the start time keeps a killed daemon's orphan (swept at the next start, maybe
    // after this one runs) from sharing this name.
    let container = format!("collie-{id}-{}", now_ms());
    let started: Result<()> = async {
        let mut args = vec!["run", "-d", "--rm", "--name", &container].into_iter().map(String::from).collect::<Vec<_>>();
        args.extend(["--label".into(), format!("collie={}", d.dir.display()), "-p".into(), format!("127.0.0.1:{port}:{inner}")]);
        if kind == "postgres" {
            args.extend(["POSTGRES_USER", "POSTGRES_PASSWORD", "POSTGRES_DB"].iter().flat_map(|k| ["-e".into(), format!("{k}=collie")]));
        }
        args.push(image.clone());
        tokio::task::spawn_blocking(move || docker(&args.iter().map(String::as_str).collect::<Vec<_>>())).await??;
        let deadline = Instant::now() + limit;
        loop {
            let c = container.clone();
            let probe = tokio::task::spawn_blocking(move || docker(&[&["exec", c.as_str()], ready].concat())).await?;
            if probe.is_ok_and(|out| kind != "redis" || out == "PONG") {
                return Ok(());
            }
            let c = container.clone();
            // `--rm` removes it once it exits, so a crash while starting shows as a missing container.
            if tokio::task::spawn_blocking(move || docker(&["container", "inspect", "--format", "{{.State.Status}}", &c])).await?.is_err() {
                bail!("{container} ({image}) exited while starting; check the image");
            }
            if Instant::now() >= deadline {
                bail!(coded("timeout", format!("{container} ({image}) not ready within {} ms", limit.as_millis())));
            }
            sleep(Duration::from_millis(500)).await;
        }
    }
    .await;
    d.lock().leases.binding.retain(|p| *p != port);
    if let Err(e) = started {
        let c = container.clone();
        let _ = tokio::task::spawn_blocking(move || docker(&["rm", "-f", &c])).await;
        return Err(e);
    }
    let url = match kind {
        "postgres" => format!("postgres://collie:collie@127.0.0.1:{port}/collie"),
        _ => format!("redis://127.0.0.1:{port}"),
    };
    let l = {
        let mut st = d.lock();
        st.leases.grant(id, holder, agent, Some(port), None, None, false, now_ms());
        let l = st.leases.leases.iter_mut().find(|l| l.id == id).context("lease just granted")?;
        (l.resource, l.service) = (kind.into(), Some(Service { image, container, url }));
        let l = l.clone();
        st.leases.save();
        l
    };
    d.changed();
    Ok(l)
}

/// Removes the containers of released service leases.
async fn remove_containers(d: D, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(c) = rx.recv().await {
        let name = c.clone();
        match tokio::task::spawn_blocking(move || docker(&["rm", "-f", &name])).await {
            Ok(Err(e)) if !format!("{e:#}").contains("No such container") => eprintln!("warn: {c}: {e:#}"),
            _ => {}
        }
        let mut st = d.lock();
        st.leases.removing = st.leases.removing.saturating_sub(1);
    }
}

/// The containers in `names` (`docker ps -a` names of this daemon's label, one per line) that no lease holds: left by
/// a daemon killed while starting or releasing one.
fn orphans(names: &str, leases: &[Lease]) -> Vec<String> {
    let held = |n: &str| leases.iter().any(|l| l.service.as_ref().is_some_and(|s| s.container == n));
    names.lines().map(str::trim).filter(|n| !n.is_empty() && !held(n)).map(Into::into).collect()
}

/// Takes the lease `fits` picks (a simulator still of the wanted kind, or the chosen physical device) out of a process
/// `from` picks (the holder's own for this target), so stopping that process for a restart keeps it, and a simulator
/// stays booted with the app's data. Renews it: each run gets a fresh TTL.
fn carry(st: &mut State, from: impl Fn(&Proc) -> bool, fits: impl Fn(&Lease) -> bool) -> Option<Lease> {
    let State { procs, leases, .. } = st;
    procs.iter_mut().filter(|p| from(p)).find_map(|p| {
        let id = leases.leases.iter().find(|l| p.leases.contains(&l.id) && fits(l))?.id;
        p.leases.retain(|l| *l != id);
        leases.renew(id, now_ms())
    })
}

/// `carry`'s `from` for a simulator run of `t` by `holder`: the holder's own process of `t`, and for a test also its
/// ended test processes in `t`'s dir (any test target), so a worktree's test runs share one booted device in turn.
/// Never another test target's running process.
fn sim_source<'a>(t: &'a Target, holder: &'a str) -> impl Fn(&Proc) -> bool + 'a {
    move |p| p.holder == holder && p.t.dir == t.dir && (p.t.name == t.name || t.kind == "test" && p.t.kind == "test" && p.ended())
}

/// `carry`'s pick for a simulator run: the device must still be of the wanted kind and slim setting. A slim device
/// whose runtime doesn't keep it across a reboot (before iOS 18.5) never fits: it may have rebooted stock since.
// ponytail: so re-runs there slim a new device each time (about a minute); verify on carry if that bites.
fn sim_fits<'a>(kind: &'a SimKind, slim: &'a Option<Vec<String>>) -> impl Fn(&Lease) -> bool + 'a {
    move |l| {
        l.simulator.as_ref().is_some_and(|s| {
            let persists = || eco::platform(&s.platform).is_some_and(|pf| pf.slim_persists(&s.runtime));
            s.device == kind.device_name && s.runtime == kind.runtime_name && s.slim == *slim && (slim.is_none() || persists())
        })
    }
}

/// The lease a run of `name` takes on physical device `dev`: carried from `holder`'s own process for this target (with
/// `take`, from anyone's), else `holder`'s free-standing lease on it (from `dibs acquire device`), else a new one.
/// Code `busy` while any other lease has the device; a run doesn't queue for it.
fn lease_device(st: &mut State, same: impl Fn(&Proc) -> bool, holder: &str, agent: Option<String>, take: bool, dev: &Sim, name: &str) -> Result<Lease> {
    let on = |l: &Lease| l.device.as_ref().is_some_and(|s| s.udid == dev.udid);
    if let Some(l) = carry(st, |p| same(p) && (take || p.holder == holder), on) {
        // A take-over hands the device over with the target, so no waiter gets it while the old process stops.
        let Some(x) = st.leases.leases.iter_mut().find(|x| x.id == l.id && x.holder != holder) else { return Ok(l) };
        (x.holder, x.agent) = (holder.into(), agent);
        let x = x.clone();
        st.leases.save();
        return Ok(x);
    }
    let State { procs, leases, .. } = st;
    let free_standing = |l: &Lease| l.holder == holder && !procs.iter().any(|p| p.leases.contains(&l.id));
    if let Some(i) = leases.leases.iter().position(|l| on(l) && free_standing(l)) {
        // Now the process's: released when it ends, and dropped by a daemon restart like any run lease.
        let l = &mut leases.leases[i];
        (l.run, l.expires_at) = (true, now_ms() + TTL_MS);
        let l = l.clone();
        leases.save();
        return Ok(l);
    }
    if let Some(l) = leases.device_lease(&dev.udid) {
        bail!(coded(
            "busy",
            format!(
                "{}: dibs held by {}; the CLI's `collie run`/`exec` queue for it (unless --no-queue), or run on a simulator this once with `collie run \"{name}\" --runtime <version>`",
                dev.name,
                who(&l.holder, &l.agent)
            )
        ));
    }
    let id = leases.reserve();
    Ok(leases.grant(id, holder, agent, None, None, Some(dev.clone()), true, now_ms()))
}

/// `lease_device`; or, when that is `busy` and the run may queue (`queue`: its process id; the CLI's runs, never the
/// pane's), a waiter for the device tagged with the process, which `lifecycle` awaits before the run phase.
#[allow(clippy::too_many_arguments)]
fn claim_device(st: &mut State, same: impl Fn(&Proc) -> bool, holder: &str, agent: Option<String>, take: bool, dev: &Sim, name: &str, queue: Option<&str>) -> Result<(Option<Lease>, Option<oneshot::Receiver<Lease>>)> {
    match lease_device(st, same, holder, agent.clone(), take, dev, name) {
        Err(e) if queue.is_some() && e.downcast_ref::<Coded>().is_some_and(|c| c.0 == "busy") => {
            let (_, rx) = st.leases.enqueue(holder, agent, None, Some(dev.clone()), queue.map(String::from), now_ms());
            Ok((None, Some(rx)))
        }
        r => r.map(|l| (Some(l), None)),
    }
}

/// Makes the lease a queued process `id` was handed its run lease (released with it, dropped by a daemon restart),
/// renewed. False when the process is gone (the lease is released) or the lease is (it expired while the build ran).
fn adopt(st: &mut State, id: &str, l: &Lease) -> bool {
    let State { procs, leases, .. } = st;
    let Some(p) = procs.iter_mut().find(|p| p.id == id) else {
        leases.release(l.id, now_ms());
        return false;
    };
    let Some(x) = leases.leases.iter_mut().find(|x| x.id == l.id) else { return false };
    (x.run, x.expires_at, x.process) = (true, now_ms() + TTL_MS, None);
    p.leases.push(l.id);
    p.device = l.device.clone();
    leases.save();
    true
}

/// `args` with `{udid}` replaced by the leased destination's UDID; `bad_request` when one uses it and there is none.
fn with_udid(args: Vec<String>, udid: Option<&str>) -> Result<Vec<String>> {
    match udid {
        Some(u) => Ok(args.into_iter().map(|a| a.replace("{udid}", u)).collect()),
        None if args.iter().any(|a| a.contains("{udid}")) => bail!(coded("bad_request", "{udid} needs --sim or --physical")),
        None => Ok(args),
    }
}

/// `run`'s `simulator`/`physical`, a destination for a command: for exec and tasks only, one of the two, and
/// `device`/`runtime` (the simulator kind) only with `simulator`. Xcode targets choose theirs with `sim set`.
fn check_dest(t: &Target, p: &Value) -> Result<()> {
    let (sim, phys) = (p["simulator"] == true, p["physical"].is_string());
    let command = t.ecosystem == "task";
    if (sim || phys) && !command {
        bail!(coded("bad_request", format!("{}: --sim and --physical are for exec and tasks; an Xcode target's destination is `collie sims set`", t.name)));
    }
    if sim && phys {
        bail!(coded("bad_request", "--sim and --physical: pick one"));
    }
    if command && !sim && (p["device"].is_string() || p["runtime"].is_string()) {
        bail!(coded("bad_request", "--device and --runtime pick the simulator kind for --sim"));
    }
    if phys && !p["slim"].is_null() {
        bail!(coded("bad_request", "simslim is simulator-only: --slim and --stock don't go with --physical"));
    }
    Ok(())
}

/// Starts the target named `name` among those detected in `cwd`.
async fn run(d: &D, cwd: &Path, name: &str, holder: &str, p: &Value) -> Result<Value> {
    let mut t = detect(cwd).await?.into_iter().find(|t| t.name == name);
    // A fresh worktree of a generated project (Tuist, XcodeGen) has no schemes until its `[prepare]` runs.
    if t.is_none() && prepare_up_front(d, cwd, p).await? {
        t = detect(cwd).await?.into_iter().find(|t| t.name == name);
    }
    let t = t.ok_or_else(|| coded("not_found", format!("no target {name} in {}", cwd.display())))?;
    run_target(d, t, holder, p).await
}

/// Runs the `[prepare]` covering `cwd` when due, outside any process (there is no target to log under yet), with
/// `p`'s env. False when none is due; error `prepare_failed` with its output's tail when it fails.
async fn prepare_up_front(d: &D, cwd: &Path, p: &Value) -> Result<bool> {
    let _g = PREPARE.lock().await;
    let Some((root, prep)) = prepare_due(&d.dir, cwd)? else { return Ok(false) };
    let env: HashMap<String, String> = serde_json::from_value(p["env"].clone()).unwrap_or_default();
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", &prep.cmd]).current_dir(&root).envs(env).stdin(Stdio::null());
    let out = cmd.output().await.with_context(|| format!("prepare: {}", prep.cmd))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout);
        let tail: Vec<&str> = text.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(coded("prepare_failed", format!("prepare `{}` in {} failed ({}):\n{}", prep.cmd, root.display(), out.status, tail.join("\n"))));
    }
    prepared(&d.dir, &root);
    Ok(true)
}

/// Starts a process for `t` with `run`'s params: `take`, `env`, `device`, `runtime`, and `args` and `timeout_ms` for
/// the run phase.
async fn run_target(d: &D, t: Target, holder: &str, p: &Value) -> Result<Value> {
    let (take, device, runtime, name) = (p["take"] == true, p["device"].as_str(), p["runtime"].as_str(), t.name.clone());
    let env: HashMap<String, String> = serde_json::from_value(p["env"].clone()).unwrap_or_default();
    let args: Vec<String> = serde_json::from_value(p["args"].clone()).unwrap_or_default();
    let limit = run_limit(&t.kind, p["timeout_ms"].as_u64());
    let e = eco::get(&t.ecosystem);
    e.check_args(&t, &args)?;
    let only: Vec<String> = serde_json::from_value(p["only"].clone()).unwrap_or_default();
    let args = only_args(e, &t, &only, args)?;
    check_dest(&t, p)?;
    let needs = e.needs(&t);
    let pf = || e.platform().ok_or_else(|| coded("bad_request", format!("{name}: {} targets have no devices", t.ecosystem)));
    // Before any lease is taken, so a kind that isn't installed fails without side effects.
    let dev = eco::caller_developer_dir(&t.dir, &env);
    let (kind, phone, head, template) = match (needs.contains(&Resource::Simulator), p["simulator"] == true, p["physical"].as_str()) {
        (true, ..) => destination(d, pf()?, &t, device, runtime, dev.clone()).await?,
        (false, true, _) => {
            let (pf, dv, (device, runtime)) = (pf()?, dev.clone(), (device.map(String::from), runtime.map(String::from)));
            let kind = tokio::task::spawn_blocking(move || pf.resolve(&pf.runtimes(dv.as_deref())?, &Value::Null, device.as_deref(), runtime.as_deref()));
            (Some(kind.await??), None, false, None)
        }
        (false, false, Some(q)) => {
            let pf = pf()?;
            let list = tokio::task::spawn_blocking(move || pf.physical_devices()).await??;
            (None, Some(pf.find_physical(&list, q, false)?.sim()), false, None)
        }
        (false, false, None) => (None, None, false, None),
    };
    if kind.is_none() && phone.is_none() {
        with_udid(args.clone(), None)?;
    }
    // Stored only for a target that runs on a simulator; an exec's or task's `--sim` has the run's flag alone.
    let stored = match needs.contains(&Resource::Simulator) {
        true => {
            let (path, st) = (d.dir.join("sims.json"), t.clone());
            tokio::task::spawn_blocking(move || stored(&path, &st).1["slim"].clone()).await?
        }
        false => Value::Null,
    };
    let slim = slim_for(&p["slim"], &stored, phone.is_some())?;
    if !p["slim"].is_null() && kind.is_none() && phone.is_none() {
        bail!(coded("bad_request", "--slim and --stock are for a simulator: an iOS target's, or --sim's"));
    }
    if let Some(except) = slim.clone() {
        let pf = pf()?;
        tokio::task::spawn_blocking(move || check_slim(pf, &except)).await??;
    }
    let queue = p["queue"] == true;
    let same = |p: &Proc| p.t.dir == t.dir && p.t.name == t.name;
    let now = now_ms();
    let id = format!("{}-{now}", t.name.replace('/', "_"));
    let mut log = LogFile::open(&d.log_path(&id))?;
    let agent = agent_name(holder).await;
    let bdir = t.dir.clone();
    let branch = tokio::task::spawn_blocking(move || git(&bdir, &["branch", "--show-current"])).await.ok().flatten();
    // The held check, the carry and a placeholder process happen under one lock, so a second run of this target
    // arriving while this one creates its device sees it: another holder gets `held`; the same holder restarts it.
    // A physical device is leased here too, needing no I/O: `busy` then leaves nothing behind, like `held`.
    let claim = {
        let mut st = d.lock();
        let p = st.procs.iter().find(|p| same(p) && p.holder != holder && !p.ended());
        if st.stopping {
            Err(coded("stopping", "the daemon is stopping"))
        } else if let (Some(p), false) = (p, take) {
            Err(coded("held", format!("{name} is held by {}; pass --take to take it over", who(&p.holder, &p.agent))))
        } else {
            let kept = match (&kind, &phone) {
                (Some(k), _) => Ok((carry(&mut st, sim_source(&t, holder), sim_fits(k, &slim)), None)),
                (None, Some(dev)) => claim_device(&mut st, same, holder, agent.clone(), take, dev, &name, queue.then_some(id.as_str())),
                (None, None) => Ok((None, None)),
            };
            kept.inspect(|(kept, queued)| {
                st.procs.push(Proc {
                    id: id.clone(),
                    target: t.name.clone(),
                    kind: t.kind.clone(),
                    dir: t.dir.clone(),
                    state: St::Building,
                    code: None,
                    signal: None,
                    pid: None,
                    holder: holder.into(),
                    agent: agent.clone(),
                    leases: kept.iter().map(|l| l.id).collect(),
                    port: None,
                    simulator: kept.as_ref().and_then(|l| l.simulator.clone()),
                    // A queued run shows the device it waits for.
                    device: kept.as_ref().and_then(|l| l.device.clone()).or(queued.as_ref().and(phone.clone())),
                    since: now,
                    ended_at: None,
                    built: false,
                    launched: false,
                    blocked: false,
                    tests: None,
                    only: !only.is_empty(),
                    branch: branch.clone(),
                    problems: None,
                    timed_out: false,
                    t: t.clone(),
                })
            })
        }
    };
    let (kept, queued) = match claim {
        Ok(claimed) => claimed,
        Err(e) => {
            // Rejected before the process exists: its log would be an empty orphan.
            let _ = std::fs::remove_file(d.log_path(&id));
            return Err(e);
        }
    };
    d.changed();
    stop_where(d, |p| same(p) && p.id != id).await;

    let kept_id = kept.as_ref().map(|l| l.id);
    let mut granted: Vec<Lease> = kept.into_iter().collect();
    let got: Result<()> = async {
        if let (Some(k), true) = (kind, granted.is_empty()) {
            let started = now_ms();
            let (l, note, slimmed) = new_sim(d, pf()?, holder, agent.clone(), k, true, dev, template, slim.clone()).await?;
            if let Some((level, line)) = note {
                log.append(&Record { ts: now_ms(), source: Source::Build, level, line });
            }
            if let Some(except) = l.simulator.as_ref().and_then(|s| s.slim.as_ref()) {
                let but = if except.is_empty() { String::new() } else { format!(" except {}", except.join(",")) };
                let line = format!("collie: simulator created and slimmed{but} in {}", crate::tui::ago(now_ms() - started));
                log.append(&Record { ts: now_ms(), source: Source::Build, level: Level::Info, line });
            }
            if let Err(e) = slimmed {
                log.append(&Record { ts: now_ms(), source: Source::Build, level: Level::Error, line: format!("collie: {e:#}") });
                // Possibly half slimmed: retired now, never carried as stock.
                d.lock().leases.release(l.id, now_ms());
                d.changed();
                return Err(e);
            }
            granted.push(l);
        }
        for need in &needs {
            if let Resource::Port { preferred } = need {
                let mut st = d.lock();
                let port = st.leases.pick(*preferred, false).ok_or_else(|| coded("unavailable", "no free port in 3000-3999"))?;
                let lid = st.leases.reserve();
                granted.push(st.leases.grant(lid, holder, agent.clone(), Some(port), None, None, true, now_ms()));
            }
        }
        Ok(())
    }
    .await;

    let ids: Vec<u64> = granted.iter().map(|l| l.id).collect();
    let mut st = d.lock();
    let Some(p) = st.procs.iter_mut().find(|p| p.id == id) else {
        // Stopped, taken over or restarted while leasing: release what this run took, except a carried lease a newer
        // run of the same holder has carried on.
        let held: Vec<u64> = st.procs.iter().flat_map(|p| p.leases.clone()).collect();
        st.leases.remove(|l| ids.contains(&l.id) && !held.contains(&l.id), now_ms());
        drop(st);
        d.changed();
        return Err(got.err().unwrap_or_else(|| coded("stopped", format!("{name} was stopped or restarted while starting"))));
    };
    // Appends only what this run granted: the carried lease is already in `p.leases` unless a newer run of the same
    // holder carried it away meanwhile, and then it must stay away.
    p.leases.extend(ids.iter().filter(|id| Some(**id) != kept_id));
    let mine: Vec<&Lease> = granted.iter().filter(|l| p.leases.contains(&l.id)).collect();
    let (port, simulator) = (mine.iter().find_map(|l| l.port), mine.iter().find_map(|l| l.simulator.clone()));
    let device = mine.iter().find_map(|l| l.device.clone()).or(queued.as_ref().and(phone));
    (p.port, p.simulator, p.device) = (port, simulator.clone(), device.clone());
    if let Err(e) = got {
        // Ends like a failed build: the device stays with the ended process for the holder's next run (A1).
        drop(st);
        finish(d, &id, None);
        return Err(e);
    }
    let v = json!(p);
    drop(st);
    d.changed();
    tokio::spawn(lifecycle(d.clone(), id, t, env, port, simulator.map(|s| s.udid), device.map(|s| s.udid), head, queued, args, limit, Mutex::new(log)));
    Ok(v)
}

async fn wait(d: &D, cwd: &Path, target: Option<&str>, holder: &str, until: &str, timeout: Option<u64>) -> Result<Value> {
    let id = newest(d, cwd, target, holder)?;
    let deadline = timeout.map(|ms| Instant::now() + Duration::from_millis(ms));
    // ponytail: 100ms poll inside the daemon; drive it from the broadcast channel if latency matters
    loop {
        let p = d.lock().procs.iter().find(|p| p.id == id).cloned();
        let p = p.ok_or_else(|| coded("stopped", format!("{id} was stopped")))?;
        let done = match until {
            "built" => p.built,
            "running" => p.state == St::Running,
            "launched" => p.launched,
            "exited" => p.ended(),
            // Not `timed-out`: collie killed it, so it is no crash of its own.
            "crashed" => p.state == St::Crashed,
            "port-open" => p.state == St::Running && p.port.is_some_and(port_open),
            other => bail!(coded("bad_request", format!("unknown condition {other}"))),
        };
        if done {
            return Ok(json!(p));
        }
        if p.ended() {
            bail!(coded("ended", format!("{id} is {} (code {}, signal {}), will never be {until}", json!(p.state), json!(p.code), json!(p.signal))));
        }
        if until == "port-open" && p.port.is_none() {
            bail!(coded("no_port", format!("{id} holds no dibs on a port")));
        }
        if deadline.is_some_and(|dl| Instant::now() >= dl) {
            let last = logs::tail(&d.log_path(&id), 1, &Filter::default()).ok().and_then(|(r, _)| r.into_iter().next());
            let last = last.map_or("—".to_string(), |r| r.line.lines().next().unwrap_or_default().chars().take(160).collect());
            let state = json!(p.state);
            let elapsed = crate::tui::ago(now_ms().saturating_sub(p.since));
            if let (St::Queued, Some(dev)) = (p.state, &p.device) {
                let behind = d.lock().leases.device_lease(&dev.udid).map_or("—".to_string(), |l| who(&l.holder, &l.agent));
                bail!(coded("timeout", format!("{id} still queued for {} behind {behind} ({elapsed} elapsed); it keeps running: wait again", dev.name)));
            }
            bail!(coded("timeout", format!("{id} still {} ({elapsed} elapsed, last: {last}); it keeps running: wait again", state.as_str().unwrap_or_default())));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn filter_of(p: &Value) -> Result<Filter> {
    Ok(Filter { level: serde_json::from_value(p["level"].clone())?, source: serde_json::from_value(p["source"].clone())? })
}

fn read_logs(d: &D, p: &Value, cwd: &Path, holder: &str) -> Result<Value> {
    let filter = filter_of(p)?;
    let all = p["all"] == true;
    if all && (p["since"].is_string() || p["mark"] == true) {
        bail!(coded("bad_request", "`all` reads a whole run: drop `since` and `mark`"));
    }
    if let Some(c) = p["since"].as_str() {
        let (id, off) = logs::parse_cursor(c)?;
        let (mut recs, end) = logs::read(&d.log_path(&id), off)?;
        recs.retain(|r| filter.keep(r));
        let mut v = json!({"records": recs, "cursor": logs::cursor(&id, end)});
        if let Some(latest) = newer_run(d, &id) {
            v["stale"] = json!({"process": id, "latest": latest});
        }
        return Ok(v);
    }
    let id = match p["process"].as_str() {
        // An id names a file in the logs dir: no separators, so it can't name one elsewhere.
        Some(id) if id.contains('/') || id.starts_with('.') => bail!(coded("bad_request", format!("bad process id {id:?}"))),
        Some(id) if !d.log_path(id).exists() => bail!(coded("not_found", format!("no log for {id}; it may have been pruned"))),
        Some(id) => id.to_string(),
        None => newest(d, cwd, p["target"].as_str(), holder)?,
    };
    let path = d.log_path(&id);
    if p["mark"] == true {
        return Ok(json!({"cursor": logs::cursor(&id, logs::read(&path, 0)?.1)}));
    }
    let (recs, end) = match all {
        true => logs::read(&path, 0).map(|(mut recs, end)| {
            recs.retain(|r| filter.keep(r));
            (recs, end)
        })?,
        false => logs::tail(&path, TAIL, &filter)?,
    };
    Ok(json!({"records": recs, "cursor": logs::cursor(&id, end)}))
}

/// The newest process of the target that ran process `id` (`select`'s rule, without narrowing by cwd), when it is
/// another: a cursor taken before a restart still reads the old run. `id` is looked up live, then in the history,
/// where a restart (`stop_where`) moves it.
fn newer_run(d: &D, id: &str) -> Option<String> {
    let live = d.lock().procs.iter().find(|p| p.id == id).map(|p| (p.t.name.clone(), p.t.dir.clone()));
    // Read without the state lock held: the history file can be large.
    let (name, dir) = match live {
        Some(found) => found,
        None => {
            let runs = {
                let _g = HISTORY.lock().unwrap();
                read_history(&d.dir)
            };
            let r = runs.into_iter().find(|r| r["id"] == id)?;
            (r["target"].as_str()?.to_string(), PathBuf::from(r["dir"].as_str()?))
        }
    };
    let st = d.lock();
    let latest = st.procs.iter().rfind(|p| p.t.name == name && p.t.dir == dir)?;
    (latest.id != id).then(|| latest.id.clone())
}

async fn send(w: &mut OwnedWriteHalf, v: &Value) -> Result<()> {
    Ok(w.write_all(format!("{v}\n").as_bytes()).await?)
}

async fn follow(d: &D, p: &Value, cwd: &Path, holder: &str, w: &mut OwnedWriteHalf) -> Result<Value> {
    let mut rx = d.tx.subscribe();
    let filter = filter_of(p)?;
    let (mut id, mut off) = match p["since"].as_str() {
        // ponytail: stays on the cursor's process, with no `stale` warning when it has been restarted; warn as
        // `read_logs` does (`newer_run`) if agents follow stale cursors.
        Some(c) => logs::parse_cursor(c)?,
        None => {
            let id = newest(d, cwd, p["target"].as_str(), holder)?;
            let (recs, end) = logs::tail(&d.log_path(&id), TAIL, &filter)?;
            for r in recs {
                send(w, &json!({"event": "log", "data": r})).await?;
            }
            (id, end)
        }
    };
    loop {
        let (recs, end) = logs::read(&d.log_path(&id), off)?;
        off = end;
        for r in recs.iter().filter(|r| filter.keep(r)) {
            send(w, &json!({"event": "log", "data": r})).await?;
        }
        // Moves on to the newest process once the old one's output is drained and read to the end.
        if p["since"].is_null()
            && !d.lock().writing.contains(&id)
            && let Some(n) = newest(d, cwd, p["target"].as_str(), holder).ok().filter(|n| *n != id)
        {
            (id, off) = (n, 0);
            continue;
        }
        // Any broadcast (or lag) is just a wake-up; the file is the source of truth.
        if let Err(broadcast::error::RecvError::Closed) = rx.recv().await {
            bail!("daemon shutting down");
        }
    }
}

async fn subscribe(d: &D, w: &mut OwnedWriteHalf) -> Result<Value> {
    let mut rx = d.tx.subscribe();
    send(w, &json!({"event": "status", "data": d.snapshot()})).await?;
    loop {
        match rx.recv().await {
            Ok(ev) => send(w, &ev).await?,
            // `lagged`: the skipped events may include log lines, so clients reload what they show.
            Err(broadcast::error::RecvError::Lagged(_)) => send(w, &json!({"event": "status", "data": d.snapshot(), "lagged": true})).await?,
            Err(broadcast::error::RecvError::Closed) => bail!("daemon shutting down"),
        }
    }
}

async fn acquire(d: &D, p: &Value, holder: &str) -> Result<Value> {
    let device = match p["resource"].as_str().unwrap_or("port") {
        "port" => None,
        // A container is started for every service lease, so like a simulator it never queues.
        r @ ("postgres" | "redis") if p["preferred"].is_null() && p["wait"] != true => {
            let limit = Duration::from_millis(p["timeout_ms"].as_u64().unwrap_or(120_000));
            return Ok(json!(new_service(d, holder, agent_name(holder).await, r, p["image"].as_str(), limit).await?));
        }
        "postgres" | "redis" => bail!(coded("bad_request", "--preferred and --wait apply to ports and devices, not databases")),
        // ponytail: dibs name no target, so they are iOS's; a second platform would add a `platform` param here.
        "device" if p["preferred"].is_null() => {
            let q = p["device"].as_str().ok_or_else(|| coded("bad_request", "dibs on a device needs --device <udid|name>"))?.to_string();
            let list = tokio::task::spawn_blocking(|| eco::Ios.physical_devices()).await??;
            Some(eco::Ios.find_physical(&list, &q, false)?.sim())
        }
        // A device is created for every simulator lease, so it is always granted and never queues.
        "simulator" if p["preferred"].is_null() => {
            let (dev, rt) = (p["device"].as_str().map(String::from), p["runtime"].as_str().map(String::from));
            let kind = tokio::task::spawn_blocking(move || eco::Ios.resolve(&eco::Ios.runtimes(None)?, &Value::Null, dev.as_deref(), rt.as_deref()));
            let kind = kind.await??;
            return Ok(json!(new_sim(d, &eco::Ios, holder, agent_name(holder).await, kind, false, None, None, None).await?.0));
        }
        "simulator" | "device" => bail!(coded("bad_request", "preferred applies to ports only")),
        other => bail!(coded("bad_request", format!("unknown resource {other}"))),
    };
    let preferred = p["preferred"].as_u64().map(|n| n as u16);
    let wait = p["wait"] == true;
    let agent = agent_name(holder).await;
    let (granted, queued, by) = {
        let mut st = d.lock();
        let now = now_ms();
        match st.leases.acquire(holder, agent.clone(), preferred, wait, device.clone(), now) {
            Some(l) => (Some(l), None, None),
            None if wait => (None, Some(st.leases.enqueue(holder, agent, preferred, device.clone(), None, now)), None),
            None => (None, None, device.as_ref().and_then(|dev| st.leases.device_lease(&dev.udid)).map(|l| who(&l.holder, &l.agent))),
        }
    };
    d.changed();
    let (wid, mut rx) = match (granted, queued, device) {
        (Some(l), ..) => return Ok(json!(l)),
        (None, Some(q), _) => q,
        (_, _, Some(dev)) => bail!(coded("busy", format!("{}: dibs held by {}; pass --wait to queue for it", dev.name, by.unwrap_or_default()))),
        _ => bail!(coded("unavailable", "no free port in 3000-3999")),
    };
    let got = match p["timeout_ms"].as_u64() {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), &mut rx).await {
            Ok(r) => r.ok(),
            Err(_) => rx.try_recv().ok(), // granted in the same instant the timeout fired
        },
        None => rx.await.ok(),
    };
    if let Some(l) = got {
        return Ok(json!(l));
    }
    d.lock().leases.queue.retain(|w| w.id != wid);
    d.changed();
    bail!(coded("timeout", "dibs not granted before timeout"))
}

async fn call(d: &D, method: &str, p: &Value) -> Result<Value> {
    let holder = p["holder"].as_str().unwrap_or_default();
    let cwd = PathBuf::from(p["cwd"].as_str().unwrap_or_default());
    let target = p["target"].as_str();
    let id = || p["id"].as_u64().context("missing dibs id");
    match method {
        "targets" => {
            let ts = detect(&cwd).await?;
            let path = d.dir.join("sims.json");
            let ts = tokio::task::spawn_blocking(move || {
                // Each platform's lists, read once.
                let (mut rts, mut phys) = (HashMap::new(), HashMap::new());
                let ts: Vec<Value> = ts
                    .iter()
                    .map(|t| {
                        let e = eco::get(&t.ecosystem);
                        let needs = e.needs(t);
                        let mut v = json!(t);
                        if let (true, Some(pf)) = (needs.contains(&Resource::Simulator), e.platform()) {
                            let choice = stored(&path, t).1;
                            v["sim"] = sim_json(pf, rts.entry(pf.id()).or_insert_with(|| pf.runtimes(None)), &choice);
                            if let Some(u) = choice["physical"].as_str() {
                                v["sim"]["physical"] = physical_json(phys.entry(pf.id()).or_insert_with(|| pf.physical_devices().unwrap_or_default()), u);
                            }
                        }
                        v["needs"] = json!(needs);
                        v
                    })
                    .collect();
                ts
            });
            Ok(json!({"targets": ts.await?}))
        }
        "sims" => {
            let path = d.dir.join("sims.json");
            let sims = tokio::task::spawn_blocking(move || {
                // iOS's lists, in iOS's shape; a second platform would give `sims` a `platform` param.
                let pf = &eco::Ios;
                let mut v = eco::sims_json(&pf.runtimes(None)?);
                match pf.physical_devices() {
                    Ok(ds) => v["physical"] = json!(ds),
                    Err(e) => (v["physical"], v["physical_error"]) = (json!([]), format!("{e:#}").into()),
                }
                let all: Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
                let mut templates = vec![];
                for (repo, targets) in all.as_object().into_iter().flatten() {
                    for (target, choice) in targets.as_object().into_iter().flatten().filter(|(_, c)| c["template"].is_object()) {
                        let mut tm = choice["template"].clone();
                        (tm["repo"], tm["target"]) = (repo.as_str().into(), target.as_str().into());
                        templates.push(tm);
                    }
                }
                v["templates"] = templates.into();
                Ok::<_, anyhow::Error>(v)
            });
            sims.await?
        }
        "sim.template" => {
            let name = target.context("missing target")?;
            let t = detect(&cwd).await?.into_iter().find(|t| t.name == name);
            let t = t.ok_or_else(|| coded("not_found", format!("no target {name} in {}", cwd.display())))?;
            if !eco::get(&t.ecosystem).needs(&t).contains(&Resource::Simulator) {
                bail!(coded("bad_request", format!("{name} does not run on a simulator")));
            }
            // One save or removal at a time: interleaved queue-then-store steps could orphan a template.
            let _one = TEMPLATES.lock().await;
            if p["rm"] != true {
                let env = serde_json::from_value(p["env"].clone()).unwrap_or_default();
                return save_template(d, &t, holder, &env).await;
            }
            // Queued before it is dropped from sims.json, so a kill in between can't orphan the device.
            let (path, tt) = (d.dir.join("sims.json"), t.clone());
            if let Some(u) = tokio::task::spawn_blocking(move || stored(&path, &tt).1["template"]["udid"].as_str().map(String::from)).await? {
                queue_retirement(d, &u, false);
            }
            let (path, tt) = (d.dir.join("sims.json"), t.clone());
            if let Some(old) = tokio::task::spawn_blocking(move || store_template(&path, &tt, None)).await?? {
                queue_retirement(d, &old.udid, true);
            }
            Ok(json!({"target": t.name, "template": null}))
        }
        "sim.set" => {
            let name = target.context("missing target")?;
            let t = detect(&cwd).await?.into_iter().find(|t| t.name == name);
            let t = t.ok_or_else(|| coded("not_found", format!("no target {name} in {}", cwd.display())))?;
            let e = eco::get(&t.ecosystem);
            let pf = match (e.needs(&t).contains(&Resource::Simulator), e.platform()) {
                (true, Some(pf)) => pf,
                _ => bail!(coded("bad_request", format!("{name} does not run on a simulator"))),
            };
            if p["physical"].is_string() && (p["device"].is_string() || p["runtime"].is_string() || p["reset"] == true) {
                bail!(coded("bad_request", "physical picks a device instead of a simulator kind: pass it alone"));
            }
            if let Some(set) = p["slim"].as_str() {
                if p["physical"].is_string() {
                    bail!(coded("bad_request", "simslim is simulator-only: --slim doesn't go with --physical"));
                }
                if set != "off" {
                    let except = except_list(set);
                    tokio::task::spawn_blocking(move || check_slim(pf, &except)).await??;
                }
            }
            let (path, p) = (d.dir.join("sims.json"), p.clone());
            let set = tokio::task::spawn_blocking(move || {
                // Resolved only when a kind is picked, so choosing a physical device needs no simulator runtime.
                let rts = pf.runtimes(None);
                let mut list = None;
                let physical = match p["physical"].as_str() {
                    Some(q) => Some(pf.find_physical(list.insert(pf.physical_devices()?), q, false)?.udid.clone()),
                    None => None,
                };
                let _sims = SIMS.lock().unwrap();
                let (repo, choice) = stored(&path, &t);
                let next = if p["reset"] == true {
                    // Back to the default kind; the template (`sims template --rm` deletes it) and slim stay, unless
                    // this call sets slim too.
                    let mut kept = serde_json::Map::new();
                    for (k, v) in [("template", choice["template"].clone()), ("slim", next_slim(&choice, &p))] {
                        if !v.is_null() {
                            kept.insert(k.into(), v);
                        }
                    }
                    let kept = (!kept.is_empty()).then_some(Value::Object(kept));
                    store(&path, &repo, &t.name, kept.clone())?;
                    kept.unwrap_or(Value::Null)
                } else {
                    let k = match (p["device"].as_str(), p["runtime"].as_str(), &rts) {
                        (None, None, _) => None,
                        (dev, rt, Ok(rts)) => Some(pf.resolve(rts, &choice, dev, rt)?),
                        (_, _, Err(e)) => bail!(coded("no_xcode", format!("{e:#}"))),
                    };
                    let next = next_choice(&choice, &p, k.as_ref(), physical.as_deref());
                    store(&path, &repo, &t.name, Some(next.clone()))?;
                    next
                };
                let mut sim = sim_json(pf, &rts, &next);
                if let Some(u) = next["physical"].as_str() {
                    sim["physical"] = physical_json(&list.take().unwrap_or_else(|| pf.physical_devices().unwrap_or_default()), u);
                }
                Ok::<_, anyhow::Error>(json!({"target": t.name, "sim": sim}))
            });
            set.await?
        }
        "run" => run(d, &cwd, target.context("missing target")?, holder, p).await,
        "exec" => {
            let args: Vec<String> = serde_json::from_value(p["args"].clone()).unwrap_or_default();
            if args.is_empty() {
                bail!(coded("bad_request", "exec needs a command: collie exec -- <cmd> [args…]"));
            }
            let name = p["name"].as_str().map_or_else(|| args.iter().take(2).cloned().collect::<Vec<_>>().join(" "), Into::into);
            let t = Target { name, ecosystem: "task".into(), dir: cwd, kind: "exec".into(), package: None, needs_port: false, port: None, target_dir: None };
            run_target(d, t, holder, p).await
        }
        "stop" => {
            let ids = match target {
                Some(_) => {
                    let st = d.lock();
                    select(&st.procs, &cwd, target, holder).iter().map(|p| p.id.clone()).collect()
                }
                None => vec![],
            };
            let stopped = if target.is_some() {
                stop_where(d, |p| ids.contains(&p.id)).await
            } else {
                stop_where(d, |p| p.holder == holder).await
            };
            Ok(json!({"stopped": stopped}))
        }
        "status" => Ok(d.snapshot()),
        "logs" => read_logs(d, p, &cwd, holder),
        "problems" => {
            let (d, cwd, rescan, new) = (d.clone(), cwd.clone(), p["rescan"] == true, p["new"] == true);
            Ok(tokio::task::spawn_blocking(move || problems(&d, &cwd, rescan, new)).await?)
        }
        "history" => {
            let mut runs = {
                let _g = HISTORY.lock().unwrap();
                read_history(&d.dir)
            };
            if let Some(t) = target {
                runs.retain(|r| r["target"] == t);
            }
            // As `select` narrows: runs of cwd's repo (or package), whose dir cwd is in.
            if p["here"] == true {
                runs.retain(|r| r["dir"].as_str().is_some_and(|dir| !dir.is_empty() && cwd.starts_with(dir)));
            }
            runs.reverse();
            if let Some(n) = p["limit"].as_u64() {
                runs.truncate(n as usize);
            }
            Ok(json!({"runs": runs}))
        }
        "tests" => {
            let results = |id: &str| -> Option<Value> { serde_json::from_slice(&std::fs::read(d.logs.join(format!("{id}.tests.json"))).ok()?).ok() };
            if let Some(id) = p["process"].as_str() {
                // As `logs`' `process`: an id names a file in the logs dir.
                if id.contains('/') || id.starts_with('.') {
                    bail!(coded("bad_request", format!("bad process id {id:?}")));
                }
                return results(id).ok_or_else(|| coded("not_found", format!("no test results for {id}")));
            }
            let name = target.context("missing target or process")?;
            let mut runs = {
                let _g = HISTORY.lock().unwrap();
                read_history(&d.dir)
            };
            // The target's runs in the dir nearest above cwd: not a same-named target's elsewhere, nor a workspace's
            // above a package's.
            runs.retain(|r| r["target"] == name && r["dir"].as_str().is_some_and(|dir| !dir.is_empty() && cwd.starts_with(dir)));
            let nearest = runs.iter().filter_map(|r| r["dir"].as_str()).max_by_key(|dir| dir.len()).map(String::from);
            runs.retain(|r| r["dir"].as_str() == nearest.as_deref());
            let (sep, tests) = merge_results(runs.iter().filter_map(|r| results(r["id"].as_str()?)));
            if tests.is_empty() {
                bail!(coded("not_found", format!("no test results for {name}")));
            }
            Ok(json!({"sep": sep, "tests": tests}))
        }
        "wait" => wait(d, &cwd, target, holder, p["until"].as_str().unwrap_or_default(), p["timeout_ms"].as_u64()).await,
        "lease.acquire" => acquire(d, p, holder).await,
        "lease.release" => {
            let id = id()?;
            let gone = drop_leases(d, |ls| ls.release(id, now_ms())).await;
            if gone.is_empty() {
                bail!(coded("not_found", format!("no dibs {id}")));
            }
            Ok(json!({"released": gone}))
        }
        "lease.renew" => {
            let id = id()?;
            let l = d.lock().leases.renew(id, now_ms()).ok_or_else(|| coded("not_found", format!("no dibs {id}")))?;
            d.changed();
            Ok(json!(l))
        }
        "lease.list" => {
            let s = d.snapshot();
            Ok(json!({"leases": s["leases"], "queue": s["queue"]}))
        }
        "daemon.stop" => {
            // A run either claimed before this (and is stopped here) or sees the flag.
            d.lock().stopping = true;
            Ok(json!({"stopped": stop_where(d, |_| true).await}))
        }
        other => bail!(coded("unknown_method", format!("unknown method {other}"))),
    }
}

async fn conn(d: D, s: UnixStream) {
    let (r, mut w) = s.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let req: Value = serde_json::from_str(&line).unwrap_or_default();
        let (id, m, p) = (&req["id"], req["method"].as_str().unwrap_or_default(), &req["params"]);
        let holder = p["holder"].as_str().unwrap_or_default();
        let cwd = PathBuf::from(p["cwd"].as_str().unwrap_or_default());
        let res = match m {
            "subscribe" => subscribe(&d, &mut w).await,
            "logs" if p["follow"] == true => follow(&d, p, &cwd, holder, &mut w).await,
            _ => call(&d, m, p).await,
        };
        // `built` lets a client tell it talks to an older daemon than itself.
        let frame = match res {
            Ok(r) => json!({"id": id, "result": r, "built": d.built}),
            Err(e) => {
                let code = e.downcast_ref::<Coded>().map_or("error", |c| &c.0);
                json!({"id": id, "error": {"code": code, "message": format!("{e:#}")}, "built": d.built})
            }
        };
        if send(&mut w, &frame).await.is_err() {
            return;
        }
        if m == "daemon.stop" {
            // Unlinked first, so no new client reaches this daemon during the drain and starts a process it would orphan.
            let _ = std::fs::remove_file(socket_path());
            drain_retirements(&d).await;
            std::process::exit(0);
        }
    }
}

// ---- herdr

fn herdr_socket() -> PathBuf {
    std::env::var_os("HERDR_SOCKET_PATH").map_or_else(|| home().join(".config/herdr/herdr.sock"), PathBuf::from)
}

async fn herdr_call(method: &str, params: Value) -> Option<Value> {
    let call = async {
        let mut s = UnixStream::connect(herdr_socket()).await.ok()?;
        let req = json!({"id": "collie", "method": method, "params": params});
        s.write_all(format!("{req}\n").as_bytes()).await.ok()?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).await.ok()?;
        serde_json::from_str::<Value>(&line).ok().map(|v| v["result"].clone())
    };
    tokio::time::timeout(Duration::from_secs(1), call).await.ok().flatten()
}

/// Best effort: the herdr agent name (or kind) running in the holder's pane.
async fn agent_name(holder: &str) -> Option<String> {
    if holder.starts_with("host:") {
        return None;
    }
    let r = herdr_call("agent.list", json!({})).await?;
    let a = r["agents"].as_array()?.iter().find(|a| a["pane_id"] == holder)?;
    a["name"].as_str().or(a["agent"].as_str()).map(Into::into)
}

fn closed_pane(frame: &Value) -> Option<&str> {
    match frame["event"].as_str() {
        Some("pane_closed" | "pane_exited") => frame["data"]["pane_id"].as_str(),
        _ => None,
    }
}

async fn watch_herdr(d: D) {
    loop {
        if let Ok(s) = UnixStream::connect(herdr_socket()).await {
            let (r, mut w) = s.into_split();
            let types = ["pane.closed", "pane.exited"].map(|t| json!({"type": t}));
            let sub = json!({"id": "collie", "method": "events.subscribe", "params": {"subscriptions": types}});
            if send(&mut w, &sub).await.is_ok() {
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let frame: Value = serde_json::from_str(&l).unwrap_or_default();
                    if let Some(pane) = closed_pane(&frame) {
                        drop_leases(&d, |ls| ls.release_holder(pane, now_ms())).await;
                        stop_where(&d, |p| p.holder == pane).await;
                        d.changed();
                    }
                }
            }
        }
        // herdr not running, or it restarted: retry quietly
        sleep(Duration::from_secs(5)).await;
    }
}

async fn retire_sims(d: D, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(udid) = rx.recv().await {
        let u = udid.clone();
        // Kept on failure, so the next daemon start retries it.
        match tokio::task::spawn_blocking(move || eco::retire_device(&eco::PLATFORMS, &u)).await {
            Ok(Ok(())) => d.lock().leases.forget_deleted(&udid),
            _ => d.lock().leases.retire_failed.push(udid),
        }
    }
}

/// Lets queued retirements (and creations still in flight, which a stopped run then retires) finish before a graceful
/// exit, so stopping doesn't leave devices until the next start.
/// Capped at 15 s: a retirement takes 1–5 s, and launchd kills a job 20 s after SIGTERM by default.
async fn drain_retirements(d: &D) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while d.lock().leases.retiring() && Instant::now() < deadline {
        sleep(Duration::from_millis(100)).await;
    }
}

async fn expire_leases(d: D) {
    loop {
        sleep(Duration::from_secs(1)).await;
        drop_leases(&d, |ls| ls.expire(now_ms())).await;
    }
}

// ---- entry

// From libc, which std has no stable wrapper for (`CommandExt::setsid` is unstable).
unsafe extern "C" {
    fn setsid() -> i32;
}

/// Blocks until no daemon holds `daemon.lock`, up to 20 s: a stopped daemon answers first, then drains (simulators,
/// containers) before it exits, and a command starting one meanwhile would lose the lock to it and fail.
pub fn wait_gone() {
    let Ok(lock) = File::open(state_dir().join("daemon.lock")) else { return };
    for _ in 0..200 {
        if lock.try_lock().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Starts the daemon unless one is running; without `foreground`, detaches and returns once it accepts connections.
pub fn start(foreground: bool) -> Result<()> {
    let sock = socket_path();
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        return Ok(());
    }
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(logs_dir())?;
    if !foreground {
        let log = File::options().create(true).append(true).open(dir.join("daemon.log"))?;
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.args(["daemon", "--foreground"]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        // Its own session: closing a herdr pane sends SIGHUP, SIGTERM, then SIGKILL to the pane's whole session, which
        // would end the daemon (and every process it runs) that a command in that pane happened to start.
        // SAFETY: setsid is async-signal-safe, as pre_exec requires.
        unsafe { cmd.pre_exec(|| if setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }) };
        cmd.spawn()?;
        for _ in 0..50 {
            if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("daemon did not start; see {}", dir.join("daemon.log").display());
    }
    // Two clients can race to start a daemon; the lock makes the loser exit instead of unlinking the winner's socket.
    let lock = File::create(dir.join("daemon.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let _ = std::fs::remove_file(&sock);
    tokio::runtime::Runtime::new()?.block_on(serve(dir, sock, lock))
}

async fn serve(dir: PathBuf, sock: PathBuf, _lock: File) -> Result<()> {
    let listener = UnixListener::bind(&sock)?;
    let (retire, rx) = mpsc::unbounded_channel();
    let mut leases = Leases::load(&dir, retire);
    // What a daemon killed with SIGKILL left running, stopped before any new run can start.
    let groups = std::mem::take(&mut leases.groups);
    // All at once: each group gets the same 3 s between TERM and KILL, so the wait doesn't grow with their number.
    let stopping: Vec<_> = tokio::task::spawn_blocking(move || survivors(&groups, started))
        .await?
        .into_iter()
        .map(|g| {
            eprintln!("collie: stopping process group {g}, left running by a killed daemon");
            tokio::spawn(kill_group(g))
        })
        .collect();
    for s in stopping {
        let _ = s.await;
    }
    leases.save();
    let (remove, containers) = mpsc::unbounded_channel();
    leases.remove_containers = Some(remove);
    // Logs lived in the state dir before they moved to the cache dir; those runs have no history to show them by.
    let old_logs = dir.join("logs");
    if old_logs != logs_dir() && old_logs.is_dir() {
        let _ = std::fs::remove_dir_all(&old_logs);
    }
    let d = Arc::new(Daemon {
        st: Mutex::new(State { procs: vec![], leases, stopping: false, writing: HashSet::new(), pruned_at: 0 }),
        tx: broadcast::channel(1024).0,
        dir,
        logs: logs_dir(),
        built: build_time(),
    });
    prune_soon(&d);
    tokio::spawn(retire_sims(d.clone(), rx));
    tokio::spawn(remove_containers(d.clone(), containers));
    // Containers of this daemon's label that no surviving lease holds; free-standing dibs survive a restart, and so do
    // theirs. Without Docker running there is nothing to sweep (until a start with it).
    let label = format!("label=collie={}", d.dir.display());
    let dd = d.clone();
    tokio::task::spawn_blocking(move || {
        let Ok(names) = docker(&["ps", "-a", "--filter", &label, "--format", "{{.Names}}"]) else { return };
        let mut st = dd.lock();
        for c in orphans(&names, &st.leases.leases) {
            eprintln!("collie: removing container {c}, which no dibs hold");
            if let Some(tx) = &st.leases.remove_containers {
                st.leases.removing += usize::from(tx.send(c).is_ok());
            }
        }
    });
    // Devices an earlier daemon was creating when it died: found by their recorded temporary names.
    let pending = d.lock().leases.pending.clone();
    if !pending.is_empty() {
        let d = d.clone();
        tokio::task::spawn_blocking(move || {
            let mut found = vec![];
            for pf in eco::PLATFORMS {
                let Ok(ids) = pf.named(&pending) else { continue };
                found.extend(ids.iter().map(|id| eco::device_key(pf.id(), id)));
            }
            // A template's clone is stored in sims.json before its pending name goes: a kill in between spares it.
            let sims = std::fs::read(d.dir.join("sims.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            let templates = template_udids(&sims);
            found.retain(|u| !templates.contains(u));
            let mut st = d.lock();
            st.leases.devices.extend(found.iter().cloned());
            st.leases.pending.retain(|n| !pending.contains(n));
            st.leases.save();
            for u in found {
                if let Some(tx) = &st.leases.retire {
                    let _ = tx.send(u);
                }
            }
        });
    }
    tokio::spawn(watch_herdr(d.clone()));
    tokio::spawn(expire_leases(d.clone()));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            Ok((s, _)) = listener.accept() => { tokio::spawn(conn(d.clone(), s)); }
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    // Unlinked and closed first: a client connecting during the stop and drain would otherwise wait in the backlog.
    let _ = std::fs::remove_file(&sock);
    drop(listener);
    d.lock().stopping = true;
    stop_where(&d, |_| true).await;
    drain_retirements(&d).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty dir under `target/tmp`, so a test daemon's history and logs never touch the real ones.
    fn scratch() -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("target/tmp/scratch-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_becomes_the_runners_own_selection() {
        let target = |ecosystem: &str, kind: &str| Target { name: "t".into(), ecosystem: ecosystem.into(), dir: "/w".into(), kind: kind.into(), package: None, needs_port: false, port: None, target_dir: None };
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let map = |t: &Target, only: &[&str], args: &[&str]| only_args(eco::get(&t.ecosystem), t, &s(only), s(args));
        let xcode = target("xcode", "test");
        assert_eq!(map(&xcode, &["SumTests/Outer/adds()", "SumTests/Inner/"], &[]).unwrap(), ["-only-testing:SumTests/Outer/adds()", "-only-testing:SumTests/Inner"]);
        let cargo = target("cargo", "test");
        assert_eq!(map(&cargo, &["tests::passes"], &[]).unwrap(), ["--", "tests::passes", "--exact"]);
        assert_eq!(map(&cargo, &["tests::passes"], &["-p", "a", "--", "--nocapture"]).unwrap(), ["-p", "a", "--", "--nocapture", "tests::passes", "--exact"], "after the existing `--`");
        assert_eq!(map(&cargo, &["tests::", "x::y"], &[]).unwrap(), ["--", "tests::", "x::y"], "a group: no --exact");
        assert_eq!(map(&cargo, &[], &["-p", "a"]).unwrap(), ["-p", "a"], "none: the whole target");
        for t in [target("node", "test"), target("cargo", "bin"), target("xcode", "app")] {
            let e = map(&t, &["x"], &[]).unwrap_err();
            assert_eq!(e.downcast_ref::<Coded>().unwrap().0, "bad_request");
        }
    }

    #[tokio::test]
    async fn a_stopped_runs_results_drop_no_tests() {
        let logs = scratch();
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs: vec![], leases: Leases::default(), stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(1).0, dir: scratch(), logs: logs.clone(), built: 0 });
        let mut log = LogFile::open(&logs.join("stopped.log")).unwrap();
        log.append(&Record { ts: 0, source: Source::Stdout, level: Level::Info, line: "test tests::a ... ok".into() });
        let t = Target { name: "cargo test".into(), ecosystem: "cargo".into(), dir: "/w".into(), kind: "test".into(), package: None, needs_port: false, port: None, target_dir: None };
        save_results(&d, "stopped", eco::get("cargo"), t, HashMap::new(), logs.join("stopped.log"), true).await;
        let saved: Value = serde_json::from_slice(&std::fs::read(logs.join("stopped.tests.json")).unwrap()).unwrap();
        assert_eq!(saved["only"], true);
        let full = json!({"sep": "::", "only": false, "tests": [{"id": "tests::a", "status": "failed"}, {"id": "tests::b", "status": "passed"}]});
        let ids: Vec<_> = merge_results([full, saved].into_iter()).1.iter().map(|t| t["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["tests::a", "tests::b"], "the stop cut tests::b short; it stays");
    }

    #[test]
    fn a_full_run_drops_gone_tests_and_a_filtered_one_adds_new_ones() {
        let run = |only: bool, tests: &[(&str, &str)]| json!({"sep": "::", "only": only, "tests": tests.iter().map(|(id, s)| json!({"id": id, "status": s})).collect::<Vec<_>>()});
        let ids = |runs: Vec<Value>| merge_results(runs.into_iter()).1.iter().map(|t| format!("{}={}", t["id"].as_str().unwrap(), t["status"].as_str().unwrap())).collect::<Vec<_>>();
        // `old` was renamed `new`: the next full run drops it.
        assert_eq!(ids(vec![run(false, &[("a", "failed"), ("old", "failed")]), run(false, &[("a", "passed"), ("new", "passed")])]), ["a=passed", "new=passed"]);
        // A filtered rerun updates `a` and keeps `b`; a test first seen in it stays.
        let filtered = vec![run(false, &[("a", "failed"), ("b", "passed")]), run(true, &[("a", "passed"), ("added", "failed")])];
        assert_eq!(ids(filtered), ["a=passed", "b=passed", "added=failed"]);
    }

    #[test]
    fn prune_drops_old_logs_but_spares_live_and_newest_runs() {
        let (dir, logs) = (scratch(), scratch());
        let run = |id: &str, target: &str, since: u64| json!({"id": id, "target": target, "dir": "/w", "since": since}).to_string();
        let history = [run("a-1", "a", 1), run("a-2", "a", 2), run("b-1", "b", 1)].join("\n");
        std::fs::write(dir.join("history.jsonl"), history + "\n").unwrap();
        let week_ago = SystemTime::now() - LOG_AGE - Duration::from_secs(60);
        for id in ["a-1", "a-2", "b-1", "live"] {
            let f = File::create(logs.join(format!("{id}.log"))).unwrap();
            f.set_modified(week_ago).unwrap();
        }
        // Test results and problems go with their log; a result bundle stays only while its run is live.
        for f in ["a-1.tests.json", "a-2.tests.json", "a-1.problems.json", "b-1.problems.json"] {
            std::fs::write(logs.join(f), "{}").unwrap();
        }
        for d in ["dead.xcresult", "live.xcresult"] {
            std::fs::create_dir_all(logs.join(d).join("Data")).unwrap();
        }
        prune(&dir, &logs, &HashSet::from(["live".to_string()]));
        let mut left: Vec<_> = std::fs::read_dir(&logs).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        // a-1 is old and not a's newest run; a-2, b-1 (each target's newest) and the live run stay.
        assert_eq!(left, ["a-2.log", "a-2.tests.json", "b-1.log", "b-1.problems.json", "live.log", "live.xcresult"]);
        let ids: Vec<_> = read_history(&dir).iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["a-2", "b-1"]);
    }

    #[test]
    fn problems_stand_until_their_file_is_rebuilt_and_merge_across_targets() {
        let run = |id: &str, target: &str, since: u64, built: bool| json!({"id": id, "target": target, "dir": "/w", "since": since, "ended_at": since + 1, "built": built});
        let item = |path: &str, kind: &str, line: u64| json!({"kind": kind, "path": path, "resolved": true, "line": line, "col": 1, "message": "m", "cursor": "x@0"});
        let file = |items: Vec<Value>| json!({"items": items});
        let at = |m: u64| move |_: &Value, _: &str| Some(m);
        let warned = (run("a-1", "app", 10, true), file(vec![item("A.swift", "warning", 3)]));
        // A later no-op build (Xcode re-emits nothing for files it didn't compile) keeps them.
        let noop = (run("a-2", "app", 20, true), file(vec![]));
        assert_eq!(merge_problems(&[warned.clone(), noop.clone()], at(5)).len(), 1);
        // The file changed (15) and a build started after that (20) without reporting it: fixed.
        assert!(merge_problems(&[warned.clone(), noop.clone()], at(15)).is_empty());
        assert_eq!(merge_problems(std::slice::from_ref(&warned), at(15)).len(), 1, "changed, not rebuilt yet");
        assert!(merge_problems(std::slice::from_ref(&warned), |_, _| None).is_empty(), "the file is gone");
        // A failed build's errors show.
        let failed = (run("a-3", "app", 30, false), file(vec![item("B.swift", "error", 1)]));
        let kinds: Vec<_> = merge_problems(&[warned.clone(), failed], at(5)).iter().map(|i| i["kind"].clone()).collect();
        assert_eq!(kinds, ["warning", "error"]);
        // The same problem from three targets is one item, its sources newest first.
        let tests = (run("t-1", "tests", 30, true), file(vec![item("A.swift", "warning", 3)]));
        let ui = (run("u-1", "ui", 20, true), file(vec![item("A.swift", "warning", 3)]));
        let merged = merge_problems(&[warned.clone(), tests, ui], at(5));
        assert_eq!(merged.len(), 1);
        let sources: Vec<_> = merged[0]["sources"].as_array().unwrap().iter().map(|s| s["target"].clone()).collect();
        assert_eq!(sources, [json!("tests"), json!("ui"), json!("app")]);
        // The same message on another line is another problem.
        let twice = (run("a-4", "app", 40, true), file(vec![item("A.swift", "warning", 2), item("A.swift", "warning", 3)]));
        assert_eq!(merge_problems(&[warned, twice], at(5)).len(), 2);
    }

    #[test]
    fn a_swift_diagnostic_is_one_record_with_its_excerpt_and_notes() {
        let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/swiftc-diagnostic.txt")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let recs = group(&lines, |h, l| eco::Xcode.continues(h, l));
        assert_eq!(recs.len(), 2, "the warning with its note, then the next warning: {recs:#?}");
        assert!(recs.iter().all(|r| !r.trim().is_empty()), "no blank records");
        let block: Vec<&str> = recs[0].lines().collect();
        assert_eq!(block.len(), 15, "header, 6 excerpt lines, the blank between, the note and its 6");
        assert!(block[0].ends_with("can not be referenced from a nonisolated autoclosure") && block[8].ends_with("note: property declared here"));
        // swiftc prints `589 | ` with its space; editors may strip it from the fixture.
        assert_eq!(block.last().unwrap().trim_end(), "589 |", "the blank line after the block is trimmed");
        let levels: Vec<Level> = recs.iter().map(|r| eco::Xcode.classify(Source::Build, r.lines().next().unwrap()).0).collect();
        assert_eq!(levels, [Level::Warn, Level::Warn], "by the first line: the `- warning marker doesn't count");
        // Outside a block a blank line is a record, as before; a continuing line with no record open starts one.
        assert_eq!(group(&["a", "", "b"], |h, l| eco::Xcode.continues(h, l)), ["a", "", "b"]);
        assert_eq!(group(&["  12 | x", "b"], |_, _| true), ["  12 | x\nb"]);
        // The cap: a block's 201st line starts a record of its own.
        let long: Vec<&str> = std::iter::once("h").chain(std::iter::repeat_n("1 | x", 200)).collect();
        assert_eq!(group(&long, |h, l| eco::Xcode.continues(h, l)).len(), 2);
        // One problem, as the header alone gave; the note is none.
        let rec = |line: &str| ("p@0".to_string(), Record { ts: 0, source: Source::Build, level: Level::Warn, line: line.into() });
        let root = std::fs::canonicalize(scratch()).unwrap();
        // Relative, as if in this worktree: an absolute path outside it is dropped.
        let block = recs[0].replace("/Users/dev/Projects/demo/demo-ios/", "");
        let found = build_problems(&[rec(&block), rec(&block)], &root, &root, true);
        assert_eq!(found.len(), 1, "{found:#?}");
        let i = &found[0];
        assert_eq!((&i["kind"], &i["path"], &i["line"], &i["col"]), (&json!("warning"), &json!("Packages/StepFlow/Module/Sources/Views/StepContainerView.swift"), &json!(85), &json!(95)));
        assert_eq!(i["message"], "main actor-isolated property 'isOnInnerDisplay' can not be referenced from a nonisolated autoclosure");
        assert_eq!(eco::diagnostic(&recs[0]), eco::diagnostic(lines[0]), "the same problem as its header line");
    }

    #[tokio::test]
    async fn a_runs_problems_are_its_builds_once_each_and_a_stopped_run_writes_none() {
        let root = std::fs::canonicalize(scratch()).unwrap();
        let rec = |source, line: &str| ("p@0".to_string(), Record { ts: 0, source, level: Level::Warn, line: line.into() });
        let recs = [
            rec(Source::Build, "Sources/A.swift:3:5: warning: unused"),
            rec(Source::Build, "Sources/A.swift:3:5: warning: unused"),
            rec(Source::Build, "Sources/A.swift:9:5: warning: unused"),
            rec(Source::Build, "/elsewhere/DerivedData/B.swift:1:1: warning: theirs"),
            rec(Source::Stdout, "Sources/C.swift:1:1: warning: the app's own output"),
        ];
        let items = build_problems(&recs, &root, &root, true);
        let found = items.iter().map(|i| (i["path"].clone(), i["resolved"].clone(), i["line"].clone())).collect::<Vec<_>>();
        assert_eq!(found, [(json!("Sources/A.swift"), json!(false), json!(3)), (json!("Sources/A.swift"), json!(false), json!(9))], "a repeat is one; the same message on another line isn't");
        assert_eq!(build_problems(&recs, &root, &root, false).len(), 3, "a task without a build: all its output");
        // Stopped: the process is gone by the time its problems would be saved.
        let logs = scratch();
        let d: D = Arc::new(Daemon { st: Mutex::new(state(Leases::default())), tx: broadcast::channel(1).0, dir: scratch(), logs: logs.clone(), built: 0 });
        let t = Target { name: "app".into(), ecosystem: "xcode".into(), dir: root.clone(), kind: "app".into(), package: None, needs_port: false, port: None, target_dir: None };
        save_problems(&d, "gone", &t, true).await;
        assert!(!logs.join("gone.problems.json").exists());
    }

    #[test]
    fn a_branch_is_compared_with_the_default_and_its_added_lines() {
        assert_eq!(pick_base(Some("origin/develop".into()), |_| true).as_deref(), Some("origin/develop"));
        assert_eq!(pick_base(None, |b| b == "master" || b == "main").as_deref(), Some("main"));
        assert_eq!(pick_base(None, |_| false), None);
        assert!(is_base(Some("develop"), Some("develop"), || false));
        assert!(!is_base(Some("feat/x"), Some("develop"), || true), "a fresh branch at the merge base: its edits are new");
        assert!(is_base(None, Some("develop"), || true) && !is_base(None, Some("develop"), || false), "detached: at the base's HEAD only");
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,0 +2,2 @@\n+x\n+y\n@@ -9 +11 @@\n-old\n+new\n@@ -20,3 +21,0 @@\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n";
        let added = added_lines(diff);
        assert_eq!(added["a.rs"], [(2, 2), (11, 1), (21, 0)]);
        assert!(!added.contains_key("gone.rs"));
        let hit = todo_item("a.rs:3:    // TODO(jan): fix: this */").unwrap();
        assert_eq!((&hit["tag"], &hit["col"], &hit["message"]), (&json!("TODO"), &json!(8), &json!("(jan): fix: this")));
        assert!(todo_item("a.rs:1:TODOS and todo").is_none(), "whole words, case-sensitive");
    }

    #[test]
    fn released_port_goes_to_the_waiter() {
        let mut ls = Leases::default();
        let a = ls.acquire("w1:p1", None, None, false, None, 0).unwrap();
        let b = ls.acquire("w1:p2", None, None, false, None, 0).unwrap();
        assert_ne!(a.port, b.port);
        assert!(ls.acquire("w1:p3", None, a.port, true, None, 0).is_none());
        let (_, mut rx) = ls.enqueue("w1:p3", None, a.port, None, None, 0);
        assert_eq!(ls.queue.len(), 1);
        ls.release(a.id, 0);
        let got = rx.try_recv().unwrap();
        assert_eq!((got.holder.as_str(), got.port), ("w1:p3", a.port));
        assert!(ls.queue.is_empty());
    }

    #[test]
    fn ttl_expiry_releases() {
        let mut ls = Leases::default();
        ls.acquire("w1:p1", None, None, false, None, 0).unwrap();
        assert!(ls.expire(TTL_MS - 1).is_empty());
        assert_eq!(ls.expire(TTL_MS).len(), 1);
        assert!(ls.leases.is_empty());
    }

    #[test]
    fn pane_closed_releases_that_holders_leases() {
        let mut ls = Leases::default();
        ls.acquire("w1:p1", None, None, false, None, 0).unwrap();
        ls.acquire("w1:p2", None, None, false, None, 0).unwrap();
        let frame = json!({"data": {"pane_id": "w1:p2", "type": "pane_closed", "workspace_id": "w1"}, "event": "pane_closed"});
        ls.release_holder(closed_pane(&frame).unwrap(), 0);
        assert_eq!(ls.leases.iter().map(|l| l.holder.as_str()).collect::<Vec<_>>(), ["w1:p1"]);
    }

    #[test]
    fn phase1_port_lease_json_round_trips_unchanged() {
        let phase1 = r#"[{"id":3,"holder":"w1:p2","agent":"claude","resource":"port","port":3000,"acquired_at":1,"expires_at":2}]"#;
        let ls: Vec<Lease> = serde_json::from_str(phase1).unwrap();
        assert_eq!(serde_json::to_string(&ls).unwrap(), phase1);
    }

    #[test]
    fn restart_drops_run_leases_and_sweeps_only_its_own_devices() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/leases-load");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sim = |udid: &str| format!(r#""simulator":{{"udid":"{udid}","name":"collie","device":"iPhone 18 Pro","runtime":"iOS 27.0"}}"#);
        let leases = format!(
            r#"[{{"id":1,"holder":"w1:p1","agent":null,"resource":"simulator",{},"acquired_at":0,"expires_at":9,"run":true}},
                {{"id":2,"holder":"w1:p2","agent":null,"resource":"simulator",{},"acquired_at":0,"expires_at":9}},
                {{"id":3,"holder":"w1:p3","agent":null,"resource":"port","port":3000,"acquired_at":0,"expires_at":9,"run":true}},
                {{"id":4,"holder":"w1:p4","agent":null,"resource":"port","port":3001,"acquired_at":0,"expires_at":9}},
                {{"id":5,"holder":"w1:p5","agent":null,"resource":"simulator",{},"acquired_at":0,"expires_at":9,"run":true}}]"#,
            sim("RUN"),
            sim("KEPT"),
            sim("FOREIGN")
        );
        std::fs::write(dir.join("leases.json"), leases).unwrap();
        // LOST: created here, its lease gone. FOREIGN: its lease dies too, but this daemon never created it.
        std::fs::write(dir.join("devices.json"), r#"["RUN","KEPT","LOST"]"#).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut ls = Leases::load(&dir, tx);
        assert_eq!(ls.leases.iter().map(|l| l.id).collect::<Vec<_>>(), [2, 4]);
        let mut retired = vec![];
        while let Ok(u) = rx.try_recv() {
            retired.push(u);
        }
        assert_eq!(retired, ["RUN", "LOST"]);
        assert!(ls.retiring(), "a graceful stop waits for RUN and LOST");
        ls.forget_deleted("RUN");
        ls.retire_failed.push("LOST".into());
        assert!(!ls.retiring(), "LOST's delete failed and KEPT is leased, so nothing is left to wait for");
        assert!(ls.devices.contains(&"LOST".to_string()), "the next start retries LOST");
        ls.creating = 1;
        assert!(ls.retiring(), "a device being created is waited for too");
        let saved: Vec<Lease> = serde_json::from_slice(&std::fs::read(dir.join("leases.json")).unwrap()).unwrap();
        assert_eq!(saved.len(), 2);
    }

    #[test]
    fn head_alone_keeps_following_the_default_kind() {
        let iphone = |id: &str| json!({"identifier": id, "name": id, "productFamily": "iPhone"});
        let rt = |v: &str, devices: Vec<Value>| json!({"identifier": v, "name": v, "version": v, "supportedDeviceTypes": devices});
        let rts = vec![rt("27.0", vec![iphone("18 Pro"), iphone("17 Pro")])];
        let k = eco::resolve(&rts, &Value::Null, None, None).unwrap();
        let next = next_choice(&Value::Null, &json!({"head": true}), Some(&k), None);
        assert_eq!(next, json!({"head": true}));
        let v = sim_json(&eco::Ios, &Ok(rts.clone()), &next);
        assert_eq!((&v["default"], &v["head"], &v["device_name"]), (&json!(true), &json!(true), &json!("18 Pro")));
        // A newer runtime installed later becomes the kind, since nothing was pinned.
        let newer = vec![rt("28.0", vec![iphone("19 Pro")]), rts[0].clone()];
        assert_eq!(sim_json(&eco::Ios, &Ok(newer), &next)["device_name"], "19 Pro");
        let picked = eco::resolve(&rts, &next, Some("17 Pro"), None).unwrap();
        let next = next_choice(&next, &json!({"device": "17 Pro"}), Some(&picked), None);
        assert_eq!(next, json!({"device": "17 Pro", "runtime": "27.0", "head": true}));
        assert_eq!(sim_json(&eco::Ios, &Ok(rts), &next)["default"], false);
    }

    #[test]
    fn a_runs_slim_flag_beats_the_stored_setting_and_carry_keeps_to_it() {
        let some = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let code = |r: Result<Option<Vec<String>>>| r.unwrap_err().downcast_ref::<Coded>().unwrap().0.clone();
        assert_eq!(slim_for(&Value::Null, &Value::Null, false).unwrap(), None, "neither: stock");
        assert_eq!(slim_for(&Value::Null, &json!(["web", "icloud"]), false).unwrap(), some(&["icloud", "web"]), "the stored one");
        assert_eq!(slim_for(&json!(false), &json!([]), false).unwrap(), None, "--stock over a stored slim");
        assert_eq!(slim_for(&json!(["store"]), &Value::Null, false).unwrap(), some(&["store"]), "--slim=store over stock");
        assert_eq!(slim_for(&Value::Null, &json!([]), true).unwrap(), None, "a physical device: the stored one doesn't apply");
        assert_eq!(code(slim_for(&json!([]), &Value::Null, true)), "bad_request", "nor does a flag");
        // Stored, beside head: set, kept, changed, removed.
        let next = next_choice(&json!({"head": true}), &json!({"slim": "web,icloud"}), None, None);
        assert_eq!(next, json!({"head": true, "slim": ["icloud", "web"]}));
        assert_eq!(next_choice(&next, &json!({"head": false}), None, None)["slim"], json!(["icloud", "web"]));
        assert_eq!(next_choice(&next, &json!({"slim": "on"}), None, None)["slim"], json!([]));
        assert!(next_choice(&next, &json!({"slim": "off"}), None, None)["slim"].is_null());
        // `--reset` keeps the stored setting unless it sets one too.
        assert_eq!(next_slim(&next, &json!({"reset": true})), json!(["icloud", "web"]));
        assert!(next_slim(&next, &json!({"reset": true, "slim": "off"})).is_null());
        assert_eq!(next_slim(&next, &json!({"reset": true, "slim": "store"})), json!(["store"]));
        // Anything but null, false or an array of category ids is refused, not read as stock.
        for bad in [json!(true), json!("on"), json!([1]), json!({})] {
            assert_eq!(code(slim_for(&bad, &Value::Null, false)), "bad_request", "{bad}");
        }
        let iphone = json!({"identifier": "17 Pro", "name": "17 Pro", "productFamily": "iPhone"});
        let rts = vec![json!({"identifier": "27.0", "name": "iOS 27.0", "version": "27.0", "supportedDeviceTypes": [iphone]})];
        assert_eq!(sim_json(&eco::Ios, &Ok(rts), &next)["slim"], json!(["icloud", "web"]));
        // A re-run keeps a device only with the same setting, and never a no-reboot slim one (before iOS 18.5).
        let mut ls = Leases::default();
        let mut lease = |runtime: &str, slim: Option<Vec<String>>| {
            let id = ls.reserve();
            let sim = Sim { udid: format!("U{id}"), name: format!("collie {id}"), device: "iPhone 17 Pro".into(), runtime: runtime.into(), slim, platform: "ios".into() };
            ls.grant(id, "w1:p1", None, None, Some(sim), None, true, 0)
        };
        let (slim27, stock27, slim18) = (lease("iOS 27.0", some(&[])), lease("iOS 27.0", None), lease("iOS 18.0", some(&[])));
        let k27 = kind("iPhone 17 Pro");
        let k18 = SimKind { runtime_name: "iOS 18.0".into(), ..k27.clone() };
        assert!(sim_fits(&k27, &some(&[]))(&slim27) && !sim_fits(&k27, &some(&[]))(&stock27));
        assert!(!sim_fits(&k27, &None)(&slim27) && sim_fits(&k27, &None)(&stock27));
        assert!(!sim_fits(&k27, &some(&["web"]))(&slim27), "another except list");
        assert!(!sim_fits(&k18, &some(&[]))(&slim18), "may have rebooted stock");
    }

    #[test]
    fn restart_carries_the_simulator_and_release_retires_it() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut ls = Leases { retire: Some(tx), ..Default::default() };
        let port = ls.acquire("w1:p1", None, None, false, None, 0).unwrap();
        let id = ls.reserve();
        let sim = Sim { udid: "U1".into(), name: format!("collie {id}"), device: "iPhone 18 Pro".into(), runtime: "iOS 27.0".into(), slim: None, platform: "ios".into() };
        let s = ls.grant(id, "w1:p1", None, None, Some(sim), None, true, 0);
        assert!(s.run, "marked at grant, with no window before it");
        let v = json!(s);
        assert_eq!((&v["resource"], &v["simulator"]["name"], v.get("port")), (&json!("simulator"), &json!(format!("collie {id}")), None));

        let p = my_app("APP-1", vec![port.id, s.id], &s, true);
        let mut st = State { procs: vec![p], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 };
        let mine = |h: &'static str| move |p: &Proc| p.target == "MyApp" && p.holder == h;
        assert!(carry(&mut st, mine("w1:p2"), sim_fits(&kind("iPhone 18 Pro"), &None)).is_none(), "another holder gets a fresh device");
        assert!(carry(&mut st, mine("w1:p1"), sim_fits(&kind("iPhone 17 Pro"), &None)).is_none(), "a changed kind gets a fresh device");
        let carried = carry(&mut st, mine("w1:p1"), sim_fits(&kind("iPhone 18 Pro"), &None)).unwrap();
        assert_eq!(carried.id, s.id);
        assert!(carried.expires_at > TTL_MS, "the restart gets a fresh TTL");
        assert_eq!(st.leases.leases.iter().find(|l| l.id == s.id).unwrap().expires_at, carried.expires_at);
        // what stop_where releases for the restart
        let ids = st.procs[0].leases.clone();
        assert_eq!(st.leases.remove(|l| ids.contains(&l.id), 0).iter().map(|l| l.id).collect::<Vec<_>>(), [port.id]);
        assert!(rx.try_recv().is_err());
        st.leases.release(s.id, 0);
        assert_eq!(rx.try_recv().unwrap(), "U1");
    }

    #[test]
    fn unlaunched_failure_keeps_the_simulator_but_an_app_exit_retires_it() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut ls = Leases { retire: Some(tx), ..Default::default() };
        let mut sim_lease = |udid: &str| {
            let id = ls.reserve();
            let sim = Sim { udid: udid.into(), name: format!("collie {id}"), device: "iPhone 18 Pro".into(), runtime: "iOS 27.0".into(), slim: None, platform: "ios".into() };
            ls.grant(id, "w1:p1", None, None, Some(sim), None, true, 0)
        };
        let (failed, exited) = (sim_lease("FAILED"), sim_lease("EXITED"));
        let mut building = my_app("APP-1", vec![failed.id], &failed, false);
        building.state = St::Building;
        let running = my_app("APP-2", vec![exited.id], &exited, true);
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs: vec![building, running], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(1).0, dir: scratch(), logs: scratch(), built: 0 });
        use std::os::unix::process::ExitStatusExt;
        finish(&d, "APP-1", Some(ExitStatus::from_raw(1 << 8)));
        finish(&d, "APP-2", Some(ExitStatus::from_raw(0)));
        assert_eq!(rx.try_recv().unwrap(), "EXITED");
        assert!(rx.try_recv().is_err(), "the build failure's device stays");
        let mut st = d.lock();
        assert_eq!(st.procs[0].state, St::Crashed);
        let carried = carry(&mut st, |p| p.id == "APP-1" && p.holder == "w1:p1", sim_fits(&kind("iPhone 18 Pro"), &None));
        assert_eq!(carried.map(|l| l.id), Some(failed.id));
    }

    #[test]
    fn a_test_run_carries_the_simulator_of_the_holders_ended_test_run() {
        let mut ls = Leases { retire: Some(mpsc::unbounded_channel().0), ..Default::default() };
        let mut lease = |udid: &str| {
            let id = ls.reserve();
            let sim = Sim { udid: udid.into(), name: format!("collie {id}"), device: "iPhone 18 Pro".into(), runtime: "iOS 27.0".into(), slim: None, platform: "ios".into() };
            ls.grant(id, "w1:p1", None, None, Some(sim), None, true, 0)
        };
        let (running, ended) = (lease("RUNNING"), lease("ENDED"));
        let test = |id: &str, name: &str, l: &Lease, state: St| {
            let mut p = my_app(id, vec![l.id], l, false);
            (p.target, p.t.name, p.t.kind, p.state) = (name.into(), name.into(), "test".into(), state);
            p
        };
        let procs = vec![test("F-1", "ListCoreTests", &running, St::Running), test("D-1", "SyncCoreTests", &ended, St::Exited)];
        let t = procs[0].t.clone();
        let mut st = State { procs, leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 };
        let k = kind("iPhone 18 Pro");
        let fits = || sim_fits(&k, &None);
        let search = Target { name: "SearchCoreTests".into(), ..t.clone() };
        assert!(carry(&mut st, sim_source(&search, "w1:p2"), fits()).is_none(), "another holder's");
        let app = Target { name: "MyApp".into(), kind: "app".into(), ..t.clone() };
        assert!(carry(&mut st, sim_source(&app, "w1:p1"), fits()).is_none(), "an app run doesn't take a test's device");
        let elsewhere = Target { dir: "/other".into(), ..search.clone() };
        assert!(carry(&mut st, sim_source(&elsewhere, "w1:p1"), fits()).is_none(), "another worktree's");
        let carried = carry(&mut st, sim_source(&search, "w1:p1"), fits()).unwrap();
        assert_eq!(carried.id, ended.id, "the ended SyncCoreTests run's, never the running ListCoreTests run's");
        assert!(st.procs[1].leases.is_empty() && st.procs[0].leases == [running.id]);
    }

    #[test]
    fn a_timed_out_run_retires_its_simulator_while_a_failed_test_run_keeps_it() {
        assert_eq!(json!(St::TimedOut), "timed-out");
        let ms = |ms| Some(Duration::from_millis(ms));
        assert_eq!(run_limit("test", None), ms(30 * 60 * 1000), "tests default to 30 min");
        assert_eq!(run_limit("test", Some(0)), None, "0 turns it off");
        assert_eq!(run_limit("bin", None), None, "other kinds have none");
        assert_eq!(run_limit("exec", Some(500)), ms(500));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut ls = Leases { retire: Some(tx), ..Default::default() };
        let mut sim_lease = |udid: &str| {
            let id = ls.reserve();
            let sim = Sim { udid: udid.into(), name: format!("collie {id}"), device: "iPhone 18 Pro".into(), runtime: "iOS 27.0".into(), slim: None, platform: "ios".into() };
            ls.grant(id, "w1:p1", None, None, Some(sim), None, true, 0)
        };
        let (failed, hung) = (sim_lease("FAILED"), sim_lease("HUNG"));
        let test = |id: &str, l: &Lease, timed_out: bool| {
            let mut p = my_app(id, vec![l.id], l, false);
            (p.t.kind, p.timed_out) = ("test".into(), timed_out);
            assert!(!p.ended(), "being killed is not ended yet");
            p
        };
        let procs = vec![test("T-1", &failed, false), test("T-2", &hung, true)];
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs, leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(1).0, dir: scratch(), logs: scratch(), built: 0 });
        use std::os::unix::process::ExitStatusExt;
        finish(&d, "T-1", Some(ExitStatus::from_raw(65 << 8)));
        finish(&d, "T-2", Some(ExitStatus::from_raw(15)));
        assert_eq!(rx.try_recv().unwrap(), "HUNG");
        assert!(rx.try_recv().is_err(), "the failed run's device stays for the next run");
        let st = d.lock();
        let v = json!(st.procs[1]);
        assert_eq!((&v["state"], &v["signal"], v["ended_at"].is_u64()), (&json!("timed-out"), &json!(15), true), "finish keeps the state");
        assert!(st.procs[1].ended() && st.procs[1].leases.is_empty());
        assert_eq!((st.procs[0].state, &st.procs[0].leases), (St::Crashed, &vec![failed.id]));
    }

    #[test]
    fn a_service_lease_round_trips_and_its_release_removes_the_container() {
        let pg = r#"{"id":4,"holder":"w1:p1","agent":null,"resource":"postgres","port":3001,"service":{"image":"postgres:17","container":"collie-4-1793225844000","url":"postgres://collie:collie@127.0.0.1:3001/collie"},"acquired_at":1,"expires_at":2}"#;
        let l: Lease = serde_json::from_str(pg).unwrap();
        assert_eq!(serde_json::to_string(&l).unwrap(), pg);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut ls = Leases { remove_containers: Some(tx), leases: vec![l], ..Default::default() };
        ls.acquire("w1:p2", None, None, false, None, 0).unwrap();
        assert!(!ls.retiring());
        ls.release(4, 0);
        assert_eq!(rx.try_recv().unwrap(), "collie-4-1793225844000");
        assert!(ls.retiring(), "a graceful stop waits for the removal");
        assert!(rx.try_recv().is_err(), "a port lease has no container");
        // `docker ps -a --filter label=collie=<state dir> --format '{{.Names}}'`, as Docker 29 prints it. A killed
        // daemon's orphan with the same lease id as a live dib differs in its start time, so only it goes.
        let held: Lease = serde_json::from_str(&pg.replace("collie-4-1793225844000", "collie-7-1793229000000")).unwrap();
        assert_eq!(orphans("collie-7-1793225000000\ncollie-7-1793229000000\n", &[held]), ["collie-7-1793225000000"]);
        assert!(orphans("", &[]).is_empty());
    }

    #[test]
    fn templates_are_cloned_only_for_their_kind_and_stored_beside_the_choice() {
        let tm = Sim { udid: "TMPL".into(), name: "collie template MyApp DEV".into(), device: "iPhone 17 Pro Max".into(), runtime: "iOS 26.5".into(), slim: None, platform: "ios".into() };
        let kind = |device: &str, runtime: &str| SimKind { device: "".into(), device_name: device.into(), runtime: "".into(), runtime_name: runtime.into() };
        assert_eq!(template_for(Some(&tm), &kind("iPhone 17 Pro Max", "iOS 26.5")).map(|s| s.udid.as_str()), Some("TMPL"));
        assert!(template_for(Some(&tm), &kind("iPhone 17 Pro Max", "iOS 27.0")).is_none(), "another runtime: created fresh");
        assert!(template_for(Some(&tm), &kind("iPhone 18 Pro", "iOS 26.5")).is_none(), "another device: created fresh");
        assert!(template_for(None, &kind("iPhone 17 Pro Max", "iOS 26.5")).is_none());

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/templates");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sims.json");
        let t = Target {
            name: "MyApp DEV".into(),
            ecosystem: "xcode".into(),
            dir: dir.clone(),
            kind: "app".into(),
            package: None,
            needs_port: false,
            port: None,
            target_dir: None,
        };
        let (repo, _) = stored(&path, &t);
        store(&path, &repo, &t.name, Some(json!({"device": "d", "runtime": "r", "head": true}))).unwrap();
        assert_eq!(store_template(&path, &t, Some(&tm)).unwrap(), None);
        let choice = stored(&path, &t).1;
        assert_eq!((&choice["head"], &choice["template"]["udid"]), (&json!(true), &json!("TMPL")), "the choice stays");
        assert_eq!(next_choice(&choice, &json!({"head": false}), None, None)["template"]["udid"], "TMPL", "sim.set keeps it");
        let newer = Sim { udid: "TMPL2".into(), ..tm.clone() };
        assert_eq!(store_template(&path, &t, Some(&newer)).unwrap(), Some(tm.clone()), "saving again hands back the old one to delete");
        assert_eq!(store_template(&path, &t, None).unwrap(), Some(newer));
        assert_eq!(stored(&path, &t).1, json!({"device": "d", "runtime": "r", "head": true}));
        store(&path, &repo, &t.name, None).unwrap();
        store_template(&path, &t, Some(&tm)).unwrap();
        assert_eq!(store_template(&path, &t, None).unwrap().map(|s| s.udid), Some("TMPL".into()));
        assert_eq!(stored(&path, &t).1, Value::Null, "an entry that only held a template goes with it");

        let file = || serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(file(), json!({}), "nor an empty repo entry");

        // A restart's sweep spares what sims.json stores as a template, even when queued in devices.json for deletion
        // (a kill between queueing a replaced template and storing the new one).
        store_template(&path, &t, Some(&tm)).unwrap();
        assert_eq!(template_udids(&file()), ["TMPL"]);
        std::fs::write(dir.join("devices.json"), r#"["RUN","TMPL"]"#).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ls = Leases::load(&dir, tx);
        assert_eq!(rx.try_recv().unwrap(), "RUN");
        assert!(rx.try_recv().is_err() && !ls.devices.contains(&"TMPL".to_string()));
    }

    #[tokio::test]
    async fn a_failed_template_save_keeps_the_simulator_as_dibs() {
        let mut ls = Leases { retire: Some(mpsc::unbounded_channel().0), ..Default::default() };
        let id = ls.reserve();
        // No such device: `simctl clone` fails (as it does for a booted one) without touching any simulator.
        let sim = Sim { udid: "00000000-0000-0000-0000-0000C0111E00".into(), name: "collie 1".into(), device: "iPhone 17 Pro Max".into(), runtime: "iOS 26.5".into(), slim: None, platform: "ios".into() };
        let l = ls.grant(id, "w1:p1", None, None, Some(sim), None, true, 0);
        let p = my_app("APP-1", vec![l.id], &l, true);
        let t = p.t.clone();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/template-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs: vec![p], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(16).0, dir: dir.clone(), logs: dir.join("logs"), built: 0 });
        let e = save_template(&d, &t, "w1:p1", &HashMap::new()).await.unwrap_err().to_string();
        assert!(e.contains(&format!("kept as dibs {id}")), "{e}");
        let st = d.lock();
        let kept = st.leases.leases.iter().find(|x| x.id == id).expect("the lease stays");
        // Set at the carry (the failure path no longer touches it), so a kill mid-save keeps it as dibs too.
        assert_eq!((kept.holder.as_str(), kept.run), ("w1:p1", false));
        assert!(st.leases.pending.is_empty() && st.procs.is_empty(), "the process was stopped, the pending name dropped");
        assert!(!dir.join("sims.json").exists(), "nothing stored");
    }

    fn phone() -> Sim {
        Sim { udid: "00000000-0000000000000001".into(), name: "Test iPhone".into(), device: "iPhone 13 Pro".into(), runtime: "iOS 27.0".into(), slim: None, platform: "ios".into() }
    }

    fn state(leases: Leases) -> State {
        State { procs: vec![], leases, stopping: false, writing: HashSet::new(), pruned_at: 0 }
    }

    #[test]
    fn a_queued_run_waits_its_turn_with_dibs_waiters_and_adopts_its_lease() {
        let mut ls = Leases::default();
        let held = ls.acquire("w1:p1", None, None, false, Some(phone()), 0).unwrap();
        let mut st = state(ls);
        let claim = |st: &mut State, holder: &str, queue: Option<&str>| claim_device(st, |_| false, holder, None, false, &phone(), "x", queue);
        let (lease, rx_a) = claim(&mut st, "claude:a", Some("a-1")).unwrap();
        assert!(lease.is_none());
        let (_, mut rx_dibs) = st.leases.enqueue("w1:p3", None, None, Some(phone()), None, 0);
        let (_, rx_b) = claim(&mut st, "claude:b", Some("b-1")).unwrap();
        assert_eq!(st.leases.queue.iter().map(|w| w.process.as_deref()).collect::<Vec<_>>(), [Some("a-1"), None, Some("b-1")]);
        let e = claim(&mut st, "w1:p9", None).unwrap_err();
        assert_eq!(e.downcast_ref::<Coded>().unwrap().0, "busy", "without `queue` (the pane's runs)");
        // FIFO across run and dibs waiters; the run's lease becomes its run lease.
        st.leases.release(held.id, 0);
        let got = rx_a.unwrap().try_recv().unwrap();
        assert!(!got.run);
        st.procs.push(my_app("a-1", vec![], &held, false));
        assert!(adopt(&mut st, "a-1", &got));
        assert!(st.leases.leases.iter().any(|l| l.id == got.id && l.run) && st.procs[0].leases == [got.id]);
        st.leases.release(got.id, 0);
        let dibs = rx_dibs.try_recv().unwrap();
        st.leases.release(dibs.id, 0);
        // A process gone by the time its turn comes releases the lease.
        let late = rx_b.unwrap().try_recv().unwrap();
        assert!(!adopt(&mut st, "b-1", &late));
        assert!(st.leases.device_lease(&phone().udid).is_none());
    }

    #[tokio::test]
    async fn a_device_handed_to_a_run_still_building_goes_back_when_it_ends() {
        let mut ls = Leases::default();
        let held = ls.acquire("w1:p1", None, None, false, Some(phone()), 0).unwrap();
        let mut st = state(ls);
        let mut rxs = vec![];
        for id in ["a-1", "b-1"] {
            rxs.push(claim_device(&mut st, |_| false, "claude:a", None, false, &phone(), "x", Some(id)).unwrap().1);
            let mut p = my_app(id, vec![], &held, false);
            p.state = St::Building;
            st.procs.push(p);
        }
        // Released during a-1's build: handed to it, though it doesn't take it (`adopt`) until its run phase.
        st.leases.release(held.id, 0);
        assert_eq!(st.leases.device_lease(&phone().udid).and_then(|l| l.process.clone()).as_deref(), Some("a-1"));
        let d: D = Arc::new(Daemon { st: Mutex::new(st), tx: broadcast::channel(16).0, dir: scratch(), logs: scratch(), built: 0 });
        finish(&d, "a-1", None); // its build failed
        assert_eq!(d.lock().leases.device_lease(&phone().udid).and_then(|l| l.process.clone()).as_deref(), Some("b-1"), "back, to the next in line");
        stop_where(&d, |p| p.id == "b-1").await; // stopped mid-build
        assert!(d.lock().leases.device_lease(&phone().udid).is_none());
        // A lease gone by the run phase (its TTL ran out) fails the run instead of using the device anyway.
        let mut st = d.lock();
        st.procs.push(my_app("c-1", vec![], &held, false));
        assert!(!adopt(&mut st, "c-1", &held));
        drop(rxs);
    }

    #[tokio::test]
    async fn a_queued_run_that_ends_or_is_stopped_leaves_the_queue() {
        let mut ls = Leases::default();
        let held = ls.acquire("w1:p1", None, None, false, Some(phone()), 0).unwrap();
        let mut st = state(ls);
        let mut rxs = vec![];
        for id in ["a-1", "b-1"] {
            rxs.push(claim_device(&mut st, |_| false, "claude:a", None, false, &phone(), "x", Some(id)).unwrap().1);
            let mut p = my_app(id, vec![], &held, false);
            p.state = St::Queued;
            st.procs.push(p);
        }
        let d: D = Arc::new(Daemon { st: Mutex::new(st), tx: broadcast::channel(16).0, dir: scratch(), logs: scratch(), built: 0 });
        finish(&d, "a-1", None); // its build failed
        stop_where(&d, |p| p.id == "b-1").await;
        assert!(d.lock().leases.queue.is_empty());
        d.lock().leases.release(held.id, 0);
        assert!(d.lock().leases.device_lease(&phone().udid).is_none(), "no ghost process got it");
        drop(rxs);
    }

    #[test]
    fn udid_fills_the_destination_args_and_needs_a_lease() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let args = s(&["test", "-destination", "id={udid}", "{udid}"]);
        assert_eq!(with_udid(args.clone(), Some("U1")).unwrap(), s(&["test", "-destination", "id=U1", "U1"]));
        assert_eq!(with_udid(args, None).unwrap_err().downcast_ref::<Coded>().unwrap().0, "bad_request");
        assert_eq!(with_udid(s(&["true"]), None).unwrap(), s(&["true"]));
    }

    #[test]
    fn a_destination_is_for_commands_one_at_a_time() {
        let t = |ecosystem: &str, kind: &str| Target { name: "t".into(), ecosystem: ecosystem.into(), dir: "/w".into(), kind: kind.into(), package: None, needs_port: false, port: None, target_dir: None };
        let code = |t: &Target, p: Value| check_dest(t, &p).err().map(|e| e.downcast_ref::<Coded>().unwrap().0.clone());
        assert_eq!(code(&t("cargo", "test"), json!({"simulator": true})), Some("bad_request".into()));
        assert_eq!(code(&t("task", "exec"), json!({"simulator": true, "physical": "Test iPhone"})), Some("bad_request".into()));
        assert_eq!(code(&t("task", "exec"), json!({"device": "iPhone 18 Pro"})), Some("bad_request".into()));
        assert_eq!(code(&t("task", "exec"), json!({"simulator": true, "device": "iPhone 18 Pro"})), None);
        assert_eq!(code(&t("task", "task"), json!({"physical": "Test iPhone"})), None);
        assert_eq!(code(&t("xcode", "app"), json!({"device": "iPhone 18 Pro"})), None, "an Xcode target's own --device");
    }

    #[test]
    fn released_device_goes_to_the_waiter_and_is_never_retired() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut ls = Leases { retire: Some(tx), ..Default::default() };
        let a = ls.acquire("w1:p1", None, None, false, Some(phone()), 0).unwrap();
        let v = json!(a);
        assert_eq!((&v["resource"], &v["device"]["name"], v.get("port"), v.get("simulator")), (&json!("device"), &json!("Test iPhone"), None, None));
        assert!(ls.acquire("w1:p2", None, None, false, Some(phone()), 0).is_none(), "one holder at a time");
        let again = ls.acquire("w1:p1", None, None, false, Some(phone()), 5).unwrap();
        assert_eq!((again.id, again.expires_at, ls.leases.len()), (a.id, 5 + TTL_MS, 1), "the holder gets its own lease back, renewed");
        assert!(ls.acquire("w1:p2", None, None, false, None, 0).is_some(), "ports are unaffected");
        let (_, mut rx2) = ls.enqueue("w1:p2", None, None, Some(phone()), None, 0);
        let (_, mut rx3) = ls.enqueue("w1:p3", None, None, Some(phone()), None, 0);
        assert_eq!(json!(ls.queue[0])["device"]["udid"], "00000000-0000000000000001");
        assert_eq!((json!(ls.queue[0])["resource"].as_str(), json!(ls.queue[0])["preferred"].is_null()), (Some("device"), true));
        ls.release(a.id, 0);
        let got = rx2.try_recv().unwrap();
        assert_eq!((got.holder.as_str(), got.device.as_ref().map(|s| s.name.as_str())), ("w1:p2", Some("Test iPhone")));
        assert!(rx3.try_recv().is_err(), "FIFO: the second waiter still waits");
        ls.release_holder("w1:p2", 0);
        assert_eq!(rx3.try_recv().unwrap().holder, "w1:p3");
        assert!(rx.try_recv().is_err(), "a physical device is never retired");
        assert!(ls.devices.is_empty(), "nor recorded as a created simulator");
    }

    #[test]
    fn restart_drops_run_device_leases_without_retiring_them() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/leases-load-device");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dev = r#""device":{"udid":"00000000-0000000000000001","name":"Test iPhone","device":"iPhone 13 Pro","runtime":"iOS 27.0"}"#;
        let held = format!(r#"{{"id":2,"holder":"w1:p2","agent":null,"resource":"device",{dev},"acquired_at":0,"expires_at":9}}"#);
        let leases = format!(r#"[{{"id":1,"holder":"w1:p1","agent":null,"resource":"device",{dev},"acquired_at":0,"expires_at":9,"run":true}},{held}]"#);
        std::fs::write(dir.join("leases.json"), leases).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ls = Leases::load(&dir, tx);
        // Unchanged but for `slim`, which a device always shows now (null: stock).
        let kept = held.replace(r#""runtime":"iOS 27.0""#, r#""runtime":"iOS 27.0","slim":null"#);
        assert_eq!(serde_json::to_string(&ls.leases).unwrap(), format!("[{kept}]"), "a free-standing device lease round-trips");
        assert!(rx.try_recv().is_err());
        assert!(!ls.retiring());
    }

    #[test]
    fn restart_carries_the_device_and_another_holder_is_busy() {
        let mut ls = Leases::default();
        let id = ls.reserve();
        let l = ls.grant(id, "w1:p1", None, None, None, Some(phone()), true, 0);
        let mut st = State { procs: vec![my_app("APP-1", vec![l.id], &l, true)], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 };
        let same = |p: &Proc| p.target == "MyApp";
        let code = |r: Result<Lease>| r.unwrap_err().downcast_ref::<Coded>().unwrap().0.clone();
        assert_eq!(code(lease_device(&mut st, same, "w1:p2", None, false, &phone(), "MyApp")), "busy");
        assert_eq!(st.procs[0].leases, [l.id], "a refused run takes nothing");
        let carried = lease_device(&mut st, same, "w1:p1", None, false, &phone(), "MyApp").unwrap();
        assert_eq!(carried.id, l.id);
        assert!(carried.expires_at > TTL_MS, "the restart gets a fresh TTL");
        assert!(st.procs[0].leases.is_empty(), "stopping the old process leaves it alone");
        // A take-over: the device moves to the new holder with the target, under the same lease.
        st.procs[0].leases = vec![l.id];
        let taken = lease_device(&mut st, same, "w1:p2", Some("codex".into()), true, &phone(), "MyApp").unwrap();
        assert_eq!((taken.id, taken.holder.as_str(), taken.agent.as_deref()), (l.id, "w1:p2", Some("codex")));
        assert_eq!(st.leases.leases[0].holder, "w1:p2");
        assert!(st.procs[0].leases.is_empty());
    }

    #[test]
    fn run_adopts_the_holders_free_standing_device_lease() {
        let mut ls = Leases::default();
        let held = ls.acquire("w1:p1", None, None, false, Some(phone()), 0).unwrap();
        assert!(!held.run);
        let mut st = State { procs: vec![], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 };
        let same = |p: &Proc| p.target == "MyApp";
        let code = |r: Result<Lease>| r.unwrap_err().downcast_ref::<Coded>().unwrap().0.clone();
        assert_eq!(code(lease_device(&mut st, same, "w1:p2", None, true, &phone(), "MyApp")), "busy", "take doesn't take a free-standing lease");
        let adopted = lease_device(&mut st, same, "w1:p1", None, false, &phone(), "MyApp").unwrap();
        assert_eq!(adopted.id, held.id);
        assert!(adopted.run && adopted.expires_at > TTL_MS, "now the process's, renewed");
        assert_eq!(st.leases.leases.len(), 1);
        st.leases.release(held.id, 0);
        let fresh = lease_device(&mut st, same, "w1:p2", None, false, &phone(), "MyApp").unwrap();
        assert_eq!((fresh.holder.as_str(), fresh.run), ("w1:p2", true));
    }

    #[test]
    fn restart_stops_only_groups_whose_leader_is_unchanged() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/groups");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ls = Leases { snapshot: Some(dir.join("leases.json")), groups: vec![(10, "A".into()), (20, "B".into()), (30, "C".into())], ..Default::default() };
        ls.save();
        let ls = Leases::load(&dir, mpsc::unbounded_channel().0);
        // 10 still runs, 20's pid now belongs to a later process, 30 is gone.
        let now = |g: u32| [(10, "A"), (20, "B2")].iter().find(|(p, _)| *p == g).map(|(_, s)| s.to_string());
        assert_eq!(survivors(&ls.groups, now), [10]);
        let me = std::process::id();
        assert_eq!(survivors(&[(me, started(me).unwrap())], started), [me], "ps reads a live process's start time");
        assert!(survivors(&[(me, "Thu Jan  1 00:00:00 1970".into())], started).is_empty());
    }

    #[test]
    fn a_failed_device_build_releases_the_device_to_the_waiter() {
        let mut ls = Leases::default();
        let id = ls.reserve();
        let l = ls.grant(id, "w1:p1", None, None, None, Some(phone()), true, 0);
        let (_, mut rx) = ls.enqueue("w1:p2", None, None, Some(phone()), None, 0);
        let mut building = my_app("APP-1", vec![l.id], &l, false);
        building.state = St::Building;
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs: vec![building], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(1).0, dir: scratch(), logs: scratch(), built: 0 });
        use std::os::unix::process::ExitStatusExt;
        finish(&d, "APP-1", Some(ExitStatus::from_raw(1 << 8)));
        assert_eq!(rx.try_recv().unwrap().holder, "w1:p2", "unlike a simulator, it doesn't stay with the ended process");
    }

    #[tokio::test]
    async fn a_device_run_is_not_launched_at_spawn() {
        let mut ls = Leases::default();
        let id = ls.reserve();
        let l = ls.grant(id, "w1:p1", None, None, None, Some(phone()), true, 0);
        let mut port_only = my_app("NODE-1", vec![], &l, false);
        port_only.device = None;
        let procs = vec![my_app("APP-1", vec![l.id], &l, false), port_only];
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs, leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(16).0, dir: scratch(), logs: scratch(), built: 0 });
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let log = Mutex::new(LogFile::open(&tmp.join("device-launched.log")).unwrap());
        for id in ["APP-1", "NODE-1"] {
            exec(&d, id, &log, &eco::Xcode, std::process::Command::new("true"), &HashMap::new(), None, St::Running).await;
        }
        let launched: Vec<bool> = d.lock().procs.iter().map(|p| p.launched).collect();
        assert_eq!(launched, [false, true]);
    }

    #[tokio::test]
    async fn a_lock_wait_blocks_until_newer_output_and_the_end_is_stamped() {
        let mut ls = Leases::default();
        let id = ls.reserve();
        let l = ls.grant(id, "w1:p1", None, None, None, None, true, 0);
        let mut p = my_app("TASK-1", vec![], &l, false);
        p.device = None;
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs: vec![p], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(64).0, dir: scratch(), logs: scratch(), built: 0 });
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let log = Mutex::new(LogFile::open(&tmp.join("lock-wait.log")).unwrap());
        let sh = |script: &str| {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", script]);
            c
        };
        let blocked = |d: &D| d.lock().procs[0].blocked;
        let lock = "echo '    Blocking waiting for file lock on build directory' >&2";
        exec(&d, "TASK-1", &log, &eco::Task, sh(lock), &HashMap::new(), None, St::Running).await;
        assert!(blocked(&d));
        exec(&d, "TASK-1", &log, &eco::Task, sh(&format!("{lock}; echo '   Compiling beta v0.1.0' >&2")), &HashMap::new(), None, St::Running).await;
        assert!(!blocked(&d), "a newer line clears it");
        exec(&d, "TASK-1", &log, &eco::Task, sh(lock), &HashMap::new(), None, St::Running).await;
        assert_eq!(d.lock().procs[0].ended_at, None);
        use std::os::unix::process::ExitStatusExt;
        finish(&d, "TASK-1", Some(ExitStatus::from_raw(0)));
        let v = json!(d.lock().procs[0]);
        assert!(v["ended_at"].as_u64().is_some_and(|t| t <= now_ms()), "{v}");
        assert_eq!((&v["blocked"], &v["state"], &v["code"], &v["signal"]), (&json!(false), &json!("exited"), &json!(0), &Value::Null));
        // Killed by SIGKILL (a raw wait status of 9): no exit code, the signal instead.
        finish(&d, "TASK-1", Some(ExitStatus::from_raw(9)));
        let v = json!(d.lock().procs[0]);
        assert_eq!((&v["state"], &v["code"], &v["signal"]), (&json!("crashed"), &Value::Null, &json!(9)));
    }

    #[test]
    fn physical_choice_round_trips_and_a_kind_clears_it() {
        let iphone = |id: &str| json!({"identifier": id, "name": id, "productFamily": "iPhone"});
        let rts = vec![json!({"identifier": "27.0", "name": "27.0", "version": "27.0", "supportedDeviceTypes": [iphone("18 Pro"), iphone("17 Pro")]})];
        let picked = json!({"device": "17 Pro", "runtime": "27.0", "head": true});
        // Choosing a device resolves no kind: sim.set does it without a runtime installed.
        let next = next_choice(&picked, &json!({"physical": "Test iPhone"}), None, Some("U"));
        assert_eq!(next, json!({"device": "17 Pro", "runtime": "27.0", "head": true, "physical": "U"}), "the simulator kind is kept");
        assert_eq!(next_choice(&next, &json!({"head": false}), None, None)["physical"], "U", "head leaves it");
        let k = eco::resolve(&rts, &next, None, Some("27.0")).unwrap();
        assert_eq!(next_choice(&next, &json!({"runtime": "27.0"}), Some(&k), None), json!({"device": "17 Pro", "runtime": "27.0", "head": true}));
        let alone = next_choice(&Value::Null, &json!({"physical": "Test iPhone"}), None, Some("U"));
        assert_eq!(alone, json!({"head": false, "physical": "U"}));
        assert_eq!(sim_json(&eco::Ios, &Err(anyhow::anyhow!("no runtimes")), &alone), Value::Null, "sim.set then answers with just `physical`");
        let ds = [eco::Physical { udid: "U".into(), name: "Test iPhone".into(), model: "iPhone 13 Pro".into(), os: "iOS 27.0".into(), state: "connected".into(), developer_mode: true, paired: true }];
        assert_eq!(physical_json(&ds, "U")["model"], "iPhone 13 Pro");
        assert_eq!(physical_json(&[], "U"), json!({"udid": "U", "name": "U", "state": "unavailable"}));
    }

    fn kind(device: &str) -> SimKind {
        SimKind { device: "".into(), device_name: device.into(), runtime: "".into(), runtime_name: "iOS 27.0".into() }
    }

    /// A `MyApp` process held by `w1:p1` on `sim`'s device.
    fn my_app(id: &str, leases: Vec<u64>, sim: &Lease, launched: bool) -> Proc {
        let t = Target {
            name: "MyApp".into(),
            ecosystem: "xcode".into(),
            dir: "/r".into(),
            kind: "app".into(),
            package: None,
            needs_port: false,
            port: None,
            target_dir: None,
        };
        Proc {
            id: id.into(),
            target: t.name.clone(),
            kind: t.kind.clone(),
            dir: t.dir.clone(),
            state: St::Running,
            code: None,
            signal: None,
            pid: None,
            holder: "w1:p1".into(),
            agent: None,
            leases,
            port: None,
            simulator: sim.simulator.clone(),
            device: sim.device.clone(),
            since: 0,
            ended_at: None,
            built: true,
            launched,
            blocked: false,
            tests: None,
            only: false,
            branch: None,
            problems: None,
            timed_out: false,
            t,
        }
    }

    /// An ecosystem made of the trait alone, never registered in `ECOSYSTEMS`: what the daemon needs from one.
    struct Toy;

    impl Ecosystem for Toy {
        fn id(&self) -> &'static str {
            "toy"
        }
        fn detect(&self, dir: &Path) -> Result<Vec<Target>> {
            Ok(vec![Target { name: "suite".into(), ecosystem: "toy".into(), dir: dir.into(), kind: "test".into(), package: None, needs_port: false, port: None, target_dir: None }])
        }
        fn needs(&self, _: &Target) -> Vec<Resource> {
            vec![]
        }
        fn build(&self, _: &Target, _: &Ctx) -> Option<std::process::Command> {
            None
        }
        fn run(&self, _: &Target, _: &Ctx) -> std::process::Command {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", "echo 'pass a'; echo 'fail b'; echo 'pass c'"]);
            c
        }
        fn classify(&self, _: Source, line: &str) -> (Level, String) {
            (Level::Info, line.into())
        }
        fn outcome(&self, line: &str) -> Option<&'static str> {
            [("pass ", "passed"), ("fail ", "failed")].iter().find(|(p, _)| line.starts_with(p)).map(|(_, o)| *o)
        }
        fn select(&self, _: &Target, ids: &[String], mut args: Vec<String>) -> Result<Vec<String>> {
            args.extend(ids.iter().map(|id| format!("--only={id}")));
            Ok(args)
        }
    }

    /// `Toy` without its optional methods: the defaults.
    struct Bare;

    impl Ecosystem for Bare {
        fn id(&self) -> &'static str {
            "bare"
        }
        fn detect(&self, dir: &Path) -> Result<Vec<Target>> {
            Toy.detect(dir)
        }
        fn needs(&self, t: &Target) -> Vec<Resource> {
            Toy.needs(t)
        }
        fn build(&self, t: &Target, ctx: &Ctx) -> Option<std::process::Command> {
            Toy.build(t, ctx)
        }
        fn run(&self, t: &Target, ctx: &Ctx) -> std::process::Command {
            Toy.run(t, ctx)
        }
        fn classify(&self, src: Source, line: &str) -> (Level, String) {
            Toy.classify(src, line)
        }
    }

    #[tokio::test]
    async fn an_ecosystem_needs_only_the_trait_for_counts_only_and_args() {
        let t = Toy.detect(Path::new("/w")).unwrap().remove(0);
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(only_args(&Toy, &t, &s(&["a", "c"]), s(&["-v"])).unwrap(), ["-v", "--only=a", "--only=c"], "its own select");
        assert_eq!(only_args(&Toy, &t, &[], s(&["-v"])).unwrap(), ["-v"], "none: the whole run");
        let refused = only_args(&Bare, &t, &s(&["a"]), vec![]).unwrap_err();
        assert_eq!(refused.downcast_ref::<Coded>().unwrap().0, "bad_request", "the default select refuses");
        assert!(Bare.check_args(&t, &s(&["-v"])).is_ok(), "the default check_args takes any args");
        // Its outcome lines reach the live counts.
        let mut ls = Leases::default();
        let id = ls.reserve();
        let l = ls.grant(id, "w1:p1", None, None, None, None, true, 0);
        let mut p = my_app("TOY-1", vec![], &l, false);
        (p.kind, p.device) = ("test".into(), None);
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs: vec![p], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(64).0, dir: scratch(), logs: scratch(), built: 0 });
        let log = Mutex::new(LogFile::open(&scratch().join("toy.log")).unwrap());
        exec(&d, "TOY-1", &log, &Toy, Toy.run(&t, &Ctx::default()), &HashMap::new(), None, St::Running).await;
        let tests = d.lock().procs[0].tests.clone().unwrap();
        assert_eq!((&tests["passed"], &tests["failed"], &tests["skipped"]), (&json!(2), &json!(1), &json!(0)));
    }

    /// A platform made of the required methods alone, recording what the daemon asks of it.
    struct ToyPlatform;

    static TOY_CALLS: Mutex<Vec<String>> = Mutex::new(vec![]);

    impl Platform for ToyPlatform {
        fn id(&self) -> &'static str {
            "toy"
        }
        fn runtimes(&self, _: Option<&str>) -> Result<Vec<Value>> {
            Ok(vec![])
        }
        fn resolve(&self, _: &[Value], _: &Value, _: Option<&str>, _: Option<&str>) -> Result<SimKind> {
            Ok(SimKind { device: "toy-phone".into(), device_name: "Toy Phone".into(), runtime: "toy-1".into(), runtime_name: "Toy 1".into() })
        }
        fn create(&self, name: &str, kind: &SimKind, _: Option<&str>) -> Result<Sim> {
            TOY_CALLS.lock().unwrap().push(format!("create {name}"));
            Ok(Sim { udid: "FAKE1".into(), name: name.into(), device: kind.device_name.clone(), runtime: kind.runtime_name.clone(), slim: None, platform: "toy".into() })
        }
        fn retire(&self, id: &str) -> Result<()> {
            TOY_CALLS.lock().unwrap().push(format!("retire {id}"));
            Ok(())
        }
        fn named(&self, _: &[String]) -> Result<Vec<String>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn a_platform_needs_only_the_trait_to_create_and_retire_devices() {
        let code = |e: anyhow::Error| e.downcast_ref::<Coded>().unwrap().0.clone();
        let kind = ToyPlatform.resolve(&[], &Value::Null, None, None).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ls = Leases { retire: Some(tx), ..Default::default() };
        let d: D = Arc::new(Daemon { st: Mutex::new(State { procs: vec![], leases: ls, stopping: false, writing: HashSet::new(), pruned_at: 0 }), tx: broadcast::channel(16).0, dir: scratch(), logs: scratch(), built: 0 });
        // A template of the same kind: the default clone refuses, so the device is created instead.
        let tm = Sim { udid: "TMPL".into(), name: "t".into(), device: kind.device_name.clone(), runtime: kind.runtime_name.clone(), slim: None, platform: "toy".into() };
        let (l, note, slimmed) = new_sim(&d, &ToyPlatform, "w1:p1", None, kind.clone(), false, None, Some(tm.clone()), Some(vec![])).await.unwrap();
        let (level, line) = note.unwrap();
        assert!(level == Level::Warn && line.contains("toy devices don't support cloning"), "{line}");
        assert_eq!(code(slimmed.unwrap_err()), "bad_request", "nor slimming");
        let s = l.simulator.clone().unwrap();
        assert_eq!((s.udid.as_str(), s.platform.as_str(), s.slim.as_ref()), ("FAKE1", "toy", None));
        assert!(TOY_CALLS.lock().unwrap().iter().any(|c| c.starts_with("create collie ")), "created through the trait");
        assert_eq!(d.lock().leases.devices, ["toy:FAKE1"], "recorded under its platform");
        // Released, it is retired on its own platform.
        d.lock().leases.release(l.id, 0);
        let key = rx.try_recv().unwrap();
        eco::retire_device(&[&eco::Ios, &ToyPlatform], &key).unwrap();
        assert_eq!(TOY_CALLS.lock().unwrap().last().unwrap(), "retire FAKE1");
        // What it doesn't implement is refused by name.
        assert_eq!(code(ToyPlatform.clone_device(&tm, "x", None).unwrap_err()), "bad_request");
        assert_eq!(code(ToyPlatform.slim_categories().unwrap_err()), "bad_request");
        assert_eq!(code(ToyPlatform.physical_devices().unwrap_err()), "bad_request");
        assert_eq!(code(ToyPlatform.find_physical(&[], "x", false).unwrap_err()), "bad_request");
        assert!(!ToyPlatform.slim_persists("Toy 1"));
    }
}
