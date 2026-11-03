//! Build slots: how many builds (build steps, `exec` commands, test runs, CI jobs holding a build dib) run at once. The
//! cap drops while the slot holders' processes use too much CPU or memory, and climbs back once they don't; it never
//! drops below one.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Samples this far apart; the controller's counts below are in samples.
pub const EVERY_MS: u64 = 5000;
/// Over a threshold this many samples in a row (10 s) lowers the cap by one.
const OVER: u32 = 2;
/// Under 80% of both thresholds this many samples in a row (30 s) raises it by one.
const UNDER: u32 = 6;

/// The `[builds]` table of the global config. Read again on every sample, so an edit applies without a daemon restart
/// (which would stop every process).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Knobs {
    /// Most builds at once.
    pub max: usize,
    /// Percent of all cores the slot holders' processes may use together.
    pub cpu: f64,
    /// Percent of RAM the slot holders' processes may use together.
    pub memory: f64,
}

impl Knobs {
    fn defaults(cores: usize) -> Self {
        // ponytail: cores / 4 because one xcodebuild or cargo build already spreads over every core; tune per machine.
        Knobs { max: (cores / 4).max(1), cpu: 70.0, memory: 50.0 }
    }
}

#[derive(Deserialize, Default)]
struct File {
    builds: Option<Partial>,
    remote: Option<Remote>,
}

/// The `[remote]` table: another Mac's daemon that takes slot work when this one has no free slot.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// The ssh destination its socket is forwarded from; without one, `socket` is a path on this machine (tests).
    pub host: Option<String>,
    /// Its daemon's socket, on its machine.
    pub socket: String,
    /// Absolute path prefixes here and where the same trees live there, one to one.
    #[serde(default)]
    pub paths: std::collections::BTreeMap<String, String>,
}

impl Remote {
    /// How it shows in logs and status: its ssh host, else `remote`.
    pub fn name(&self) -> &str {
        self.host.as_deref().unwrap_or("remote")
    }
}

/// The `[remote]` table of the config at `path`; None without one, or when the file doesn't parse (`knobs` reports
/// that as `config_error`).
pub fn remote(path: &Path) -> Option<Remote> {
    toml::from_str::<File>(&std::fs::read_to_string(path).ok()?).ok()?.remote
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Partial {
    max: Option<usize>,
    cpu: Option<f64>,
    memory: Option<f64>,
}

/// `~/.config/collie/config.toml` (or under `$XDG_CONFIG_HOME`); beside the state when `COLLIE_STATE_DIR` isolates a
/// daemon (tests).
pub fn config_path() -> PathBuf {
    if let Some(d) = std::env::var_os("COLLIE_STATE_DIR") {
        return PathBuf::from(d).join("config.toml");
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()).map(PathBuf::from);
    base.unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".config")).join("collie/config.toml")
}

/// The knobs `path` sets over the defaults for `cores`; with the defaults, the reason when it doesn't parse.
pub fn knobs(path: &Path, cores: usize) -> (Knobs, Option<String>) {
    let d = Knobs::defaults(cores);
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (d, None),
        Err(e) => return (d, Some(format!("{}: {e}", path.display()))),
    };
    match toml::from_str::<File>(&text) {
        Ok(f) => {
            let p = f.builds.unwrap_or_default();
            (Knobs { max: p.max.unwrap_or(d.max).max(1), cpu: p.cpu.unwrap_or(d.cpu), memory: p.memory.unwrap_or(d.memory) }, None)
        }
        Err(e) => (d, Some(format!("{}: {e}", path.display()))),
    }
}

/// Logical cores and RAM in bytes.
pub fn machine() -> (usize, u64) {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let ram = if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl").args(["-n", "hw.memsize"]).output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).and_then(|s| s.trim().parse().ok())
    } else {
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).map(|kb| kb * 1024)
    };
    (cores, ram.unwrap_or(0))
}

/// One `ps` row: pid, parent pid, CPU in percent of one core, resident memory in bytes.
pub type Row = (u32, u32, f64, u64);

/// Every process on the machine, from one `ps`.
pub fn sample() -> Vec<Row> {
    let out = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid=,%cpu=,rss="]).output();
    let text = out.ok().and_then(|o| String::from_utf8(o.stdout).ok()).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (pid, ppid, cpu, rss) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?.parse::<u64>().ok()?);
            Some((pid, ppid, cpu, rss * 1024))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    /// Percent of one core, summed: 250 is two and a half cores.
    pub cpu: f64,
    /// Resident bytes, summed.
    pub mem: u64,
}

/// `root` and all its descendants, by parent pid. Not by process group: xcodebuild puts its build service, and that its
/// compilers, each in a group of its own.
pub fn tree(rows: &[Row], root: u32) -> Vec<u32> {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, ppid, ..) in rows {
        kids.entry(ppid).or_default().push(pid);
    }
    let (mut out, mut todo) = (vec![], vec![root]);
    while let Some(p) = todo.pop() {
        if rows.iter().any(|r| r.0 == p) {
            out.push(p);
        }
        todo.extend(kids.get(&p).into_iter().flatten().filter(|&&k| k != p));
    }
    out
}

pub fn usage(rows: &[Row], pids: &[u32]) -> Usage {
    rows.iter().filter(|r| pids.contains(&r.0)).fold(Usage::default(), |u, r| Usage { cpu: u.cpu + r.2, mem: u.mem + r.3 })
}

/// The cap and the pressure controller behind it.
#[derive(Debug, Serialize)]
pub struct Slots {
    /// Builds that may run at once now: between 1 and `knobs.max`.
    pub cap: usize,
    pub knobs: Knobs,
    pub cores: usize,
    pub ram: u64,
    /// collie's processes and the build dibs' together, at the last sample.
    pub usage: Usage,
    /// Every slot holder but one (the oldest CI holder, else the oldest) runs at background priority until the
    /// pressure is gone.
    pub slowed: bool,
    /// Holding a slot, oldest first.
    pub running: Vec<String>,
    /// Waiting for one: prioritised entries first, each group in arrival order.
    pub queued: Vec<String>,
    /// Why the config file was ignored, if it was.
    pub config_error: Option<String>,
    /// The `[remote]` daemon's slots at its last reading: `{"host", "cap", "running", "queued", "version", "at",
    /// "error"}`, counts, `at` in Unix ms, `error` null or why it takes no work (unreachable, version mismatch). Null
    /// without a `[remote]`.
    pub remote: Option<serde_json::Value>,
    #[serde(skip)]
    prioritised: HashSet<String>,
    #[serde(skip)]
    over: u32,
    #[serde(skip)]
    under: u32,
}

#[cfg(test)]
impl Default for Slots {
    fn default() -> Self {
        Slots::new(Knobs::defaults(4), 4, 0)
    }
}

impl Slots {
    pub fn new(knobs: Knobs, cores: usize, ram: u64) -> Self {
        Slots { cap: knobs.max, knobs, cores, ram, usage: Usage::default(), slowed: false, running: vec![], queued: vec![], config_error: None, remote: None, prioritised: HashSet::new(), over: 0, under: 0 }
    }

    /// Whether a process starting now would wait for a slot: none is free, or others wait already.
    pub fn would_queue(&self) -> bool {
        self.running.len() >= self.cap || !self.queued.is_empty()
    }

    /// Whether the remote may take work at `now` from a daemon at commit `ours`: read recently, without an error, at
    /// the same known commit.
    pub fn remote_usable(&self, now: u64, ours: &str) -> bool {
        // ponytail: two samples, not one: a reading is up to a sample plus the round trip old just before the next.
        let fresh = |r: &serde_json::Value| r["at"].as_u64().is_some_and(|at| now.saturating_sub(at) <= 2 * EVERY_MS);
        self.remote.as_ref().is_some_and(|r| fresh(r) && r["error"].is_null() && ours != "unknown" && r["version"] == ours)
    }

    /// `remote_usable`, with a slot free there and nothing (a CI job's dib included) waiting for one; `pending`: runs
    /// on their way there that its reading doesn't count yet.
    pub fn remote_idle(&self, now: u64, ours: &str, pending: usize) -> bool {
        let n = |k: &str| self.remote.as_ref().and_then(|r| r[k].as_u64()).unwrap_or_default();
        self.remote_usable(now, ours) && n("running") + (pending as u64) < n("cap") && n("queued") == 0
    }

    /// Queues `id` again where it was, at the front, but behind the prioritised entries: a move to the remote failed.
    pub fn put_back(&mut self, id: String) {
        let at = self.queued.iter().take_while(|q| self.prioritised.contains(*q)).count();
        self.queued.insert(at, id);
    }

    /// Whether `id`, queued, may take a slot now: one is free and nobody queued before it.
    pub fn may_start(&self, id: &str) -> bool {
        self.running.len() < self.cap && self.queued.first().is_some_and(|q| q == id)
    }

    /// Queues `id` last; with `first`, ahead of every entry queued without it but behind those queued with it.
    pub fn enqueue(&mut self, id: String, first: bool) {
        let at = match first {
            true => self.queued.iter().take_while(|q| self.prioritised.contains(*q)).count(),
            false => self.queued.len(),
        };
        if first {
            self.prioritised.insert(id.clone());
        }
        self.queued.insert(at, id);
    }

    pub fn leave(&mut self, id: &str) {
        self.queued.retain(|q| q != id);
        self.prioritised.remove(id);
    }

    /// Takes in one sample of the slot holders' total usage, under (possibly new) knobs.
    pub fn step(&mut self, knobs: Knobs, usage: Usage) {
        self.knobs = knobs;
        self.usage = usage;
        self.cap = self.cap.min(knobs.max);
        let cpu_limit = knobs.cpu * self.cores as f64;
        let mem_limit = knobs.memory / 100.0 * self.ram as f64;
        let over = usage.cpu > cpu_limit || (self.ram > 0 && usage.mem as f64 > mem_limit);
        let under = usage.cpu < 0.8 * cpu_limit && (self.ram == 0 || (usage.mem as f64) < 0.8 * mem_limit);
        (self.over, self.under) = (if over { self.over + 1 } else { 0 }, if under { self.under + 1 } else { 0 });
        if self.over >= OVER {
            // Below what runs now, or a cap above the running count would change nothing.
            // ponytail: steps down every 10 s while the pressure lasts; wait for running builds to end first if it
            // drops to 1 too eagerly.
            self.cap = (self.cap.min(self.running.len()).saturating_sub(1)).max(1);
            self.slowed = true;
            self.over = 0;
        } else if self.under >= UNDER {
            self.cap = (self.cap + 1).min(knobs.max);
            self.slowed = false;
            self.under = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;
    fn knobs(max: usize) -> Knobs {
        Knobs { max, cpu: 70.0, memory: 50.0 }
    }
    fn busy(s: &mut Slots, n: usize) {
        s.running = (0..n).map(|i| i.to_string()).collect();
    }

    #[test]
    fn a_tree_follows_parents_across_process_groups() {
        // collie's shell 10 → xcodebuild 11 → build service 12 → two compilers; 20 is someone else's.
        let rows = [(10, 1, 0.0, 1), (11, 10, 5.0, 2), (12, 11, 1.0, 4), (13, 12, 95.0, 8), (14, 12, 90.0, 16), (20, 1, 99.0, 32)];
        let mut t = tree(&rows, 10);
        t.sort();
        assert_eq!(t, [10, 11, 12, 13, 14]);
        assert_eq!(usage(&rows, &t), Usage { cpu: 191.0, mem: 31 });
        assert!(tree(&rows, 99).is_empty(), "a gone root has no tree");
    }

    #[test]
    fn pressure_lowers_the_cap_below_what_runs_but_never_below_one() {
        let mut s = Slots::new(knobs(4), 10, 32 * GB);
        busy(&mut s, 3);
        let hot = Usage { cpu: 900.0, mem: GB };
        s.step(knobs(4), hot);
        assert_eq!(s.cap, 4, "one hot sample is noise");
        s.step(knobs(4), hot);
        assert_eq!((s.cap, s.slowed), (2, true), "below the 3 running, so the next waits");
        for _ in 0..10 {
            s.step(knobs(4), hot);
        }
        assert_eq!(s.cap, 1);
        let memory_hot = Usage { cpu: 0.0, mem: 17 * GB };
        let mut m = Slots::new(knobs(2), 10, 32 * GB);
        busy(&mut m, 2);
        m.step(knobs(2), memory_hot);
        m.step(knobs(2), memory_hot);
        assert_eq!(m.cap, 1, "memory over half the RAM counts too");
    }

    #[test]
    fn calm_raises_the_cap_back_to_max_and_ends_the_slowdown() {
        let mut s = Slots::new(knobs(3), 10, 32 * GB);
        (s.cap, s.slowed) = (1, true);
        let calm = Usage { cpu: 100.0, mem: GB };
        for _ in 0..5 {
            s.step(knobs(3), calm);
        }
        assert_eq!((s.cap, s.slowed), (1, true), "30 s of calm first");
        s.step(knobs(3), calm);
        assert_eq!((s.cap, s.slowed), (2, false));
        for _ in 0..30 {
            s.step(knobs(3), calm);
        }
        assert_eq!(s.cap, 3, "never above max");
        let between = Usage { cpu: 600.0, mem: GB };
        s.cap = 2;
        for _ in 0..30 {
            s.step(knobs(3), between);
        }
        assert_eq!(s.cap, 2, "between 80% and the threshold holds the cap where it is");
        s.step(knobs(1), calm);
        assert_eq!(s.cap, 1, "a lower max in the config applies at once");
    }

    #[test]
    fn slots_go_in_arrival_order() {
        let mut s = Slots::new(knobs(1), 4, GB);
        s.queued = vec!["a".into(), "b".into()];
        assert!(s.may_start("a") && !s.may_start("b"));
        s.running.push(s.queued.remove(0));
        assert!(!s.may_start("b"), "the one slot is taken");
    }

    #[test]
    fn a_prioritised_entry_goes_ahead_of_waiting_ones_but_behind_earlier_prioritised_ones() {
        let mut s = Slots::new(knobs(1), 4, GB);
        s.running = vec!["agent".into()];
        for (id, first) in [("a", false), ("ci1", true), ("b", false), ("ci2", true)] {
            s.enqueue(id.into(), first);
        }
        assert_eq!(s.queued, ["ci1", "ci2", "a", "b"]);
        assert_eq!(s.running, ["agent"], "a running holder is never displaced");
        assert!(!s.may_start("ci1"), "and keeps its slot until it ends");
        s.running.clear();
        assert!(s.may_start("ci1"));
        s.leave("ci1");
        s.running.push("ci1".into());
        s.enqueue("ci3".into(), true);
        assert_eq!(s.queued, ["ci2", "ci3", "a", "b"], "one that left is no longer counted among the prioritised");
    }

    #[test]
    fn a_process_would_queue_with_every_slot_taken_or_others_waiting() {
        let mut s = Slots::new(knobs(2), 4, GB);
        assert!(!s.would_queue());
        busy(&mut s, 1);
        assert!(!s.would_queue(), "one of two taken");
        s.queued = vec!["dibs:1".into()];
        assert!(s.would_queue(), "a free slot, but a CI dib is about to take it");
        s.queued.clear();
        s.cap = 1;
        assert!(s.would_queue(), "the cap dropped under pressure");
    }

    #[test]
    fn the_remote_is_idle_only_when_fresh_free_error_free_and_at_our_commit() {
        let at = |s: &mut Slots, v: serde_json::Value| s.remote = Some(v);
        let mut s = Slots::new(knobs(1), 4, GB);
        let idle = serde_json::json!({"host": "mini", "cap": 2, "running": 1, "queued": 0, "version": "abc", "at": 1000, "error": null});
        assert!(!s.remote_idle(1000, "abc", 0), "no reading yet");
        at(&mut s, idle.clone());
        assert!(s.remote_idle(1000, "abc", 0));
        assert!(!s.remote_idle(1000, "abc", 1), "its free slot is already spoken for by a run on its way there");
        assert!(!s.remote_idle(1000 + 2 * EVERY_MS + 1, "abc", 0), "a stale reading");
        let with = |k: &str, v: serde_json::Value| {
            let mut r = idle.clone();
            r[k] = v;
            r
        };
        at(&mut s, with("queued", 1.into()));
        assert!(!s.remote_idle(1000, "abc", 0) && s.remote_usable(1000, "abc"), "a CI job queued there goes first");
        at(&mut s, with("cap", 1.into()));
        assert!(!s.remote_idle(1000, "abc", 0), "its cap dropped to what runs");
        at(&mut s, with("version", "def".into()));
        assert!(!s.remote_usable(1000, "abc"), "another commit");
        at(&mut s, with("version", "unknown".into()));
        assert!(!s.remote_usable(1000, "unknown"), "unknown on both sides is no match");
        at(&mut s, with("error", "unreachable".into()));
        assert!(!s.remote_usable(1000, "abc"));
    }

    #[test]
    fn a_put_back_entry_goes_first_but_behind_prioritised_ones() {
        let mut s = Slots::new(knobs(1), 4, GB);
        s.enqueue("ci".into(), true);
        s.enqueue("b".into(), false);
        s.put_back("a".into());
        assert_eq!(s.queued, ["ci", "a", "b"]);
    }

    #[test]
    fn the_config_reads_a_remote_table() {
        let dir = std::env::temp_dir().join(format!("collie-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert_eq!(remote(&path), None);
        std::fs::write(&path, "[builds]\nmax = 1\n[remote]\nhost = \"macmini\"\nsocket = \"/s.sock\"\n[remote.paths]\n\"/a\" = \"/b\"\n").unwrap();
        let r = remote(&path).unwrap();
        assert_eq!((r.name(), r.socket.as_str(), r.paths.get("/a").map(String::as_str)), ("macmini", "/s.sock", Some("/b")));
        assert_eq!(super::knobs(&path, 4).1, None, "the builds knobs still read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_config_sets_what_it_names_over_the_defaults() {
        let dir = std::env::temp_dir().join(format!("collie-builds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert_eq!(super::knobs(&path, 10).0, Knobs { max: 2, cpu: 70.0, memory: 50.0 }, "no file: defaults from the cores");
        std::fs::write(&path, "[builds]\nmax = 3\ncpu = 50\n").unwrap();
        assert_eq!(super::knobs(&path, 10), (Knobs { max: 3, cpu: 50.0, memory: 50.0 }, None));
        std::fs::write(&path, "[builds]\nmax = 0\n").unwrap();
        assert_eq!(super::knobs(&path, 10).0.max, 1, "never below one");
        std::fs::write(&path, "[builds]\nmaxx = 3\n").unwrap();
        let (k, err) = super::knobs(&path, 10);
        assert_eq!(k.max, 2);
        assert!(err.is_some_and(|e| e.contains("maxx")), "a typo is reported, not silently ignored");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
