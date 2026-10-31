use crate::daemon::{Coded, now_ms, state_dir, who};
use crate::eco::{Level, Source, Target};
use crate::logs::{Filter, Record};
use crate::{request, request_with, send_with};
use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::Block;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

const VIEW_CAP: usize = 5000;
/// Below this width the sidebar moves from a side column to a band on top.
const WIDE: u16 = 100;
/// Below this width the filter chips get their own line and buttons drop their words.
const NARROW: u16 = 70;

/// The running block's sections, in order: this worktree's non-agent processes, its agents' processes, processes in
/// the repo's other worktrees, processes in other repos. `section` says `elsewhere` for the last two; `place` splits it.
const SECTIONS: [&str; 4] = ["yours", "agents", "other worktrees", "other projects"];

/// The sidebar's tabs, in strip order (keys `1`–`5`), with a label and a short one for a narrow strip.
const TABS: [(Focus, &str, &str); 5] = [
    (Focus::Targets, "targets", "tgt"),
    (Focus::Tests, "tests", "test"),
    (Focus::History, "history", "hist"),
    (Focus::Dibs, "dibs", "dibs"),
    (Focus::Problems, "problems", "prob"),
];

/// A row in the targets tab, a target detected in this worktree; or, as `current` gives it, a pinned process.
#[derive(Clone)]
struct Item {
    name: String,
    dir: PathBuf,
    local: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Input {
    Filter,
    Search,
    /// The destination list is open: simulator runtimes, then physical devices.
    Runtime,
    /// The simulator device list is open.
    Device,
    /// The metadata toggles are open.
    Meta,
}

/// The running block, one of the `TABS`, or the logs.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Running,
    Targets,
    Tests,
    History,
    Dibs,
    Problems,
    Logs,
}

/// Which metadata columns go on the line under each log record; kept in `tui.json` in the state dir.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Meta {
    time: bool,
    level: bool,
    source: bool,
    pid: bool,
}

impl Default for Meta {
    fn default() -> Self {
        Meta { time: true, level: true, source: true, pid: false }
    }
}

const META: [&str; 4] = ["Time", "Level", "Source", "PID"];

impl Meta {
    fn get(&mut self, i: usize) -> &mut bool {
        match i {
            0 => &mut self.time,
            1 => &mut self.level,
            2 => &mut self.source,
            _ => &mut self.pid,
        }
    }
}

enum Prompt {
    /// The target, its holder, and the tests to run (`Act::Run`).
    Take(Item, String, Vec<String>),
    Lease(u64),
}

#[derive(Clone)]
enum Act {
    Select(usize),
    /// Run the target: only these tests (`run`'s `only`), all when empty.
    Run(Vec<String>),
    Take(Vec<String>),
    Stop,
    Mark,
    Open,
    Level(Option<Level>),
    Source(Option<Source>),
    Lease(u64),
    Release(u64),
    Renew(u64),
    Cancel,
    Toggle(usize),
    Edit(Input),
    /// A simulator runtime id.
    Runtime(String),
    /// Row `i` of the open runtime or device list.
    Pick(usize),
    /// Open the target's simulator in DeviceHub on its next runs.
    Head(bool),
    /// Slim the target's next new simulators (the stored setting), or not.
    Slim(bool),
    /// Flip metadata column `i` of `META`.
    Meta(usize),
    /// Focus the logs with the cursor on this row, counted from the bottom.
    Cursor(usize),
    /// Show row `i` of the history.
    Past(usize),
    /// Show row `i` of the running block.
    Live(usize),
    /// Open this tab, showing the sidebar.
    Tab(Focus),
    /// Hide or show the sidebar.
    Sidebar,
    /// Select row `i` of the tests tab.
    Test(usize),
    /// Mark or unmark row `i` of the tests tab.
    Check(usize),
    /// Select row `i` of the problems tab.
    Problem(usize),
    /// Show only the problems this branch added (true), or all.
    ProblemsNew(bool),
    /// Show only problems of this kind; again, all.
    ProblemsKind(&'static str),
    /// Show the report (true) or the logs of a test run.
    View(bool),
    /// Open a test failure of this process in its logs at this cursor; without one, its error records.
    Failure(String, Option<String>),
}

/// A row of the tests tab (a test target at depth 0, a group of its tests, or a test) or of the problems tab (a file,
/// or a problem in it).
struct TreeRow {
    depth: usize,
    /// The target's name; for a group or test the target's name, a tab, and its path joined by the results' `sep`. A
    /// problems row's starts `problems\t`.
    key: String,
    label: String,
    /// On a test's or a problem's row.
    item: Option<Value>,
    failed: usize,
    total: usize,
}

struct App {
    /// Feeds `on_event` from background requests (`sims`).
    tx: mpsc::Sender<Value>,
    /// This worktree: the git root of the pane's cwd, else the cwd; resolved.
    root: PathBuf,
    local: Vec<Target>,
    /// Local targets that run on a simulator, by name: the daemon's current choice (`targets`' `sim`, null without Xcode).
    sim: HashMap<String, Value>,
    /// `sims`' runtimes; None while loading.
    runtimes: Option<Vec<Value>>,
    /// `sims`' physical devices.
    physical: Vec<Value>,
    /// Filter typed into the device list, and the highlighted row of the open list.
    device: String,
    pick: usize,
    items: Vec<Item>,
    status: Value,
    sel: usize,
    /// `None` is a mark rule.
    view: Vec<Option<Record>>,
    expanded: HashSet<usize>,
    expand_all: bool,
    proc_id: Option<String>,
    filter: String,
    search: String,
    input: Option<Input>,
    level: Option<Level>,
    source: Option<Source>,
    prompt: Option<Prompt>,
    msg: String,
    /// Log rows scrolled up from the bottom; 0 follows new output.
    scroll: usize,
    page: usize,
    /// Clickable regions of the last frame.
    hits: Vec<(Rect, Act)>,
    logs_area: Rect,
    /// The running block and the tab block as last drawn, for the wheel; empty when not drawn.
    running_area: Rect,
    tabs_area: Rect,
    /// The selected lease in the dibs tab.
    dibs_sel: usize,
    meta: Meta,
    /// Whether the sidebar (running block and tabs) shows; kept in `tui.json`.
    sidebar: bool,
    /// The open tab: one of `TABS`.
    tab: Focus,
    /// Seconds east of UTC, for log timestamps.
    utc_offset: i64,
    /// The shown process's pid, kept after it exits (status then reports none).
    pid: Option<u64>,
    focus: Focus,
    /// The log cursor's row, counted from the bottom like `scroll`.
    cursor: usize,
    /// Ended runs from `history`, oldest first; fetched on (re)connect.
    history: Vec<Value>,
    /// The history tab's rows: see `rebuild_runs`.
    runs: Vec<Value>,
    /// The running block's rows, with their section: see `rebuild_running`.
    running: Vec<(&'static str, Value)>,
    /// The id of the process picked in the running block or the history; its log shows instead of the selected
    /// target's latest.
    run: Option<String>,
    /// Each local test target's results (`tests`' target form, null for none), by name, with the newest ended run
    /// they were fetched after.
    results: HashMap<String, (String, Value)>,
    /// The tests tab's unfolded rows, by `TreeRow::key`.
    open: HashSet<String>,
    /// Marked tests and groups as `<target>\t<test or group id>` (`row_id`), all of one target.
    marks: HashSet<String>,
    test_sel: usize,
    /// `t`'s choice between the report and the logs of a test run, for the session; None: `report_on`'s default.
    report: Option<bool>,
    /// The shown process's results (`tests`' process form, null for none), by its id.
    report_data: Option<(String, Value)>,
    /// The selected failure in the report.
    report_sel: usize,
    /// The process whose failure was opened in its logs: they show instead of its report, leaving `report` as it is.
    logs_for: Option<String>,
    /// `problems`' last answer for this worktree (null before the first), and whether a request is out.
    problems: Value,
    fetching: bool,
    /// The problems tab's view: new ones only (`b`), one kind only (`w`); and its selected row.
    problems_new: bool,
    problems_kind: Option<&'static str>,
    problems_sel: usize,
    /// The newest ended run of a target in this worktree when the problems were asked for.
    problems_after: Option<String>,
    /// The screen needs a full redraw: an editor ran in it.
    redraw: bool,
    /// `repo`'s answers, by dir.
    repos: std::cell::RefCell<HashMap<PathBuf, Option<PathBuf>>>,
}

/// `targets` as an event for `App::on_event`. A broken collie.toml (`bad_config`) mustn't keep the pane shut: the
/// event then carries no targets and the error, and the next one (after a reconnect) picks a fixed file up.
/// `start` as in `request_with`.
fn targets_event(start: bool) -> Value {
    match request_with("targets", json!({}), start) {
        Ok(v) => json!({"event": "targets", "data": v["targets"], "msg": ""}),
        Err(e) => {
            let fix = if e.downcast_ref::<Coded>().is_some_and(|c| c.0 == "bad_config") { " (fix it, then reopen the pane or restart the daemon)" } else { "" };
            json!({"event": "targets", "data": [], "msg": format!("{e:#}{fix}").replace('\n', " ")})
        }
    }
}

pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let top = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).current_dir(&cwd).output();
    let root = match top {
        Ok(o) if o.status.success() => PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()),
        _ => cwd,
    };
    // Resolved once, as process dirs are in `section`: a cwd reached through a symlink must still match.
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let targets = targets_event(true);
    let (tx, rx) = mpsc::channel();
    let app_tx = tx.clone();
    let events = send_with("subscribe", json!({}), true)?;
    std::thread::spawn(move || {
        let mut events = Some(events);
        loop {
            for frame in events.take().into_iter().flatten().map_while(Result::ok) {
                if tx.send(frame).is_err() {
                    return;
                }
            }
            if tx.send(json!({"event": "gone"})).is_err() {
                return;
            }
            // Waits for a daemon instead of starting one (as `send` would): the pane mustn't bring back a daemon
            // someone just stopped. Its first status frame, and a fresh `targets`, replace the stale state.
            while events.is_none() {
                std::thread::sleep(Duration::from_secs(1));
                events = send_with("subscribe", json!({}), false).ok();
            }
            if tx.send(targets_event(false)).is_err() {
                return;
            }
        }
    });
    let mut app = App::new(app_tx, root);
    (app.meta, app.sidebar) = load_ui(&state_dir().join("tui.json"));
    if !app.sidebar {
        app.focus = Focus::Logs;
    }
    app.on_event(targets);
    let mut term = ratatui::init();
    let res = (|| -> Result<()> {
        execute!(std::io::stdout(), EnableMouseCapture)?;
        loop {
            while let Ok(frame) = rx.try_recv() {
                app.on_event(frame);
            }
            if std::mem::take(&mut app.redraw) {
                term.clear()?;
            }
            term.draw(|f| app.draw(f))?;
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press && !app.key(k.code) => return Ok(()),
                Event::Mouse(m) => app.mouse(m),
                _ => {}
            }
        }
    })();
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

/// `tui.json` in `path`: the metadata columns and whether the sidebar shows (default: shown).
fn load_ui(path: &Path) -> (Meta, bool) {
    let v = std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).unwrap_or_default();
    (serde_json::from_value(v["meta"].clone()).unwrap_or_default(), v["sidebar"].as_bool().unwrap_or(true))
}

/// Writes the whole file, so every save passes both keys.
fn save_ui(path: &Path, meta: Meta, sidebar: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    std::fs::write(path, json!({"meta": meta, "sidebar": sidebar}).to_string())
}

impl App {
    fn new(tx: mpsc::Sender<Value>, root: PathBuf) -> App {
        App {
            tx,
            root,
            local: vec![],
            sim: HashMap::new(),
            runtimes: None,
            physical: vec![],
            device: String::new(),
            pick: 0,
            items: vec![],
            status: json!({}),
            sel: 0,
            view: vec![],
            expanded: HashSet::new(),
            expand_all: false,
            proc_id: None,
            filter: String::new(),
            search: String::new(),
            input: None,
            level: None,
            source: None,
            prompt: None,
            msg: String::new(),
            scroll: 0,
            page: 1,
            hits: vec![],
            logs_area: Rect::default(),
            running_area: Rect::default(),
            tabs_area: Rect::default(),
            dibs_sel: 0,
            meta: Meta::default(),
            sidebar: true,
            tab: Focus::Targets,
            utc_offset: utc_offset(),
            pid: None,
            focus: Focus::Targets,
            cursor: 0,
            history: vec![],
            runs: vec![],
            running: vec![],
            run: None,
            results: HashMap::new(),
            open: HashSet::new(),
            marks: HashSet::new(),
            test_sel: 0,
            report: None,
            report_data: None,
            report_sel: 0,
            logs_for: None,
            problems: Value::Null,
            fetching: false,
            problems_new: false,
            problems_kind: None,
            problems_sel: 0,
            problems_after: None,
            redraw: false,
            repos: Default::default(),
        }
    }
}

/// `it`'s section given its latest process `p`: outside the worktree `root` (resolved) it is `elsewhere`; in it, an agent's (herdr
/// names its agent, or a Claude Code session holds it) is `agents`, except that a local target returns to `yours`
/// once the agent's run ends.
fn section(root: &Path, it: &Item, p: Option<&Value>) -> &'static str {
    let agent = p.is_some_and(|p| p["agent"].is_string() || text(&p["holder"]).starts_with("claude:"));
    let live = p.is_some_and(|p| matches!(text(&p["state"]), "running" | "building" | "queued"));
    if !it.local && !inside(root, &it.dir) {
        "elsewhere"
    } else if agent && (live || !it.local) {
        "agents"
    } else {
        "yours"
    }
}

/// Whether `dir` is in the worktree `root` (resolved).
fn inside(root: &Path, dir: &Path) -> bool {
    // Resolved like `root`: git reports `/private/tmp/…` while a caller's cwd may go through a symlink (`/tmp/…`).
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).starts_with(root)
}

fn on_sim(t: &Value) -> bool {
    t["needs"].as_array().is_some_and(|n| n.iter().any(|n| n["kind"] == "simulator"))
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

pub(crate) fn ago(ms: u64) -> String {
    match ms / 1000 {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h{}m", s / 3600, s / 60 % 60),
    }
}

/// `path` in at most `max` chars, its middle elided: `Sources/…/FeedParser.swift`.
fn elide(path: &str, max: usize) -> String {
    let n = path.chars().count();
    if n <= max || max < 5 {
        return path.into();
    }
    let head: String = path.chars().take((max - 1) / 3).collect();
    let tail: String = path.chars().skip(n - (max - 1 - head.chars().count())).collect();
    format!("{head}…{tail}")
}

/// A test's duration: `12ms`, `1.2s`.
fn secs(ms: u64) -> String {
    if ms < 1000 { format!("{ms}ms") } else { format!("{:.1}s", ms as f64 / 1000.0) }
}

/// The local zone's offset from `date +%z` (`+0100`), 0 when that fails.
// ponytail: read once, so a DST switch mid-session shifts times by an hour until the pane reopens; a tz crate would fix it.
fn utc_offset() -> i64 {
    let out = std::process::Command::new("date").arg("+%z").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let Ok(z) = out else { return 0 };
    let (sign, digits) = if let Some(d) = z.strip_prefix('-') { (-1, d) } else { (1, z.trim_start_matches('+')) };
    match (digits.get(..2).and_then(|h| h.parse::<i64>().ok()), digits.get(2..4).and_then(|m| m.parse::<i64>().ok())) {
        (Some(h), Some(m)) => sign * (h * 3600 + m * 60),
        _ => 0,
    }
}

/// `ms` (Unix milliseconds) as local `HH:MM:SS.mmm`.
fn clock(ms: u64, offset: i64) -> String {
    let day_ms = (ms as i64 + offset * 1000).rem_euclid(86_400_000);
    format!("{:02}:{:02}:{:02}.{:03}", day_ms / 3_600_000, day_ms / 60_000 % 60, day_ms / 1000 % 60, day_ms % 1000)
}

fn level_style(l: Level) -> Style {
    match l {
        Level::Error => Style::new().red(),
        Level::Warn => Style::new().yellow(),
        Level::Debug => Style::new().dark_gray(),
        Level::Info => Style::new(),
    }
}

fn line_text(l: &Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// `s` without U+FE0F (emoji presentation), so `ℹ️` shows as its one-cell text form `ℹ`. Terminal layers disagree on
/// that sequence's width; under herdr in Ghostty the rest of the row drew one cell right of where ratatui put it,
/// leaving its last character in the border.
fn text_form(s: &str) -> String {
    s.replace('\u{fe0f}', "")
}

/// Splits `line` into rows at most `width` cells wide, keeping each span's style; a wide glyph that doesn't fit
/// starts the next row.
fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width {
        return vec![line];
    }
    let mut out = vec![Line::default().style(line.style)];
    let mut used = 0;
    for span in line.spans {
        let mut chunk = String::new();
        for c in span.content.chars() {
            let w = Span::raw(&*c.encode_utf8(&mut [0; 4])).width();
            if used > 0 && used + w > width {
                out.last_mut().unwrap().spans.push(Span::styled(std::mem::take(&mut chunk), span.style));
                out.push(Line::default().style(line.style));
                used = 0;
            }
            chunk.push(c);
            used += w;
        }
        out.last_mut().unwrap().spans.push(Span::styled(chunk, span.style));
    }
    out
}

impl App {
    fn selected(&self) -> Option<&Item> {
        self.items.get(self.sel)
    }

    /// Whether `it` is a target that only builds (kind `build`: an Xcode framework's scheme), so `r` builds it.
    fn only_builds(&self, it: &Item) -> bool {
        self.local.iter().any(|t| t.name == it.name && t.dir == it.dir && t.kind == "build")
    }

    fn proc_of(&self, it: &Item) -> Option<&Value> {
        let procs = self.status["processes"].as_array()?;
        procs.iter().rev().find(|p| text(&p["target"]) == it.name && text(&p["dir"]) == it.dir.to_string_lossy())
    }

    /// The selected item and its simulator choice, when it is a local target that runs on a simulator.
    fn sim_choice(&self) -> Option<(Item, Value)> {
        let it = self.current().filter(|i| i.local)?;
        let sim = self.sim.get(&it.name)?.clone();
        Some((it, sim))
    }

    fn runtime(&self, id: &Value) -> Option<&Value> {
        self.runtimes.as_ref()?.iter().find(|r| &r["id"] == id)
    }

    /// Devices of the chosen runtime whose name contains the typed text, in simctl's order.
    fn matches(&self) -> Vec<Value> {
        let Some((_, cur)) = self.sim_choice() else { return vec![] };
        let needle = self.device.to_lowercase();
        let devices = self.runtime(&cur["runtime"]).and_then(|r| r["devices"].as_array()).cloned().unwrap_or_default();
        devices.into_iter().filter(|d| text(&d["name"]).to_lowercase().contains(&needle)).collect()
    }

    /// The open list's row labels, and the row holding the current value.
    fn list(&self) -> (Vec<String>, Option<usize>) {
        let Some((_, cur)) = self.sim_choice() else { return (vec![], None) };
        let row = |v: &Value| (text(&v["name"]).to_string(), v["id"].clone());
        let ((labels, ids), now): ((Vec<_>, Vec<_>), _) = match self.input {
            Some(Input::Runtime) => {
                let mut rows: (Vec<_>, Vec<_>) = self.runtimes.iter().flatten().map(row).unzip();
                for d in &self.physical {
                    rows.0.push(format!("{} · {} · {}", text(&d["name"]), text(&d["os"]), text(&d["state"])));
                    rows.1.push(d["udid"].clone());
                }
                (rows, if cur["physical"].is_object() { &cur["physical"]["udid"] } else { &cur["runtime"] })
            }
            Some(Input::Device) => (self.matches().iter().map(row).unzip(), &cur["device"]),
            _ => return (vec![], None),
        };
        let at = ids.iter().position(|id| id == now);
        (labels, at)
    }

    /// The physical device on row `i` of the open destination list, if that row is one.
    fn physical_row(&self, i: usize) -> Option<&Value> {
        let rts = self.runtimes.as_ref().map_or(0, Vec::len);
        self.physical.get(i.checked_sub(rts)?).filter(|_| self.input == Some(Input::Runtime))
    }

    fn list_open(&self) -> bool {
        matches!(self.input, Some(Input::Runtime | Input::Device))
    }

    /// Stores the choice in the daemon; it applies to the next run.
    fn set_sim(&mut self, it: &Item, params: Value, note: String) {
        match self.req("sim.set", it, params.clone()) {
            Ok(v) => {
                let s = v["sim"].clone();
                let running = self.proc_of(it).is_some_and(|p| matches!(text(&p["state"]), "running" | "building" | "queued"));
                let later = if running { " — applies on next run (r to restart)" } else { "" };
                let head = match (params["head"].as_bool(), params["slim"].as_str()) {
                    (Some(on), _) => format!(" · head {}", if on { "on" } else { "off" }),
                    (_, Some(slim)) => format!(" · slim {slim}"),
                    _ => String::new(),
                };
                let ph = &s["physical"];
                let to = match ph.is_object() {
                    true => format!("{} · {} · {}", text(&ph["name"]), text(&ph["model"]), text(&ph["os"])),
                    false => format!("{} · {}", text(&s["device_name"]), text(&s["runtime_name"])),
                };
                self.msg = format!("{note}{to}{head}{later}");
                self.sim.insert(it.name.clone(), s);
            }
            Err(e) => self.msg = format!("{e:#}"),
        }
    }

    /// This worktree's targets, keeping the selection on the same row. Also rebuilds `running` and `runs`.
    fn rebuild_items(&mut self) {
        let keep = self.selected().map(|i| (i.name.clone(), i.dir.clone()));
        // Build-only targets (an Xcode workspace's framework schemes, dozens) last.
        let mut local: Vec<&Target> = self.local.iter().collect();
        local.sort_by_key(|t| t.kind == "build");
        self.items = local.into_iter().map(|t| Item { name: t.name.clone(), dir: t.dir.clone(), local: true }).collect();
        self.rebuild_running();
        self.rebuild_runs();
        if self.pinned().is_none() {
            self.run = None;
        }
        if let Some(i) = keep.and_then(|(n, d)| self.items.iter().position(|i| i.name == n && i.dir == d)) {
            self.sel = i;
        }
        self.sel = self.sel.min(self.items.len().saturating_sub(1));
    }

    /// Every live process (`ended_at` null: an ended `collie exec` command stays in `status`), commands included,
    /// from anywhere, grouped by `SECTIONS`.
    fn rebuild_running(&mut self) {
        let live = self.status["processes"].as_array().into_iter().flatten().filter(|p| p["ended_at"].is_null());
        let mut running: Vec<(&'static str, Value)> = live
            .map(|p| {
                let it = Item { name: String::new(), dir: text(&p["dir"]).into(), local: false };
                (self.place(&it.dir, section(&self.root, &it, Some(p))), p.clone())
            })
            .collect();
        running.sort_by_key(|(s, _)| SECTIONS.iter().position(|x| x == s));
        self.running = running;
        if self.focus == Focus::Running && self.live_row().is_none() {
            self.focus = self.tab;
        }
    }

    /// Every ended run, this session's (still in `status`) or recorded (`history`): this worktree's, the repo's other
    /// worktrees', then other projects'; newest first in each.
    fn rebuild_runs(&mut self) {
        let ended: Vec<Value> = self.status["processes"].as_array().into_iter().flatten().filter(|p| !p["ended_at"].is_null()).cloned().collect();
        let ids: HashSet<String> = ended.iter().map(|p| text(&p["id"]).to_string()).collect();
        let mut runs: Vec<(usize, Value)> = ended
            .into_iter()
            .chain(self.history.iter().filter(|r| !ids.contains(text(&r["id"]))).cloned())
            .map(|r| (SECTIONS.iter().position(|s| *s == self.run_place(&r)).unwrap_or(0), r))
            .collect();
        runs.sort_by_key(|(group, r)| (*group, std::cmp::Reverse(r["since"].as_u64().unwrap_or_default())));
        self.runs = runs.into_iter().map(|(_, r)| r).collect();
    }

    /// A history row's section: `yours` in this worktree, else where `place` puts it.
    fn run_place(&self, r: &Value) -> &'static str {
        let dir = Path::new(text(&r["dir"]));
        self.place(dir, if inside(&self.root, dir) { "yours" } else { "elsewhere" })
    }

    /// The process picked in the running block or the history, while one is. Looked up in `status` too, so it stays
    /// shown when it ends.
    fn pinned(&self) -> Option<&Value> {
        let id = self.run.as_ref()?;
        self.status["processes"].as_array().into_iter().flatten().chain(&self.runs).find(|r| text(&r["id"]) == id)
    }

    /// The process on the right: the pinned one, else the selected target's latest.
    fn shown(&self) -> Option<&Value> {
        self.pinned().or_else(|| self.selected().and_then(|i| self.proc_of(i)))
    }

    /// Whether the shown run is a `collie exec` command, which can't be rerun by name.
    fn shown_exec(&self) -> bool {
        self.shown().is_some_and(|p| p["kind"] == "exec")
    }

    /// Whether the log area shows the report: for a test run, `t`'s choice, else whether it ended with failures.
    fn report_on(&self) -> bool {
        let Some(p) = self.shown().filter(|p| p["kind"] == "test") else { return false };
        if self.logs_for.as_deref() == p["id"].as_str() {
            return false;
        }
        self.report.unwrap_or(!p["ended_at"].is_null() && p["tests"]["failed"].as_u64().unwrap_or_default() > 0)
    }

    /// The shown run's failed tests, from `report_data`.
    fn report_failures(&self) -> Vec<Value> {
        let Some((id, v)) = &self.report_data else { return vec![] };
        let tests = v["tests"].as_array().into_iter().flatten().filter(|_| self.proc_id.as_ref() == Some(id));
        tests.filter(|t| t["status"] == "failed").cloned().collect()
    }

    /// Fetches the shown process's results once it is an ended test run.
    fn load_report(&mut self) {
        let Some(id) = self.proc_id.clone() else { return };
        let ended_test = self.shown().is_some_and(|p| p["kind"] == "test" && !p["ended_at"].is_null());
        if !ended_test || self.report_data.as_ref().is_some_and(|(r, _)| *r == id) {
            return;
        }
        let v = request("tests", json!({"process": id})).unwrap_or_default();
        (self.report_data, self.report_sel) = (Some((id, v)), 0);
    }

    /// Refetches a local test target's results once a newer run of it has ended, unfolding the groups that now
    /// hold a failure.
    fn refresh_tests(&mut self) {
        for t in self.local.iter().filter(|t| t.kind == "test") {
            let dir = t.dir.to_string_lossy();
            let Some(newest) = self.runs.iter().find(|r| text(&r["target"]) == t.name && text(&r["dir"]) == dir).map(|r| text(&r["id"]).to_string()) else {
                continue;
            };
            if self.results.get(&t.name).is_some_and(|(id, _)| *id == newest) {
                continue;
            }
            let v = request("tests", json!({"target": t.name, "cwd": dir})).unwrap_or_default();
            self.open.extend(failing_groups(&t.name, &v));
            self.results.insert(t.name.clone(), (newest, v));
        }
    }

    /// What `run`'s `only` takes for a test's or group's row: the test's id, or the group's path and `sep` after it
    /// (`tests::`, `SumTests/Outer/`). None for a target's row.
    fn row_id(&self, row: &TreeRow) -> Option<String> {
        if let Some(t) = &row.item {
            return t["id"].as_str().map(String::from);
        }
        let (target, path) = row.key.split_once('\t')?;
        let sep = self.results.get(target).and_then(|(_, v)| v["sep"].as_str()).unwrap_or("/");
        Some(format!("{path}{sep}"))
    }

    /// The marked tests and groups of the target shown on the right.
    fn marked(&self) -> Vec<String> {
        let Some(it) = self.current() else { return vec![] };
        self.marks.iter().filter_map(|m| m.strip_prefix(&format!("{}\t", it.name))).map(String::from).collect()
    }

    /// The shown run's failed tests' ids, for `F`.
    fn failed_ids(&self) -> Vec<String> {
        self.report_failures().iter().filter_map(|t| t["id"].as_str().map(String::from)).collect()
    }

    /// The tests tab's rows: each local test target's `tree`.
    fn tree_rows(&self) -> Vec<TreeRow> {
        let null = Value::Null;
        let targets = self.local.iter().filter(|t| t.kind == "test");
        targets.flat_map(|t| tree(&t.name, self.results.get(&t.name).map_or(&null, |(_, v)| v), &self.open)).collect()
    }

    /// Asks for the problems again once a target of this worktree has ended a run since they came.
    fn refresh_problems(&mut self) {
        let here = |r: &&Value| r["kind"] != "exec" && inside(&self.root, Path::new(text(&r["dir"])));
        let newest = self.runs.iter().filter(here).max_by_key(|r| r["ended_at"].as_u64()).map(|r| text(&r["id"]).to_string());
        // While a fetch is out this stays stale, and the reply checks again.
        if newest != self.problems_after && !self.fetching {
            self.problems_after = newest;
            // A build is a fair moment to scan the TODOs again too.
            self.fetch_problems(true);
        }
    }

    /// The problems tab's rows, as its view shows them (`problem_rows`).
    fn problem_rows(&self) -> Vec<TreeRow> {
        let items = self.problems["items"].as_array().map_or(&[][..], Vec::as_slice);
        problem_rows(items, self.problems_new, self.problems_kind, &self.open)
    }

    /// Asks the daemon for this worktree's problems, in the background: a first TODO scan of a big repo takes a while.
    fn fetch_problems(&mut self, rescan: bool) {
        if self.fetching {
            return;
        }
        self.fetching = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(match request_with("problems", json!({"rescan": rescan}), false) {
                Ok(v) => json!({"event": "problems", "data": v}),
                Err(e) => json!({"event": "problems", "error": format!("problems: {e:#}")}),
            });
        });
    }

    /// Pins process `id` and, given a `cursor`, loads its log from there, that record on top: the view holds only the
    /// last records. Err when the log is gone (pruned).
    fn show_record(&mut self, id: String, cursor: Option<&str>) -> Result<()> {
        if self.proc_id.as_ref() != Some(&id) {
            self.run = Some(id);
            self.sync();
        }
        let Some(c) = cursor else { return Ok(()) };
        let v = request("logs", json!({"since": c}))?;
        let recs = serde_json::from_value::<Vec<Record>>(v["records"].clone()).unwrap_or_default();
        self.view = recs.into_iter().take(VIEW_CAP).map(Some).collect();
        self.expanded.clear();
        (self.scroll, self.cursor) = (usize::MAX, usize::MAX);
        Ok(())
    }

    /// Opens a problem's file at its line in `$EDITOR`: in a herdr split to the right when in herdr, else in this
    /// terminal while the pane waits; without `$EDITOR`, with `open`.
    // ponytail: `+<line>` is understood by vim, nvim, nano and emacs, not VS Code or helix; a per-editor table if needed.
    fn edit(&mut self, path: &str, line: u64) {
        let root = PathBuf::from(text(&self.problems["root"]));
        let Ok(editor) = std::env::var("EDITOR") else {
            let _ = std::process::Command::new("open").arg(root.join(path)).spawn();
            self.msg = format!("{path}:{line}");
            return;
        };
        if std::env::var_os("HERDR_PANE_ID").is_some() {
            let split = std::process::Command::new("herdr").args(["pane", "split", "--current", "--direction", "right", "--cwd"]).arg(&root).arg("--focus").output();
            let pane = split.ok().filter(|o| o.status.success()).and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok());
            if let Some(id) = pane.as_ref().and_then(|v| v["result"]["pane"]["pane_id"].as_str()) {
                // `exec`, so the pane ends with the editor.
                let cmd = format!("exec $EDITOR +{line} '{}'", path.replace('\'', r"'\''"));
                if std::process::Command::new("herdr").args(["pane", "run", id, &cmd]).status().is_ok_and(|s| s.success()) {
                    return;
                }
            }
        }
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        ratatui::restore();
        let ran = std::process::Command::new("sh").args(["-c", r#"$EDITOR +"$1" "$2""#, "sh", &line.to_string(), path]).current_dir(&root).status();
        let _ = ratatui::init();
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
        self.redraw = true;
        if let Err(e) = ran {
            self.msg = format!("{editor}: {e}");
        }
    }

    /// The pinned run, as an item (this worktree's target when it is one's), so actions apply to it; else the selected
    /// target.
    fn current(&self) -> Option<Item> {
        let Some(r) = self.pinned() else { return self.selected().cloned() };
        let it = Item { name: text(&r["target"]).into(), dir: text(&r["dir"]).into(), local: false };
        Some(self.items.iter().find(|i| i.name == it.name && i.dir == it.dir).cloned().unwrap_or(it))
    }

    /// `section` as `section()` gives it, with `elsewhere` split into the repo's other worktrees and other projects.
    fn place(&self, dir: &Path, section: &'static str) -> &'static str {
        match section {
            "elsewhere" if self.repo(dir).is_some_and(|r| self.repo(&self.root) == Some(r)) => "other worktrees",
            "elsewhere" => "other projects",
            s => s,
        }
    }

    /// The git dir that `dir`'s repo shares across its worktrees; None outside git (or gone). Asked once per dir.
    fn repo(&self, dir: &Path) -> Option<PathBuf> {
        let mut repos = self.repos.borrow_mut();
        let found = repos.entry(dir.to_path_buf()).or_insert_with(|| {
            let out = std::process::Command::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).current_dir(dir).output().ok()?;
            out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
        });
        found.clone()
    }

    /// Requests about `it` run in its own directory, so processes elsewhere resolve like local ones.
    fn req(&self, method: &str, it: &Item, mut params: Value) -> Result<Value> {
        params["target"] = it.name.clone().into();
        params["cwd"] = it.dir.to_string_lossy().into();
        request(method, params)
    }

    /// Reloads the stream when the shown process changed, and its results once it is an ended test run.
    fn sync(&mut self) {
        let p = self.shown();
        let (id, pid) = (p.map(|p| text(&p["id"]).to_string()), p.and_then(|p| p["pid"].as_u64()));
        if id == self.proc_id {
            self.pid = pid.or(self.pid);
            return self.load_report();
        }
        self.proc_id = id;
        self.pid = pid;
        self.view.clear();
        self.expanded.clear();
        self.scroll = 0;
        self.cursor = 0;
        let Some(id) = self.proc_id.clone() else { return };
        match request("logs", json!({"process": id})) {
            Ok(v) => self.view = serde_json::from_value::<Vec<Record>>(v["records"].clone()).unwrap_or_default().into_iter().map(Some).collect(),
            Err(e) => {
                self.msg = format!("{e:#}");
                // Unloaded, so the next status retries instead of waiting on the process's next line.
                self.proc_id = None;
            }
        }
        self.load_report();
    }

    fn on_event(&mut self, frame: Value) {
        match frame["event"].as_str() {
            Some("status") => {
                let before = self.selected().map(|i| (i.name.clone(), i.dir.clone()));
                let ids = |s: &Value| s["processes"].as_array().into_iter().flatten().map(|p| text(&p["id"]).to_string()).collect::<HashSet<_>>();
                // A stop or a rerun drops processes from `status`, ended ones too; the daemon has recorded them by then.
                let dropped = !ids(&self.status).is_subset(&ids(&frame["data"]));
                self.status = frame["data"].clone();
                if dropped && let Ok(v) = request_with("history", json!({}), false) {
                    self.history = v["runs"].as_array().cloned().unwrap_or_default();
                }
                self.rebuild_items();
                self.refresh_tests();
                self.refresh_problems();
                if self.list_open() && self.selected().map(|i| (i.name.clone(), i.dir.clone())) != before {
                    self.input = None;
                }
                // `subscribe` skipped log events to catch up: reload the tail, keeping the place unless following.
                if frame["lagged"] == true {
                    let kept = (self.scroll, self.cursor);
                    self.proc_id = None;
                    self.sync();
                    if kept.0 > 0 {
                        (self.scroll, self.cursor) = kept;
                    }
                }
                self.sync();
            }
            Some("targets") => {
                let ts = frame["data"].as_array().cloned().unwrap_or_default();
                self.local = ts.iter().filter_map(|t| serde_json::from_value(t.clone()).ok()).collect();
                self.sim = ts.iter().filter(|t| on_sim(t)).map(|t| (text(&t["name"]).into(), t["sim"].clone())).collect();
                // Here rather than at startup: simulator targets may first arrive after a reconnect (a fixed collie.toml).
                if self.runtimes.is_none() && !self.sim.is_empty() {
                    let tx = self.tx.clone();
                    let sims = move || match request_with("sims", json!({}), false) {
                        Ok(v) => tx.send(json!({"event": "sims", "data": v})),
                        Err(e) => tx.send(json!({"event": "sims", "error": format!("simulators: {e:#}")})),
                    };
                    std::thread::spawn(sims);
                }
                self.msg = text(&frame["msg"]).into();
                // With `targets` because both come on (re)connect; runs ending later arrive in `status`.
                if let Ok(v) = request_with("history", json!({}), false) {
                    self.history = v["runs"].as_array().cloned().unwrap_or_default();
                }
                self.rebuild_items();
                self.refresh_tests();
                // For the tab's count; on (re)connect, like the history.
                self.fetch_problems(false);
                self.sync();
            }
            Some("gone") => self.msg = "daemon gone — reconnecting…".into(),
            Some("problems") => {
                self.fetching = false;
                match frame["error"].as_str() {
                    Some(e) => self.msg = e.replace('\n', " "),
                    None => self.problems = frame["data"].clone(),
                }
                self.refresh_problems();
            }
            // `runtimes` stays None, so the next `targets` event (a reconnect) asks again.
            Some("sims") if frame["error"].is_string() => self.msg = text(&frame["error"]).replace('\n', " "),
            Some("sims") => {
                self.runtimes = Some(frame["data"]["runtimes"].as_array().cloned().unwrap_or_default());
                self.physical = frame["data"]["physical"].as_array().cloned().unwrap_or_default();
            }
            Some("log") if self.proc_id.as_deref().is_some_and(|id| text(&frame["data"]["process"]) == id) => {
                if let Ok(r) = serde_json::from_value::<Record>(frame["data"].clone()) {
                    // `scroll` and `cursor` count rows from the bottom, so while scrolled up, or with the cursor off
                    // the last line, they grow by the rows this record adds (as `rows()` draws it; 0 when filtered
                    // out) to keep the visible lines and the cursor's line still.
                    if self.scroll > 0 || self.focus == Focus::Logs && self.cursor > 0 {
                        let (filter, needle) = self.parsed_filter();
                        if self.keeps(&r, &filter, &needle) {
                            let added = self.record_rows(self.view.len(), &r, self.logs_width()).len();
                            self.scroll += added;
                            self.cursor += added;
                        }
                    }
                    self.view.push(Some(r));
                    let excess = self.view.len().saturating_sub(VIEW_CAP);
                    if excess > 0 {
                        self.view.drain(..excess);
                        self.expanded.clear();
                    }
                }
            }
            _ => {}
        }
    }

    fn act(&mut self, a: Act) {
        let item = self.current();
        match (a, item) {
            (Act::Past(i), _) => {
                let Some(id) = self.runs.get(i).map(|r| text(&r["id"]).to_string()) else { return };
                (self.focus, self.tab) = (Focus::History, Focus::History);
                self.run = Some(id);
                self.sync();
            }
            (Act::Live(i), _) => {
                let Some(id) = self.running.get(i).map(|(_, p)| text(&p["id"]).to_string()) else { return };
                self.focus = Focus::Running;
                self.run = Some(id);
                self.sync();
            }
            (Act::Select(i), _) => {
                (self.focus, self.tab) = (Focus::Targets, Focus::Targets);
                self.run = None;
                self.sel = i;
                self.sync();
            }
            (Act::Tab(t), _) => {
                if !self.sidebar {
                    self.act(Act::Sidebar);
                }
                (self.focus, self.tab) = (t, t);
                if t == Focus::Problems && self.problems.is_null() {
                    self.fetch_problems(false);
                }
            }
            (Act::Sidebar, _) => {
                self.sidebar = !self.sidebar;
                if !self.sidebar {
                    self.focus_on(Focus::Logs);
                }
                if let Err(e) = save_ui(&state_dir().join("tui.json"), self.meta, self.sidebar) {
                    self.msg = format!("couldn't save the sidebar choice: {e}");
                }
            }
            (Act::Test(i), _) => {
                let rows = self.tree_rows();
                let Some(row) = rows.get(i) else { return };
                let name = row.key.split('\t').next().unwrap_or_default();
                if let Some(t) = self.items.iter().position(|it| it.name == name) {
                    self.sel = t;
                }
                (self.test_sel, self.focus, self.tab, self.run) = (i, Focus::Tests, Focus::Tests, None);
                self.sync();
            }
            (Act::Check(i), _) => {
                self.act(Act::Test(i));
                let rows = self.tree_rows();
                let Some((row, id)) = rows.get(i).and_then(|r| Some((r, self.row_id(r)?))) else { return };
                let target = format!("{}\t", row.key.split('\t').next().unwrap_or_default());
                self.marks.retain(|m| m.starts_with(&target));
                let key = format!("{target}{id}");
                if !self.marks.remove(&key) {
                    self.marks.insert(key);
                }
            }
            (Act::View(on), _) => (self.report, self.logs_for) = (Some(on), None),
            (Act::Failure(id, cursor), _) => {
                self.logs_for = Some(id.clone());
                if let Err(e) = self.show_record(id, cursor.as_deref()) {
                    self.msg = format!("{e:#}");
                }
                self.focus = Focus::Logs;
                if cursor.is_none() {
                    self.level = Some(Level::Error);
                }
            }
            (Act::Problem(i), _) => {
                (self.problems_sel, self.focus, self.tab) = (i, Focus::Problems, Focus::Problems);
                let rows = self.problem_rows();
                let source = rows.get(i).and_then(|r| r.item.as_ref()).map(|p| p["sources"][0].clone()).filter(|s| s.is_object());
                if let Some(s) = source {
                    // Its record, also from a failing test run, which would show its report instead.
                    self.logs_for = Some(text(&s["process"]).into());
                    if self.show_record(text(&s["process"]).into(), s["cursor"].as_str()).is_err() {
                        self.msg = "log no longer kept".into();
                    }
                }
            }
            (Act::ProblemsNew(on), _) => (self.problems_new, self.problems_sel) = (on, 0),
            (Act::ProblemsKind(k), _) => (self.problems_kind, self.problems_sel) = (if self.problems_kind == Some(k) { None } else { Some(k) }, 0),
            (Act::Run(_) | Act::Take(_), Some(it)) if self.shown_exec() => {
                self.msg = format!("{} is a `collie exec` command: rerun it with collie exec", it.name)
            }
            (a @ (Act::Run(_) | Act::Take(_)), Some(it)) => {
                self.prompt = None;
                let env: HashMap<_, _> = std::env::vars().collect();
                let take = matches!(a, Act::Take(_));
                let (Act::Run(only) | Act::Take(only)) = a else { return };
                let what = if only.is_empty() { String::new() } else { format!(" ({} of its tests)", only.len()) };
                let marked = self.marked();
                match self.req("run", &it, json!({"env": env, "take": take, "only": only})) {
                    Ok(_) => {
                        if !marked.is_empty() && marked.len() == only.len() && marked.iter().all(|m| only.contains(m)) {
                            self.marks.clear();
                        }
                        self.msg = format!("started {}{what}", it.name)
                    }
                    Err(e) if e.downcast_ref::<Coded>().is_some_and(|c| c.0 == "held") => {
                        let by = self.proc_of(&it).map_or(String::new(), |p| who(text(&p["holder"]), &p["agent"].as_str().map(Into::into)));
                        self.prompt = Some(Prompt::Take(it, by, only));
                    }
                    Err(e) => self.msg = format!("{e:#}"),
                }
            }
            // `stop` goes by name, so it would hit the target's live run instead.
            (Act::Stop, Some(it)) if self.pinned().is_some_and(|p| !p["ended_at"].is_null()) => {
                self.msg = format!("this run of {} has ended", it.name)
            }
            (Act::Stop, Some(it)) => {
                self.msg = match self.req("stop", &it, json!({})) {
                    Ok(_) => format!("stopped {}", it.name),
                    Err(e) => format!("{e:#}"),
                }
            }
            (Act::Open, Some(it)) => {
                let p = self.proc_of(&it);
                let port = p.and_then(|p| p["port"].as_u64());
                let live = p.is_some_and(|p| matches!(text(&p["state"]), "running" | "building" | "queued"));
                let sim = p.filter(|_| live).and_then(|p| Some((p["simulator"]["udid"].as_str()?.to_string(), text(&p["simulator"]["name"]).to_string())));
                self.msg = match (port, sim) {
                    (Some(port), _) => {
                        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                        let url = format!("http://localhost:{port}");
                        match std::process::Command::new(opener).arg(&url).stderr(std::process::Stdio::null()).spawn() {
                            Ok(_) => format!("opened {url}"),
                            Err(e) => format!("{opener}: {e}"),
                        }
                    }
                    (None, Some((udid, name))) => {
                        let url = format!("devices://device/open?id={udid}");
                        match std::process::Command::new("open").arg(&url).stderr(std::process::Stdio::null()).spawn() {
                            Ok(_) => format!("opened DeviceHub on {name}"),
                            Err(e) => format!("open: {e}"),
                        }
                    }
                    (None, None) => format!("{} has no port or running simulator", it.name),
                }
            }
            (Act::Mark, _) => self.view.push(None),
            (Act::Level(l), _) => self.level = l,
            (Act::Source(s), _) => self.source = s,
            (Act::Lease(id), _) => {
                if let Some(i) = self.status["leases"].as_array().into_iter().flatten().position(|l| l["id"] == id) {
                    (self.dibs_sel, self.focus) = (i, Focus::Dibs);
                }
                self.prompt = Some(Prompt::Lease(id));
            }
            (Act::Release(id), _) => {
                self.prompt = None;
                self.msg = match request("lease.release", json!({"id": id})) {
                    Ok(_) => format!("released dibs {id}"),
                    Err(e) => format!("{e:#}"),
                }
            }
            (Act::Renew(id), _) => {
                self.prompt = None;
                self.msg = match request("lease.renew", json!({"id": id})) {
                    Ok(_) => format!("renewed dibs {id} for 2h"),
                    Err(e) => format!("{e:#}"),
                }
            }
            (Act::Cancel, _) => {
                self.prompt = None;
                self.input = None;
            }
            (Act::Toggle(i), _) => {
                if !self.expanded.remove(&i) {
                    self.expanded.insert(i);
                }
            }
            // The destination list still opens with physical devices when no runtime is installed.
            (Act::Edit(i @ (Input::Runtime | Input::Device)), _)
                if self.runtimes.as_ref().is_none_or(|r| r.is_empty() && (i == Input::Device || self.physical.is_empty())) =>
            {
                self.msg = if self.runtimes.is_none() { "loading simulators…" } else { "no iOS runtimes installed" }.into()
            }
            (Act::Edit(i), _) => {
                self.input = Some(i);
                self.device.clear();
                self.pick = self.list().1.unwrap_or(0);
            }
            (Act::Pick(i), _) => match (self.input, self.sim_choice()) {
                (Some(Input::Runtime), Some((it, _))) if let Some(d) = self.physical_row(i).cloned() => {
                    if d["state"] == "unavailable" {
                        self.msg = format!("{} is unavailable: unlock it and bring it onto this Mac's network, or connect it by cable", text(&d["name"]));
                        return;
                    }
                    self.input = None;
                    self.set_sim(&it, json!({"physical": d["udid"]}), String::new());
                }
                (Some(Input::Runtime), Some(_)) => {
                    let Some(id) = self.runtimes.iter().flatten().nth(i).map(|r| text(&r["id"]).to_string()) else { return };
                    self.input = None;
                    self.act(Act::Runtime(id));
                }
                (Some(Input::Device), Some((it, cur))) => match self.matches().get(i) {
                    Some(d) => {
                        self.input = None;
                        self.set_sim(&it, json!({"device": d["id"], "runtime": cur["runtime"]}), String::new());
                    }
                    None => self.msg = format!("no device matching \"{}\" on {}", self.device, text(&cur["runtime_name"])),
                },
                _ => self.input = None,
            },
            (Act::Runtime(id), _) => {
                let (Some((it, cur)), Some(rt)) = (self.sim_choice(), self.runtime(&json!(id)).cloned()) else { return };
                let devices = rt["devices"].as_array().cloned().unwrap_or_default();
                if devices.iter().any(|d| d["id"] == cur["device"]) {
                    return self.set_sim(&it, json!({"device": cur["device"], "runtime": rt["id"]}), String::new());
                }
                // The runtime's first iPhone stands in for a device it doesn't support.
                let Some(dev) = devices.iter().find(|d| text(&d["name"]).starts_with("iPhone")).or(devices.first()) else { return };
                let note = format!("{} isn't on {} — using ", text(&cur["device_name"]), text(&rt["name"]));
                self.set_sim(&it, json!({"device": dev["id"], "runtime": rt["id"]}), note);
            }
            (Act::Head(on), _) => {
                if let Some((it, _)) = self.sim_choice() {
                    self.set_sim(&it, json!({"head": on}), String::new());
                }
            }
            // Already on keeps its except list, which only the CLI edits.
            (Act::Slim(on), _) => {
                if let Some((it, _)) = self.sim_choice().filter(|(_, c)| c["slim"].is_null() == on) {
                    self.set_sim(&it, json!({"slim": if on { "on" } else { "off" }}), String::new());
                }
            }
            (Act::Meta(i), _) => {
                self.pick = i;
                let on = self.meta.get(i);
                *on = !*on;
                if let Err(e) = save_ui(&state_dir().join("tui.json"), self.meta, self.sidebar) {
                    self.msg = format!("couldn't save the metadata choice: {e}");
                }
            }
            (Act::Cursor(n), _) => {
                self.focus = Focus::Logs;
                self.cursor = n;
                self.park_cursor();
            }
            (Act::Run(_) | Act::Take(_) | Act::Stop | Act::Open, None) => {}
        }
    }

    /// Scrolls the least that brings the log cursor's record into view: its first row, and all of it when it fits.
    fn show_cursor(&mut self) {
        let entries: Vec<usize> = self.rows(self.logs_width()).iter().map(|(_, i)| *i).collect();
        let Some(at) = entries.len().checked_sub(self.cursor + 1) else { return };
        let (first, last) = record_span(&entries, at);
        let len = entries.len();
        self.scroll = reveal(self.scroll, self.page.max(1), len - 1 - first, len - 1 - last);
    }

    /// Moves the log cursor to the next (`down`) or previous record. It rests on the record's last row, so the
    /// newest record is row 0 and keeps following new output.
    fn step_record(&mut self, down: bool) {
        let entries: Vec<usize> = self.rows(self.logs_width()).iter().map(|(_, i)| *i).collect();
        let Some(at) = entries.len().checked_sub(self.cursor + 1) else { return };
        let (first, last) = record_span(&entries, at);
        let to = match down {
            true if last + 1 < entries.len() => record_span(&entries, last + 1).1,
            false if first > 0 => record_span(&entries, first - 1).1,
            _ => last,
        };
        self.cursor = entries.len() - 1 - to;
        self.show_cursor();
    }

    /// Moves the log cursor to its record's last row, as `step_record` leaves it.
    fn park_cursor(&mut self) {
        let entries: Vec<usize> = self.rows(self.logs_width()).iter().map(|(_, i)| *i).collect();
        if let Some(at) = entries.len().checked_sub(self.cursor + 1) {
            self.cursor = entries.len() - 1 - record_span(&entries, at).1;
        }
    }

    /// Expands or collapses the record under the log cursor, keeping the cursor on it.
    fn toggle_at_cursor(&mut self) {
        let rows = self.rows(self.logs_width());
        let Some(&(_, i)) = rows.len().checked_sub(self.cursor + 1).and_then(|j| rows.get(j)) else { return };
        if !self.view[i].as_ref().is_some_and(|r| r.line.contains('\n')) {
            return;
        }
        self.act(Act::Toggle(i));
        let rows = self.rows(self.logs_width());
        if let Some(last) = rows.iter().rposition(|(_, j)| *j == i) {
            self.cursor = rows.len() - 1 - last;
            self.show_cursor();
        }
    }

    /// The pinned run's row in the history.
    fn run_row(&self) -> Option<usize> {
        self.run.as_ref().and_then(|id| self.runs.iter().position(|r| text(&r["id"]) == id))
    }

    /// The pinned process's row in the running block.
    fn live_row(&self) -> Option<usize> {
        self.run.as_ref().and_then(|id| self.running.iter().position(|(_, p)| text(&p["id"]) == id))
    }

    /// The dibs tab's selectable rows: leases, not queue waiters.
    fn leases(&self) -> usize {
        self.status["leases"].as_array().map_or(0, Vec::len)
    }

    /// The logs keep what shows; the targets tab shows its selection's latest run; the running block and the history
    /// pin their first row unless one is.
    fn focus_on(&mut self, f: Focus) {
        match f {
            Focus::Logs => {
                self.focus = f;
                self.cursor = self.scroll;
            }
            Focus::Running => self.act(Act::Live(self.live_row().unwrap_or(0))),
            Focus::Targets => self.act(Act::Select(self.sel)),
            Focus::History => self.act(Act::Past(self.run_row().unwrap_or(0))),
            Focus::Tests | Focus::Dibs | Focus::Problems => self.focus = f,
        }
    }

    /// Tab order, as the sidebar stacks them: the open tab → running → logs, skipping an empty list.
    fn cycle_focus(&mut self, back: bool) {
        let order = [self.tab, Focus::Running, Focus::Logs];
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        for k in 1..order.len() {
            let f = order[if back { (at + order.len() - k) % order.len() } else { (at + k) % order.len() }];
            let empty = match f {
                Focus::Running => self.running.is_empty(),
                Focus::Targets => self.items.is_empty(),
                Focus::History => self.runs.is_empty(),
                Focus::Dibs => self.leases() == 0,
                Focus::Tests | Focus::Problems | Focus::Logs => false,
            };
            if !empty {
                return self.focus_on(f);
            }
        }
    }

    /// Moves one row within the focused list; to the first row of the running block or the history while the pin is
    /// elsewhere.
    fn step(&mut self, down: bool) {
        let to = |at: Option<usize>, len: usize| match at {
            None => 0,
            Some(at) if down => (at + 1).min(len.saturating_sub(1)),
            Some(at) => at.saturating_sub(1),
        };
        match self.focus {
            Focus::Running => self.act(Act::Live(to(self.live_row(), self.running.len()))),
            Focus::Targets => self.act(Act::Select(to(Some(self.sel), self.items.len()))),
            Focus::History => self.act(Act::Past(to(self.run_row(), self.runs.len()))),
            Focus::Dibs => self.dibs_sel = to(Some(self.dibs_sel), self.leases()),
            Focus::Tests => self.act(Act::Test(to(Some(self.test_sel), self.tree_rows().len()))),
            Focus::Problems => self.act(Act::Problem(to(Some(self.problems_sel), self.problem_rows().len()))),
            Focus::Logs => {}
        }
    }

    /// In the running block or the history: the first row of the next section, or of the previous one (from a
    /// section's first row) / this one.
    fn section_jump(&mut self, next: bool) {
        let (secs, at): (Vec<&str>, usize) = match self.focus {
            Focus::Running => (self.running.iter().map(|(s, _)| *s).collect(), self.live_row().unwrap_or(0)),
            Focus::History => (self.runs.iter().map(|r| self.run_place(r)).collect(), self.run_row().unwrap_or(0)),
            _ => return,
        };
        let Some(&cur) = secs.get(at) else { return };
        let sec = |i: usize| secs[i];
        let to = if next {
            (at + 1..secs.len()).find(|&i| sec(i) != cur)
        } else {
            let start = (0..at).rev().take_while(|&i| sec(i) == cur).last().unwrap_or(at);
            let prev = if start == at { start.checked_sub(1) } else { Some(start) };
            prev.map(|p| (0..=p).rev().take_while(|&i| sec(i) == sec(p)).last().unwrap_or(p))
        };
        if let Some(i) = to {
            self.act(if self.focus == Focus::Running { Act::Live(i) } else { Act::Past(i) });
        }
    }

    /// False to quit.
    fn key(&mut self, k: KeyCode) -> bool {
        if self.list_open() {
            let device = self.input == Some(Input::Device);
            let last = self.list().0.len().saturating_sub(1);
            match k {
                KeyCode::Down | KeyCode::Tab => self.pick = if self.pick >= last { 0 } else { self.pick + 1 },
                KeyCode::Up | KeyCode::BackTab => self.pick = if self.pick == 0 { last } else { self.pick - 1 },
                KeyCode::Char('j') if !device => self.pick = (self.pick + 1).min(last),
                KeyCode::Char('k') if !device => self.pick = self.pick.saturating_sub(1),
                // ponytail: pages by the log pane's height, which the list's height tracks; store the list height if they drift
                KeyCode::PageDown => self.pick = (self.pick + self.page.max(1)).min(last),
                KeyCode::PageUp => self.pick = self.pick.saturating_sub(self.page.max(1)),
                KeyCode::Char(c) if device => {
                    self.device.push(c);
                    self.pick = 0;
                }
                KeyCode::Backspace if device => {
                    self.device.pop();
                    self.pick = 0;
                }
                KeyCode::Enter => self.act(Act::Pick(self.pick)),
                KeyCode::Esc => self.input = None,
                _ => {}
            }
            return true;
        }
        if self.input == Some(Input::Meta) {
            match k {
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.pick = (self.pick + 1) % META.len(),
                KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => self.pick = (self.pick + META.len() - 1) % META.len(),
                KeyCode::Enter | KeyCode::Char(' ') => self.act(Act::Meta(self.pick)),
                KeyCode::Esc | KeyCode::Char('v') => self.input = None,
                _ => {}
            }
            return true;
        }
        if let Some(input) = self.input {
            let s = if input == Input::Filter { &mut self.filter } else { &mut self.search };
            match k {
                KeyCode::Char(c) => s.push(c),
                KeyCode::Backspace => _ = s.pop(),
                KeyCode::Enter | KeyCode::Esc => self.input = None,
                _ => {}
            }
            return true;
        }
        if let Some(p) = &self.prompt {
            let a = match (p, k) {
                (Prompt::Take(_, _, only), KeyCode::Char('y')) => Some(Act::Take(only.clone())),
                (Prompt::Lease(id), KeyCode::Char('x')) => Some(Act::Release(*id)),
                (Prompt::Lease(id), KeyCode::Char('n')) => Some(Act::Renew(*id)),
                (_, KeyCode::Esc | KeyCode::Char('n')) => Some(Act::Cancel),
                _ => None,
            };
            if let Some(a) = a {
                self.act(a);
            }
            return true;
        }
        match k {
            KeyCode::Char('q') => return false,
            KeyCode::Char('z') => self.act(Act::Sidebar),
            KeyCode::Char(c @ '1'..='5') => self.act(Act::Tab(TABS[c as usize - '1' as usize].0)),
            KeyCode::Tab if self.sidebar => self.cycle_focus(false),
            KeyCode::BackTab if self.sidebar => self.cycle_focus(true),
            KeyCode::Esc if self.focus == Focus::Logs && self.sidebar => {
                self.focus = match (self.live_row(), &self.run) {
                    (Some(_), _) => Focus::Running,
                    (None, Some(_)) => Focus::History,
                    (None, None) => self.tab,
                };
                if self.focus == Focus::History {
                    self.tab = Focus::History;
                }
            }
            KeyCode::Char('t') if self.shown().is_some_and(|p| p["kind"] == "test") => self.act(Act::View(!self.report_on())),
            KeyCode::Char('j') | KeyCode::Down if self.focus == Focus::Logs && self.report_on() => {
                self.report_sel = (self.report_sel + 1).min(self.report_failures().len().saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up if self.focus == Focus::Logs && self.report_on() => self.report_sel = self.report_sel.saturating_sub(1),
            KeyCode::Enter if self.focus == Focus::Logs && self.report_on() => {
                let f = self.report_failures().get(self.report_sel).map(|t| (text(&t["process"]).to_string(), t["failure"]["cursor"].as_str().map(String::from)));
                let build = self.shown().filter(|p| p["built"] == false && !p["ended_at"].is_null()).map(|p| (text(&p["id"]).to_string(), None));
                if let Some((id, cursor)) = f.or(build) {
                    self.act(Act::Failure(id, cursor));
                }
            }
            KeyCode::Char('j') | KeyCode::Down if self.focus == Focus::Logs => self.step_record(true),
            KeyCode::Char('k') | KeyCode::Up if self.focus == Focus::Logs => self.step_record(false),
            KeyCode::Enter if self.focus == Focus::Logs => self.toggle_at_cursor(),
            KeyCode::Enter if self.focus == Focus::Dibs => {
                if let Some(id) = self.status["leases"][self.dibs_sel]["id"].as_u64() {
                    self.act(Act::Lease(id));
                }
            }
            KeyCode::Enter | KeyCode::Left | KeyCode::Right if self.focus == Focus::Problems => {
                let rows = self.problem_rows();
                let Some(row) = rows.get(self.problems_sel) else { return true };
                let unfolded = rows.get(self.problems_sel + 1).is_some_and(|r| r.depth == 1);
                match (&row.item, k) {
                    (Some(p), KeyCode::Enter) if p["resolved"] == false => self.msg = format!("{} isn't in this worktree", text(&p["path"])),
                    (Some(p), KeyCode::Enter) => self.edit(text(&p["path"]), p["line"].as_u64().unwrap_or(1)),
                    (Some(_), KeyCode::Left) => self.problems_sel = rows[..self.problems_sel].iter().rposition(|r| r.depth == 0).unwrap_or(0),
                    (Some(_), _) => {}
                    (None, KeyCode::Enter) => _ = self.open.remove(&row.key) || self.open.insert(row.key.clone()),
                    (None, KeyCode::Left) if unfolded => _ = self.open.remove(&row.key) || self.open.insert(row.key.clone()),
                    (None, KeyCode::Right) if !unfolded => _ = self.open.remove(&row.key) || self.open.insert(row.key.clone()),
                    (None, _) => {}
                }
            }
            KeyCode::Char('w') if self.focus == Focus::Problems => {
                let kinds = ["error", "warning", "lint", "todo"];
                let next = match self.problems_kind.and_then(|k| kinds.iter().position(|x| *x == k)) {
                    None => Some(kinds[0]),
                    Some(i) => kinds.get(i + 1).copied(),
                };
                (self.problems_kind, self.problems_sel) = (next, 0);
            }
            KeyCode::Char('b') if self.focus == Focus::Problems && !self.problems["new_totals"].is_null() => self.act(Act::ProblemsNew(!self.problems_new)),
            KeyCode::Char('u') if self.focus == Focus::Problems => self.fetch_problems(true),
            KeyCode::Enter if self.focus == Focus::Tests => {
                let rows = self.tree_rows();
                let Some(row) = rows.get(self.test_sel) else { return true };
                match &row.item {
                    Some(t) if t["status"] == "failed" => self.act(Act::Failure(text(&t["process"]).into(), t["failure"]["cursor"].as_str().map(String::from))),
                    Some(_) => {}
                    None if !self.open.remove(&row.key) => _ = self.open.insert(row.key.clone()),
                    None => {}
                }
            }
            KeyCode::Left if self.focus == Focus::Tests => {
                let rows = self.tree_rows();
                let Some(row) = rows.get(self.test_sel) else { return true };
                if row.item.is_some() || !self.open.remove(&row.key) {
                    let parent = rows[..self.test_sel].iter().rposition(|r| r.depth < row.depth);
                    self.test_sel = parent.unwrap_or(self.test_sel);
                }
            }
            KeyCode::Right if self.focus == Focus::Tests => {
                if let Some(row) = self.tree_rows().get(self.test_sel).filter(|r| r.item.is_none()) {
                    self.open.insert(row.key.clone());
                }
            }
            KeyCode::Left => self.section_jump(false),
            KeyCode::Right => self.section_jump(true),
            KeyCode::Char('v') => self.act(Act::Edit(Input::Meta)),
            KeyCode::Char('j') | KeyCode::Down => self.step(true),
            KeyCode::Char('k') | KeyCode::Up => self.step(false),
            KeyCode::Char('r' | 'R') if self.focus == Focus::Logs && self.report_on() && !self.report_failures().is_empty() => {
                let id = self.report_failures().get(self.report_sel).and_then(|t| t["id"].as_str().map(String::from));
                self.act(Act::Run(id.into_iter().collect()));
            }
            KeyCode::Char('r' | 'R') if self.focus == Focus::Tests && self.marked().is_empty() => {
                let id = self.tree_rows().get(self.test_sel).and_then(|r| self.row_id(r));
                self.act(Act::Run(id.into_iter().collect()));
            }
            KeyCode::Char('r' | 'R') => self.act(Act::Run(self.marked())),
            KeyCode::Char('F') if !self.failed_ids().is_empty() => self.act(Act::Run(self.failed_ids())),
            KeyCode::Char(' ') if self.focus == Focus::Tests => self.act(Act::Check(self.test_sel)),
            KeyCode::Esc if self.focus == Focus::Tests => self.marks.clear(),
            KeyCode::Char('s') => self.act(Act::Stop),
            KeyCode::Char('o') => self.act(Act::Open),
            KeyCode::Char('m') => self.act(Act::Mark),
            KeyCode::Char('f') => self.act(Act::Edit(Input::Filter)),
            KeyCode::Char('/') => self.act(Act::Edit(Input::Search)),
            KeyCode::Char('d') if self.sim_choice().is_some_and(|(_, c)| !c["physical"].is_object()) => self.act(Act::Edit(Input::Device)),
            KeyCode::Char('h') => {
                if let Some((_, cur)) = self.sim_choice().filter(|(_, c)| !c["physical"].is_object()) {
                    self.act(Act::Head(cur["head"] != true));
                }
            }
            KeyCode::Char('i') if self.sim_choice().is_some() => self.act(Act::Edit(Input::Runtime)),
            // `rows()` reverses matching lines; matching the row text instead would also hit the metadata columns.
            KeyCode::Char('n') => self.jump(true, |_, (l, _)| l.style.add_modifier.contains(Modifier::REVERSED)),
            KeyCode::Char('N') => self.jump(false, |_, (l, _)| l.style.add_modifier.contains(Modifier::REVERSED)),
            KeyCode::Char('[') => self.jump(true, |app, (_, i)| app.view[*i].is_none()),
            KeyCode::Char(']') => self.jump(false, |app, (_, i)| app.view[*i].is_none()),
            KeyCode::Char('g') | KeyCode::Home => self.scroll = usize::MAX,
            KeyCode::Char('G') | KeyCode::End => self.scroll = 0,
            KeyCode::PageUp => self.scroll += self.page,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page),
            KeyCode::Char('e') => {
                self.expand_all = !self.expand_all;
                self.expanded.clear();
            }
            KeyCode::Char('c') => {
                self.view.clear();
                self.expanded.clear();
                self.scroll = 0;
            }
            _ => {}
        }
        true
    }

    fn mouse(&mut self, m: MouseEvent) {
        let at = Position::new(m.column, m.row);
        let last = self.list().0.len().saturating_sub(1);
        match m.kind {
            MouseEventKind::ScrollUp if self.list_open() => self.pick = self.pick.saturating_sub(3),
            MouseEventKind::ScrollDown if self.list_open() => self.pick = (self.pick + 3).min(last),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, a)) = self.hits.iter().rev().find(|(r, _)| r.contains(at)).cloned() {
                    self.act(a);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if self.running_area.contains(at) || self.tabs_area.contains(at) => {
                self.focus = if self.running_area.contains(at) { Focus::Running } else { self.tab };
                self.step(m.kind == MouseEventKind::ScrollDown);
            }
            MouseEventKind::ScrollUp if self.logs_area.contains(at) => self.scroll += 3,
            MouseEventKind::ScrollDown if self.logs_area.contains(at) => self.scroll = self.scroll.saturating_sub(3),
            _ => {}
        }
    }

    fn is_hit(&self, l: &Line) -> bool {
        !self.search.is_empty() && line_text(l).to_lowercase().contains(&self.search.to_lowercase())
    }

    /// Scrolls so the nearest row matching `pred` above (older) or below the current top row is at the top.
    fn jump(&mut self, older: bool, pred: impl Fn(&App, &(Line<'static>, usize)) -> bool) {
        let rows = self.rows(self.logs_width());
        let (len, h) = (rows.len(), self.page.max(1));
        let top = (len - self.scroll.min(len.saturating_sub(h))).saturating_sub(h);
        let found = if older { (0..top).rev().find(|&j| pred(self, &rows[j])) } else { (top + 1..len).find(|&j| pred(self, &rows[j])) };
        match found {
            Some(j) => self.scroll = len.saturating_sub(j + h),
            None => self.msg = "no further match".into(),
        }
    }

    /// The log pane's width inside its border, as `draw_logs` last laid it out.
    fn logs_width(&self) -> usize {
        self.logs_area.width.saturating_sub(2) as usize
    }

    fn keeps(&self, r: &Record, text_filter: &Filter, needle: &str) -> bool {
        text_filter.keep(r)
            && self.level.is_none_or(|l| r.level >= l)
            && self.source.is_none_or(|s| r.source == s)
            && r.line.to_lowercase().contains(needle)
    }

    /// The stream as display rows, each tagged with the view entry it came from.
    fn rows(&self, width: usize) -> Vec<(Line<'static>, usize)> {
        let (filter, needle) = self.parsed_filter();
        let mut rows = vec![];
        for (i, entry) in self.view.iter().enumerate() {
            match entry {
                None => rows.push((Line::from("─".repeat(width)).cyan(), i)),
                Some(r) if self.keeps(r, &filter, &needle) => rows.extend(self.record_rows(i, r, width).into_iter().map(|l| (l, i))),
                Some(_) => {}
            }
        }
        rows
    }

    /// One record's rows, wrapped to `width`. Multi-line records (compiler errors) collapse to their first line unless expanded.
    fn record_rows(&self, i: usize, r: &Record, width: usize) -> Vec<Line<'static>> {
        let style = level_style(r.level);
        let line = text_form(&r.line);
        let mut lines = line.lines();
        let first = lines.next().unwrap_or_default().to_string();
        let rest: Vec<String> = lines.map(String::from).collect();
        let open = self.expand_all || self.expanded.contains(&i);
        let mut out = if rest.is_empty() {
            vec![Line::styled(first, style)]
        } else if open {
            let mut v = vec![Line::from(vec![Span::styled(first, style), " ▾".dim()])];
            v.extend(rest.into_iter().map(|l| Line::styled(l, style)));
            v
        } else {
            vec![Line::from(vec![Span::styled(first, style), format!(" ▸ +{} lines", rest.len()).dim()])]
        };
        for l in &mut out {
            if self.is_hit(l) {
                *l = l.clone().reversed();
            }
        }
        let mut rows: Vec<Line<'static>> = out.into_iter().flat_map(|l| wrap(l, width)).collect();
        rows.extend(self.meta_line(r));
        rows
    }

    /// The enabled metadata columns for `r` as one dim line to go under it (`  10:35:28.001 · E · err · pid 4123`);
    /// None with none enabled.
    fn meta_line(&self, r: &Record) -> Option<Line<'static>> {
        let mut cols: Vec<Span<'static>> = vec![];
        if self.meta.time {
            cols.push(clock(r.ts, self.utc_offset).into());
        }
        if self.meta.level {
            let (c, style) = match r.level {
                Level::Error => ("E", Style::new().red().bold()),
                Level::Warn => ("W", Style::new().yellow().bold()),
                Level::Info => ("I", Style::new().blue()),
                Level::Debug => ("D", Style::new().dark_gray()),
            };
            cols.push(Span::styled(c, style));
        }
        if self.meta.source {
            let src = match r.source {
                Source::Build => "build",
                Source::Stdout => "out",
                Source::Stderr => "err",
            };
            cols.push(src.into());
        }
        if self.meta.pid {
            cols.push(format!("pid {}", self.pid.map_or("-".into(), |p| p.to_string())).into());
        }
        if cols.is_empty() {
            return None;
        }
        let mut spans = vec![Span::raw("  ")];
        for (k, c) in cols.into_iter().enumerate() {
            if k > 0 {
                spans.push(" · ".into());
            }
            spans.push(c);
        }
        Some(Line::from(spans).dim())
    }

    /// `level=warn source=build some text`: level/source tokens, the rest a case-insensitive substring.
    fn parsed_filter(&self) -> (Filter, String) {
        let (mut f, mut words) = (Filter::default(), vec![]);
        for tok in self.filter.split_whitespace() {
            match tok.split_once('=') {
                Some(("level", v)) => f.level = serde_json::from_value(json!(v)).ok(),
                Some(("source", v)) => f.source = serde_json::from_value(json!(v)).ok(),
                _ => words.push(tok),
            }
        }
        (f, words.join(" ").to_lowercase())
    }

    /// Lays `spans` out left to right in `area`; spans with an action become click targets.
    fn strip(&mut self, f: &mut Frame, area: Rect, spans: Vec<(Span<'static>, Option<Act>)>) {
        let mut x = area.x;
        for (span, act) in spans {
            if span.width() == 0 {
                continue;
            }
            let w = (span.width() as u16).min(area.right().saturating_sub(x));
            if w == 0 {
                break;
            }
            let r = Rect::new(x, area.y, w, 1);
            f.render_widget(span, r);
            if let Some(a) = act {
                self.hits.push((r, a));
            }
            x += w;
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        self.hits.clear();
        let area = f.area();
        let narrow = area.width < NARROW;
        let [main, bottom] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let secs: Vec<&str> = self.running.iter().map(|(s, _)| *s).collect();
        let running_rows = if secs.is_empty() { 0 } else { secs.len() + (0..secs.len()).filter(|&i| heads(&secs, i)).count() + 2 } as u16;
        let (running_area, tabs_area, right) = if !self.sidebar {
            (Rect::default(), Rect::default(), main)
        } else if area.width >= WIDE {
            let [left, right] = Layout::horizontal([Constraint::Length(36), Constraint::Min(1)]).areas(main);
            let [t, r] = Layout::vertical([Constraint::Min(1), Constraint::Length(running_rows.min(left.height * 2 / 5))]).areas(left);
            (r, t, right)
        } else {
            let tab_rows = match self.tab {
                Focus::Targets => self.items.len(),
                Focus::History => self.runs.len(),
                Focus::Dibs => self.leases() + self.status["queue"].as_array().map_or(0, Vec::len),
                _ => 1,
            } as u16;
            let band = running_rows.max(tab_rows + 2).max(5).min(main.height * 2 / 5);
            let [top, right] = Layout::vertical([Constraint::Length(band), Constraint::Min(1)]).areas(main);
            let [t, r] = match running_rows {
                0 => [top, Rect::default()],
                _ => Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(top),
            };
            (r, t, right)
        };
        let chip_rows = if narrow { 2 } else { 1 };
        let header_rows = if self.sim_choice().is_some() { 3 } else { 2 };
        let [header, chips, logs_area] =
            Layout::vertical([Constraint::Length(header_rows), Constraint::Length(chip_rows), Constraint::Min(1)]).areas(right);
        self.logs_area = logs_area;

        self.running_area = running_area;
        self.tabs_area = tabs_area;
        if !running_area.is_empty() {
            self.draw_running(f, running_area);
        }
        if !tabs_area.is_empty() {
            self.draw_tabs(f, tabs_area);
        }
        self.draw_header(f, header, narrow);
        self.draw_chips(f, chips, narrow);
        if self.report_on() {
            self.draw_report(f, logs_area);
        } else {
            self.draw_logs(f, logs_area);
        }
        self.draw_bottom(f, bottom);
        self.draw_list(f, bottom.y);
        self.draw_meta(f, bottom.y);
    }

    /// The open runtime or device list, as a popup under its field that ends above `bottom_y`. Drawn last: the
    /// full-frame Cancel hit it pushes, then its rows, win every click while it's open.
    fn draw_list(&mut self, f: &mut Frame, bottom_y: u16) {
        let Some(input) = self.input.filter(|_| self.list_open()) else { return };
        if self.sim_choice().is_none() {
            self.input = None;
            return;
        }
        let anchor = self.hits.iter().find(|(_, a)| matches!(a, Act::Edit(i) if *i == input)).map(|(r, _)| *r);
        let (labels, now) = self.list();
        let n = labels.len();
        self.pick = self.pick.min(n.saturating_sub(1));
        let title = match input {
            Input::Device => format!(" {}▏ {}/{} ", self.device, if n == 0 { 0 } else { self.pick + 1 }, n),
            _ => String::new(),
        };
        let screen = f.area();
        let inner_w = labels.iter().map(|l| l.chars().count() + 4).max().unwrap_or(0).max(title.chars().count()).max(10);
        let w = (inner_w as u16 + 2).min(screen.width);
        // No anchor: the pane is too short to draw the selector line at all.
        let anchor = anchor.unwrap_or(Rect { y: bottom_y, ..Rect::default() });
        let x = anchor.x.min(screen.right().saturating_sub(w));
        let y = anchor.y + 1;
        let room = bottom_y.saturating_sub(y);
        if room < 3 {
            self.input = None;
            self.msg = "pane too short for the list".into();
            return;
        }
        let h = (n.max(1) as u16 + 2).min(room);
        let area = Rect::new(x, y, w, h);
        let shown = (h - 2) as usize;
        // The window follows the highlight, centred where possible, so the wheel needs no scroll offset of its own.
        let start = self.pick.saturating_sub(shown / 2).min(n.saturating_sub(shown));
        f.render_widget(ratatui::widgets::Clear, area);
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.hits.push((screen, Act::Cancel));
        if n == 0 {
            f.render_widget(Line::from(" no match").dim(), Rect { height: 1, ..inner });
        }
        for (k, i) in (start..n).take(shown).enumerate() {
            let row = Rect::new(inner.x, inner.y + k as u16, inner.width, 1);
            let label = format!(" {} {:<width$}", if now == Some(i) { "•" } else { " " }, labels[i], width = inner.width as usize);
            let mut line = Line::from(label);
            if self.physical_row(i).is_some_and(|d| d["state"] == "unavailable") {
                line = line.dim();
            }
            f.render_widget(if i == self.pick { line.reversed() } else { line }, row);
            self.hits.push((row, Act::Pick(i)));
        }
    }

    /// Every live process, under section headers; the window follows the pinned one, and the title counts what is out
    /// of view.
    fn draw_running(&mut self, f: &mut Frame, area: Rect) {
        let secs: Vec<&'static str> = self.running.iter().map(|(s, _)| *s).collect();
        let mut rows: Vec<(Option<usize>, &'static str)> = vec![];
        for (i, s) in secs.iter().enumerate() {
            if heads(&secs, i) {
                rows.push((None, s));
            }
            rows.push((Some(i), s));
        }
        let h = area.height.saturating_sub(2) as usize;
        let sel = self.live_row();
        let at = rows.iter().position(|(r, _)| r.is_some() && *r == sel).unwrap_or(0);
        let first = window(&rows.iter().map(|(r, _)| r.is_none()).collect::<Vec<_>>(), at, h);
        let below = rows.len().saturating_sub(first + h);
        let mut title = format!("running {}", self.running.len());
        if first > 0 {
            title += &format!(" ↑{first}");
        }
        if below > 0 {
            title += &format!(" ↓{below}");
        }
        let block = Block::bordered().title(title).border_style(if self.focus == Focus::Running { Style::new().cyan() } else { Style::new() });
        let inner = block.inner(area);
        f.render_widget(block, area);
        let (me, now) = (crate::holder(), now_ms());
        for (k, (i, section)) in rows.into_iter().skip(first).take(h).enumerate() {
            let row = Rect::new(inner.x, inner.y + k as u16, inner.width, 1);
            let Some(i) = i else {
                f.render_widget(Line::from(section).dim().italic(), row);
                continue;
            };
            let p = &self.running[i].1;
            let mut spans = vec![Span::raw(format!("{} {} ", if sel == Some(i) { "›" } else { " " }, text(&p["target"]))), badge(p)];
            if p["kind"] == "exec" {
                spans.push(" (exec)".dim());
            }
            if text(&p["holder"]) != me {
                spans.push(format!(" {}", who(text(&p["holder"]), &p["agent"].as_str().map(Into::into))).dim());
            }
            if section.starts_with("other") {
                spans.push(format!(" · {}", Path::new(text(&p["dir"])).file_name().unwrap_or_default().to_string_lossy()).dim());
            }
            spans.push(format!(" {}", ago(now.saturating_sub(p["since"].as_u64().unwrap_or(now)))).dim());
            let line = Line::from(spans);
            f.render_widget(if sel == Some(i) { line.reversed() } else { line }, row);
            self.hits.push((row, Act::Live(i)));
        }
    }

    /// The tab strip over the block's top border, then the open tab inside it.
    fn draw_tabs(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == self.tab;
        let block = Block::bordered().border_style(if focused { Style::new().cyan() } else { Style::new() });
        let inner = block.inner(area);
        f.render_widget(block, area);
        let line = Rect { x: area.x + 1, height: 1, width: area.width.saturating_sub(2), ..area };
        let failed = self.results.values().flat_map(|(_, v)| v["tests"].as_array().into_iter().flatten()).filter(|t| t["status"] == "failed").count();
        let new = self.problems["new_totals"].as_object().map_or(0, |t| t.values().filter_map(Value::as_u64).sum::<u64>());
        let chips = |counts: bool, short: bool| -> Vec<(Span<'static>, Option<Act>)> {
            let n = |t| match t {
                Focus::Dibs if counts && self.leases() > 0 => format!(" {}", self.leases()),
                Focus::Tests if counts && failed > 0 => format!(" ✗{failed}"),
                Focus::Problems if counts && new > 0 => format!(" +{new}"),
                _ => String::new(),
            };
            TABS.iter().map(|&(t, label, s)| chip(&format!("{}{}", if short { s } else { label }, n(t)), t == self.tab, Act::Tab(t))).collect()
        };
        // Counts go first, then the long labels, so every tab stays clickable.
        let width = |c: &[(Span, Option<Act>)]| c.iter().map(|(s, _)| s.width()).sum::<usize>();
        let tiers = [(true, false), (false, false), (true, true)];
        let (counts, short) = tiers.into_iter().find(|&(c, s)| width(&chips(c, s)) <= line.width as usize).unwrap_or((false, true));
        let chips = chips(counts, short);
        self.strip(f, line, chips);
        match self.tab {
            Focus::History => self.draw_history(f, inner),
            Focus::Dibs => self.draw_leases(f, inner),
            Focus::Tests => self.draw_tests(f, inner),
            Focus::Problems => self.draw_problems(f, inner),
            _ => self.draw_targets(f, inner),
        }
    }

    /// Each local test target and, unfolded, its groups and tests from its ended runs; a target's results dim while
    /// it runs again and after a build that failed (stale). The window follows the selection.
    fn draw_tests(&mut self, f: &mut Frame, inner: Rect) {
        let rows = self.tree_rows();
        if rows.is_empty() {
            f.render_widget(Line::from(" no test targets here").dim(), Rect { height: 1.min(inner.height), ..inner });
            return;
        }
        self.test_sel = self.test_sel.min(rows.len() - 1);
        let (h, w) = (inner.height as usize, inner.width as usize);
        let first = (self.test_sel + 1).saturating_sub(h);
        let focused = self.focus == Focus::Tests;
        let mut dim = false;
        for (k, row) in rows.iter().enumerate() {
            let status: Span<'static> = match &row.item {
                Some(t) => {
                    let took = t["duration_ms"].as_u64().map_or(String::new(), |ms| format!(" {}", secs(ms)));
                    match text(&t["status"]) {
                        "passed" => format!("✓{took}").green(),
                        "skipped" => "⊘".dim(),
                        _ => format!("✗{took}").red(),
                    }
                }
                None if row.depth > 0 => match row.failed {
                    0 => format!("✓ {}", row.total).green(),
                    n => format!("✗ {n}/{}", row.total).red(),
                },
                None => {
                    let it = self.items.iter().find(|i| i.name == row.key);
                    let p = it.and_then(|i| self.proc_of(i));
                    let newest = self.results.get(&row.key).and_then(|(id, _)| self.runs.iter().find(|r| text(&r["id"]) == id));
                    let live = p.is_some_and(|p| p["ended_at"].is_null());
                    let stale = newest.is_some_and(|r| r["built"] == false);
                    dim = live || stale;
                    match (live, row.total, row.failed) {
                        (true, ..) => "⟳".yellow(),
                        (false, 0, _) if newest.is_none() => "not run · r to run all".dim(),
                        (false, 0, _) => p.map_or_else(|| "".into(), badge),
                        (false, n, 0) => format!("✓ {n}{}", if stale { " (stale)" } else { "" }).green(),
                        (false, n, x) => format!("✗ {x}/{n}{}", if stale { " (stale)" } else { "" }).red(),
                    }
                }
            };
            if k < first || k >= first + h {
                continue;
            }
            let arrow = match (&row.item, self.open.contains(&row.key)) {
                (Some(_), _) => "  ",
                (None, true) => "▾ ",
                (None, false) => "▸ ",
            };
            let indent = "  ".repeat(row.depth);
            let target = row.key.split('\t').next().unwrap_or_default();
            let slot = match self.row_id(row) {
                Some(id) if row.depth > 0 => if self.marks.contains(&format!("{target}\t{id}")) { "●" } else { " " },
                _ => "",
            };
            let lead = format!("{indent}{slot}{arrow}");
            // ponytail: cuts a long name at its end, not its middle.
            let room = w.saturating_sub(lead.chars().count() + status.width() + 1);
            let label: String = if row.label.chars().count() > room { row.label.chars().take(room.saturating_sub(1)).chain(['…']).collect() } else { row.label.clone() };
            let r = Rect::new(inner.x, inner.y + (k - first) as u16, inner.width, 1);
            let line = Line::from(format!("{lead}{label}"));
            let line = if dim && row.depth > 0 { line.dim() } else { line };
            f.render_widget(if focused && k == self.test_sel { line.reversed() } else { line }, r);
            f.render_widget(Line::from(status).right_aligned(), r);
            self.hits.push((r, Act::Test(k)));
            if !slot.is_empty() {
                self.hits.push((Rect { x: r.x + indent.len() as u16, width: 1, ..r }, Act::Check(k)));
            }
        }
    }

    /// This worktree's problems: a chip row (all or new; a filter per kind), in the new view how this branch's builds
    /// compare with the base branch's, then each file and, unfolded, its problems, the selected one in full. A
    /// problem whose target is building again is dimmed. The window follows the selection.
    fn draw_problems(&mut self, f: &mut Frame, inner: Rect) {
        if inner.height == 0 {
            return;
        }
        let v = self.problems.clone();
        let (narrow, focused) = (inner.width < 40, self.focus == Focus::Problems);
        let n = |k: &str| v["totals"][k].as_u64().unwrap_or_default();
        let base = v["base"]["ref"].as_str().map(|b| b.strip_prefix("origin/").unwrap_or(b).to_string());
        let mut chips = vec![chip("all", !self.problems_new, Act::ProblemsNew(false))];
        if let Some(t) = v["new_totals"].as_object() {
            let new: u64 = t.values().filter_map(Value::as_u64).sum();
            chips.push(chip(&format!("new +{new}"), self.problems_new, Act::ProblemsNew(true)));
        }
        chips.push(("│".dim(), None));
        let scan_error = v["scan"]["error"].as_str();
        for (k, glyph, letter) in [("error", "✗", "✗"), ("warning", "⚠", "⚠"), ("lint", "lint ", "L"), ("todo", "todo ", "T")] {
            let label = match k {
                "todo" if self.fetching => "todo …".to_string(),
                _ => format!("{}{}", if narrow { letter } else { glyph }, n(k)),
            };
            let (span, act) = chip(&label, self.problems_kind == Some(k), Act::ProblemsKind(k));
            chips.push((if k == "todo" && scan_error.is_some() { span.dim() } else { span }, act));
        }
        self.strip(f, Rect { height: 1, ..inner }, chips);
        let mut notes: Vec<Line<'static>> = vec![];
        if let (true, false, Some(base)) = (self.problems_new, narrow, &base) {
            let builds = v["base"]["builds"].as_object().cloned().unwrap_or_default();
            let mut parts = vec![];
            for (dir, sign) in [("added", "+"), ("fixed", "−")] {
                for (k, glyph) in [("error", "✗"), ("warning", "⚠"), ("lint", "L")] {
                    let c: u64 = builds.values().filter_map(|b| b[dir][k].as_u64()).sum();
                    if c > 0 {
                        parts.push(format!("{sign}{c}{glyph}"));
                    }
                }
            }
            let newest = builds.values().filter_map(|b| b["ended_at"].as_u64()).max();
            let missing: Vec<&str> = builds.iter().filter(|(_, b)| b.is_null()).map(|(t, _)| t.as_str()).collect();
            if let Some(at) = newest {
                let ago = ago(now_ms().saturating_sub(at));
                notes.push(Line::from(format!("vs {base}: {} · {base} built {ago} ago", if parts.is_empty() { "same".into() } else { parts.join(" ") })).dim());
            }
            if !missing.is_empty() {
                notes.push(Line::from(format!("no {base} build of {}: their warnings aren't compared", missing.join(", "))).dim());
            }
        }
        if let Some(e) = scan_error {
            notes.push(Line::from(format!("TODO scan off: {e}")).dim());
        }
        let rows = self.problem_rows();
        if rows.is_empty() {
            let never = v.is_null() || v["items"].as_array().is_some_and(Vec::is_empty) && v["base"]["builds"].as_object().is_some_and(|b| b.is_empty());
            let say = if never {
                "no builds in this worktree yet · u to scan TODOs"
            } else if self.problems_new {
                "nothing new on this branch"
            } else {
                "no problems"
            };
            notes.push(Line::from(say).dim());
        }
        let mut y = inner.y + 1;
        for line in notes.into_iter().take(inner.height.saturating_sub(1) as usize) {
            f.render_widget(line, Rect { y, height: 1, ..inner });
            y += 1;
        }
        if rows.is_empty() {
            return;
        }
        self.problems_sel = self.problems_sel.min(rows.len() - 1);
        let body = Rect { y, height: inner.bottom().saturating_sub(y), ..inner };
        let width = body.width as usize;
        let live: HashSet<String> = self.items.iter().filter(|i| self.proc_of(i).is_some_and(|p| p["ended_at"].is_null())).map(|i| i.name.clone()).collect();
        let mut lines: Vec<(Line<'static>, Option<Line<'static>>, usize)> = vec![];
        let mut at = 0;
        for (k, row) in rows.iter().enumerate() {
            let sel = focused && k == self.problems_sel;
            if k == self.problems_sel {
                at = lines.len();
            }
            let Some(p) = &row.item else {
                let unfolded = rows.get(k + 1).is_some_and(|r| r.depth == 1);
                let items = v["items"].as_array().into_iter().flatten().filter(|i| i["path"] == row.label.as_str());
                let targets: HashSet<&str> = items.flat_map(|i| i["sources"].as_array().into_iter().flatten().filter_map(|s| s["target"].as_str())).collect();
                let mut right = vec![];
                if row.failed > 0 {
                    right.push(format!("✗{}", row.failed));
                }
                if row.total > row.failed {
                    right.push(format!("⚠{}", row.total - row.failed));
                }
                if targets.len() > 1 {
                    right.push(format!("· {}", targets.into_iter().collect::<Vec<_>>().join(", ")));
                }
                let right = right.join(" ");
                let label = elide(&row.label, width.saturating_sub(right.chars().count() + 4));
                let line = Line::from(format!("{} {label}", if unfolded { "▾" } else { "▸" }));
                lines.push((if sel { line.reversed() } else { line }, Some(Line::from(right).dim()), k));
                continue;
            };
            let (kind, new) = (text(&p["kind"]), p["new"] == true);
            let glyph = match kind {
                "error" => "✗".red(),
                "warning" => "⚠".yellow(),
                "lint" => "L".dim(),
                _ => text(&p["tag"]).to_string().cyan(),
            };
            let mut spans = vec![if new { "  + ".yellow() } else { "    ".into() }, format!("{:>4} ", p["line"].as_u64().unwrap_or_default()).dim(), glyph, " ".into()];
            spans.push(Span::raw(text_form(text(&p["message"]))));
            if let Some(rule) = p["rule"].as_str() {
                spans.push(format!(" ({rule})").dim());
            }
            let rebuilding = p["sources"][0]["target"].as_str().is_some_and(|t| live.contains(t));
            let line = Line::from(spans);
            let line = if rebuilding { line.dim() } else { line };
            lines.push((if sel { line.reversed() } else { line }, None, k));
            if k == self.problems_sel {
                for l in wrap(Line::from(text_form(text(&p["message"]))), width.saturating_sub(6)) {
                    let mut l = l;
                    l.spans.insert(0, "      ".into());
                    lines.push((l, None, k));
                }
                let src = &p["sources"][0];
                let from = match src["target"].as_str() {
                    Some(t) => format!("{t} build {} ago", ago(now_ms().saturating_sub(src["ended_at"].as_u64().unwrap_or_default()))),
                    None => format!("scan {} ago", ago(now_ms().saturating_sub(v["scan"]["scanned_at"].as_u64().unwrap_or_default()))),
                };
                let found = if p["resolved"] == false { " (not found in this worktree)" } else { "" };
                lines.push((Line::from(format!("      {}:{}{found}", text(&p["path"]), p["line"])).dim(), None, k));
                lines.push((Line::from(format!("      {from}")).dim(), None, k));
                if new {
                    lines.push((Line::from("      new on this branch").yellow(), None, k));
                }
            }
        }
        let h = body.height as usize;
        let span = lines.iter().filter(|(_, _, k)| *k == self.problems_sel).count();
        let first = (at + span).saturating_sub(h).min(at);
        for (j, (line, right, k)) in lines.into_iter().skip(first).take(h).enumerate() {
            let r = Rect::new(body.x, body.y + j as u16, body.width, 1);
            f.render_widget(line, r);
            if let Some(right) = right {
                f.render_widget(right.right_aligned(), r);
            }
            self.hits.push((r, Act::Problem(k)));
        }
    }

    /// This worktree's targets; the window follows the selection.
    fn draw_targets(&mut self, f: &mut Frame, inner: Rect) {
        let h = inner.height as usize;
        let first = (self.sel + 1).saturating_sub(h);
        for i in (first..self.items.len()).take(h) {
            let row = Rect::new(inner.x, inner.y + (i - first) as u16, inner.width, 1);
            let it = &self.items[i];
            let mut spans = vec![Span::raw(format!("{} {}", if i == self.sel { "›" } else { " " }, it.name))];
            if self.only_builds(it) {
                spans.push(" build".dim());
            }
            if let Some(p) = self.proc_of(it) {
                spans.push(" ".into());
                spans.push(badge(p));
            }
            let line = Line::from(spans);
            f.render_widget(if i == self.sel && self.run.is_none() { line.reversed() } else { line }, row);
            self.hits.push((row, Act::Select(i)));
        }
    }

    /// Every ended run, newest first; another worktree's dimmed, with its name.
    fn draw_history(&mut self, f: &mut Frame, inner: Rect) {
        if self.runs.is_empty() {
            f.render_widget(Line::from(" no ended runs").dim(), Rect { height: 1.min(inner.height), ..inner });
            return;
        }
        let h = inner.height as usize;
        let at = self.run_row();
        // The window follows the pinned run.
        let first = at.map_or(0, |a| (a + 1).saturating_sub(h));
        let now = now_ms();
        for (k, r) in self.runs.iter().enumerate().skip(first).take(h) {
            let dir = Path::new(text(&r["dir"]));
            let mut spans = vec![Span::raw(format!("{} {} ", if at == Some(k) { "›" } else { " " }, text(&r["target"])))];
            spans.extend(run_badge(r, now));
            if r["only"] == true {
                spans.push(" (only)".dim());
            }
            let here = inside(&self.root, dir);
            if !here {
                spans.push(format!(" · {}", dir.file_name().unwrap_or_default().to_string_lossy()).into());
            }
            let line = Line::from(spans);
            let line = if here { line } else { line.dim() };
            let row = Rect::new(inner.x, inner.y + (k - first) as u16, inner.width, 1);
            f.render_widget(if at == Some(k) { line.reversed() } else { line }, row);
            self.hits.push((row, Act::Past(k)));
        }
    }

    /// Leases, each selectable (one or two rows), then the dim queue; the window follows the selection.
    fn draw_leases(&mut self, f: &mut Frame, inner: Rect) {
        let now = now_ms();
        let holder = |v: &Value| who(text(&v["holder"]), &v["agent"].as_str().map(Into::into));
        let mut rows: Vec<(Line, Option<(usize, u64)>)> = vec![];
        for (i, l) in self.status["leases"].as_array().into_iter().flatten().enumerate() {
            let age = ago(now.saturating_sub(l["acquired_at"].as_u64().unwrap_or(now)));
            let sim = if l["device"].is_object() { &l["device"] } else { &l["simulator"] };
            let what = match (sim["name"].as_str(), l["service"].is_object()) {
                (Some(name), _) => format!("{name} "),
                (None, true) => format!("{} :{} ", text(&l["resource"]), l["port"]),
                (None, false) => format!(":{} ", l["port"]),
            };
            let line = Line::from(vec![what.bold(), Span::raw(holder(l)), format!(" {age}").dim()]);
            let id = l["id"].as_u64().map(|id| (i, id));
            rows.push((line, id));
            if sim.is_object() {
                let kind = format!("  {} · {}", text(&sim["device"]), text(&sim["runtime"]));
                rows.push((Line::from(kind).dim(), id));
            }
        }
        for w in self.status["queue"].as_array().into_iter().flatten() {
            let want = match (w["device"]["name"].as_str(), w["preferred"].as_u64()) {
                (Some(name), _) => name.to_string(),
                (None, Some(p)) => format!(":{p}"),
                (None, None) => "any port".into(),
            };
            let process = w["process"].as_str().map_or(String::new(), |p| format!(" ({p})"));
            rows.push((Line::from(format!("… {}{process} waits for {want}", holder(w))).dim(), None));
        }
        if rows.is_empty() {
            f.render_widget(Line::from(" no dibs").dim(), Rect { height: 1.min(inner.height), ..inner });
            return;
        }
        self.dibs_sel = self.dibs_sel.min(self.leases().saturating_sub(1));
        let h = inner.height as usize;
        let at = rows.iter().rposition(|(_, l)| l.is_some_and(|(i, _)| i == self.dibs_sel)).unwrap_or(0);
        let first = (at + 1).saturating_sub(h);
        let focused = self.focus == Focus::Dibs;
        for (k, (line, lease)) in rows.into_iter().enumerate().skip(first).take(h) {
            let row = Rect::new(inner.x, inner.y + (k - first) as u16, inner.width, 1);
            let on = focused && lease.is_some_and(|(i, _)| i == self.dibs_sel);
            f.render_widget(if on { line.reversed() } else { line }, row);
            if let Some((_, id)) = lease {
                self.hits.push((row, Act::Lease(id)));
            }
        }
    }

    fn draw_header(&mut self, f: &mut Frame, area: Rect, narrow: bool) {
        let mut first: Vec<(Span<'static>, Option<Act>)> = vec![];
        if !self.sidebar {
            first.push((" ☰ ".reversed(), Some(Act::Sidebar)));
            first.push((" ".into(), None));
            let n = self.running.len();
            first.push((if n > 0 { format!(" ⟳ {n} running ").reversed() } else { "nothing running".dim() }, Some(Act::Sidebar)));
            first.push(("  ".into(), None));
        }
        let Some(it) = self.current() else {
            first.push(("no targets here".dim(), None));
            self.strip(f, Rect { height: 1, ..area }, first);
            return;
        };
        let p = self.pinned().or_else(|| self.proc_of(&it)).cloned();
        let mut info = vec![it.name.clone().bold(), " ".into()];
        match &p {
            Some(p) => {
                info.push(badge(p));
                info.push(format!("  {}", who(text(&p["holder"]), &p["agent"].as_str().map(Into::into))).into());
                let since = p["since"].as_u64().unwrap_or_default();
                info.push(match p["ended_at"].as_u64() {
                    Some(end) => format!("  took {}", ago(end.saturating_sub(since))).dim(),
                    None => format!("  up {}", ago(now_ms().saturating_sub(since))).dim(),
                });
                if p["state"] == "queued" {
                    let udid = &p["device"]["udid"];
                    let lease = self.status["leases"].as_array().into_iter().flatten().find(|l| &l["device"]["udid"] == udid);
                    let behind = lease.map_or(String::new(), |l| format!(" behind {}", who(text(&l["holder"]), &l["agent"].as_str().map(Into::into))));
                    info.push(format!("  queued for {}{behind}", text(&p["device"]["name"])).yellow());
                }
            }
            None => info.push("not running".dim()),
        }
        if !it.local {
            info.push(format!("  {}", it.dir.display()).dim());
        }
        first.extend(info.into_iter().map(|s| (s, None)));
        self.strip(f, Rect { height: 1, ..area }, first);

        let running = p.as_ref().is_some_and(|p| matches!(text(&p["state"]), "running" | "building" | "queued"));
        let has_port = p.as_ref().is_some_and(|p| p["port"].is_u64());
        let button = |icon: &str, word: &str, key: &str| {
            let label = if narrow { format!(" {icon} ") } else { format!(" {icon} {word} ({key}) ") };
            Span::styled(label, Style::new().reversed())
        };
        let mut buttons = vec![];
        let marked = self.marked();
        if !marked.is_empty() {
            buttons.push((button("▶", &format!("run {} marked", marked.len()), "r"), Some(Act::Run(marked))));
            buttons.push((" ".into(), None));
        } else if p.as_ref().is_none_or(|p| p["kind"] != "exec") {
            let (icon, word) = match (running, self.only_builds(&it)) {
                (true, _) => ("⟳", "restart"),
                (false, true) => ("▶", "build"),
                (false, false) => ("▶", "run"),
            };
            buttons.push((button(icon, word, "r"), Some(Act::Run(vec![]))));
            buttons.push((" ".into(), None));
        }
        let failed = self.failed_ids();
        if !failed.is_empty() && !running {
            buttons.push((button("↻", "failed", "F"), Some(Act::Run(failed))));
            buttons.push((" ".into(), None));
        }
        if running {
            buttons.push((button("■", "stop", "s"), Some(Act::Stop)));
            buttons.push((" ".into(), None));
        }
        if has_port || running && p.as_ref().is_some_and(|p| p["simulator"].is_object()) {
            buttons.push((button("↗", "open", "o"), Some(Act::Open)));
            buttons.push((" ".into(), None));
        }
        buttons.push((button("⚑", "mark", "m"), Some(Act::Mark)));
        if p.as_ref().is_some_and(|p| p["kind"] == "test") {
            let on = self.report_on();
            buttons.push(("  ".into(), None));
            buttons.push(chip("logs", !on, Act::View(false)));
            buttons.push(chip("report", on, Act::View(true)));
            if !narrow {
                buttons.push((" (t)".dim(), None));
            }
        }
        self.strip(f, Rect { y: area.y + 1, height: 1, ..area }, buttons);
        if let Some((_, cur)) = self.sim_choice().filter(|_| area.height > 2) {
            self.draw_sim(f, Rect { y: area.y + 2, height: 1, ..area }, narrow, &cur, p.as_ref());
        }
    }

    /// The runtime and device fields (each opens its list), then notes; `p` is the target's latest process, for `running:`.
    /// iOS's: `sims` lists iOS runtimes and device types only; a second `eco::Platform` would give it a `platform` param.
    fn draw_sim(&mut self, f: &mut Frame, area: Rect, narrow: bool, cur: &Value, p: Option<&Value>) {
        let mut spans: Vec<(Span<'static>, Option<Act>)> = if narrow { vec![] } else { vec![("sim  ".dim(), None)] };
        let ph = &cur["physical"];
        match &self.runtimes {
            _ if ph.is_object() => {
                spans.push((format!("{} ▾", text(&ph["name"])).into(), Some(Act::Edit(Input::Runtime))));
                spans.push((format!("  {} · {}", text(&ph["model"]), text(&ph["state"])).into(), None));
            }
            Some(rts) if rts.is_empty() || cur.is_null() => {
                let pick = (!self.physical.is_empty()).then_some(Act::Edit(Input::Runtime));
                spans.push((format!("no iOS runtimes installed{}", if pick.is_some() { " ▾" } else { "" }).dim(), pick));
            }
            None if cur.is_null() => spans.push(("loading simulators…".dim(), None)),
            _ => {
                let rt = text(&cur["runtime_name"]);
                let rt = if narrow { rt.trim_start_matches("iOS ") } else { rt };
                let field = if cur["installed"] == false { format!("{rt} (not installed) ▾").red() } else { format!("{rt} ▾").into() };
                spans.push((field, Some(Act::Edit(Input::Runtime))));
                spans.push(("  ".into(), None));
                spans.push((format!("{} ▾", text(&cur["device_name"])).into(), Some(Act::Edit(Input::Device))));
            }
        }
        if cur["default"] == true && !ph.is_object() {
            spans.push((" default".dim(), None));
        }
        if cur["template"] == true && !ph.is_object() {
            spans.push((" · template".dim(), None));
        }
        let live = p.filter(|p| matches!(text(&p["state"]), "running" | "building" | "queued"));
        let (sim, dev) = (live.map(|p| &p["simulator"]), live.map(|p| &p["device"]));
        if let Some(d) = dev.filter(|d| d.is_object() && d["udid"] != ph["udid"]) {
            spans.push((format!(" · running: {}", text(&d["name"])).dim(), None));
        } else if let Some(s) = sim.filter(|s| s.is_object() && (ph.is_object() || s["device"] != cur["device_name"] || s["runtime"] != cur["runtime_name"])) {
            spans.push((format!(" · running: {} · {}", text(&s["device"]), text(&s["runtime"]).trim_start_matches("iOS ")).dim(), None));
        }
        // What runs, apart from the stored setting below: its device's verified state.
        if sim.is_some_and(|s| s["slim"].is_array()) {
            spans.push((" · running slim".dim(), None));
        }
        if !cur.is_null() && !ph.is_object() {
            let on = cur["head"] == true;
            spans.push(("  head".dim(), None));
            spans.push(chip("off", !on, Act::Head(false)));
            spans.push(chip("on", on, Act::Head(true)));
            let slim = cur["slim"].as_array();
            spans.push(("  slim".dim(), None));
            spans.push(chip("off", slim.is_none(), Act::Slim(false)));
            spans.push(chip("on", slim.is_some(), Act::Slim(true)));
            if let Some(keep) = slim.filter(|k| !k.is_empty()) {
                let keep: Vec<&str> = keep.iter().filter_map(Value::as_str).collect();
                spans.push((format!(" except {}", keep.join(",")).dim(), None));
            }
        }
        if !narrow {
            spans.push((if ph.is_object() { "   (i)" } else { "   (d / i / h)" }.dim(), None));
        }
        self.strip(f, area, spans);
    }

    fn draw_chips(&mut self, f: &mut Frame, area: Rect, narrow: bool) {
        let caret = |i| if self.input == Some(i) { "▏" } else { "" };
        let mut spans: Vec<(Span<'static>, Option<Act>)> = vec![
            ("filter ".dim(), Some(Act::Edit(Input::Filter))),
            (format!("{}{} ", self.filter, caret(Input::Filter)).into(), Some(Act::Edit(Input::Filter))),
            (" search ".dim(), Some(Act::Edit(Input::Search))),
            (format!("{}{} ", self.search, caret(Input::Search)).into(), Some(Act::Edit(Input::Search))),
        ];
        let mut chips: Vec<(Span<'static>, Option<Act>)> = vec![("level".dim(), None)];
        for (label, l) in [("all", None), ("err", Some(Level::Error)), ("warn", Some(Level::Warn)), ("info", Some(Level::Info))] {
            chips.push(chip(label, self.level == l, Act::Level(l)));
        }
        chips.push((" source".dim(), None));
        for (label, s) in [("all", None), ("build", Some(Source::Build)), ("out", Some(Source::Stdout)), ("err", Some(Source::Stderr))] {
            chips.push(chip(label, self.source == s, Act::Source(s)));
        }
        chips.push(("  ".into(), None));
        chips.push((" meta ▾ ".reversed(), Some(Act::Edit(Input::Meta))));
        if narrow {
            self.strip(f, Rect { height: 1, ..area }, spans);
            self.strip(f, Rect { y: area.y + 1, height: 1, ..area }, chips);
        } else {
            spans.push(("  ".into(), None));
            spans.extend(chips);
            self.strip(f, area, spans);
        }
    }

    fn draw_logs(&mut self, f: &mut Frame, area: Rect) {
        let mut title = self.current().map_or("logs".into(), |i| i.name);
        let focused = self.focus == Focus::Logs;
        let block = Block::bordered().border_style(if focused { Style::new().cyan() } else { Style::new() });
        let inner = block.inner(area);
        let rows = self.rows(inner.width as usize);
        self.page = inner.height as usize;
        self.scroll = self.scroll.min(rows.len().saturating_sub(self.page));
        // The cursor follows the window when g/G or paging move it.
        let shown = rows.len().min(self.scroll + self.page);
        self.cursor = self.cursor.clamp(self.scroll, shown.saturating_sub(1).max(self.scroll));
        if self.scroll > 0 {
            title += &format!(" ↑{} (G to follow)", self.scroll);
        }
        f.render_widget(block.title(title), area);
        let end = rows.len() - self.scroll;
        let start = end.saturating_sub(self.page);
        let cur = rows.len().checked_sub(self.cursor + 1).and_then(|j| rows.get(j)).map(|(_, i)| *i);
        for (k, (line, i)) in rows[start..end].iter().enumerate() {
            let row = Rect::new(inner.x, inner.y + k as u16, inner.width, 1);
            let from_bottom = rows.len() - 1 - (start + k);
            let line = if focused && cur == Some(*i) { line.clone().on_dark_gray() } else { line.clone() };
            f.render_widget(line, row);
            self.hits.push((row, Act::Cursor(from_bottom)));
            if self.view[*i].as_ref().is_some_and(|r| r.line.contains('\n')) {
                self.hits.push((row, Act::Toggle(*i)));
            }
        }
    }

    /// The shown test run's report in the log area: its totals (live while it runs), its failures (each opens its
    /// logs), and its slowest tests when it has durations. The window follows the selected failure.
    fn draw_report(&mut self, f: &mut Frame, area: Rect) {
        let Some(p) = self.shown().cloned() else { return };
        let focused = self.focus == Focus::Logs;
        let block = Block::bordered().title(format!("{} report", text(&p["target"]))).border_style(if focused { Style::new().cyan() } else { Style::new() });
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.page = inner.height as usize;
        let (id, t, now) = (text(&p["id"]).to_string(), &p["tests"], now_ms());
        let n = |k: &str| t[k].as_u64().unwrap_or_default();
        let since = p["since"].as_u64().unwrap_or(now);
        let mut rows: Vec<(Line<'static>, Option<Act>)> = vec![];
        rows.push(match p["ended_at"].as_u64() {
            None => (format!("⟳ {} done · {} failed so far · {}", n("passed") + n("failed"), n("failed"), ago(now.saturating_sub(since))).yellow().into(), None),
            Some(_) if p["built"] == false => (Line::from("build failed — enter for its errors").red(), Some(Act::Failure(id.clone(), None))),
            Some(_) if t.is_null() => (Line::from("no test results: see the logs (t)").dim(), None),
            Some(end) => {
                let took = t["duration_ms"].as_u64().map_or(String::new(), |ms| format!(" · {}", secs(ms)));
                let ended = format!("{took} · ended {} ago", ago(now.saturating_sub(end)));
                (Line::from(vec![format!("✗ {}  ", n("failed")).red(), format!("✓ {}  ", n("passed")).green(), format!("⊘ {}", n("skipped")).dim(), ended.dim()]), None)
            }
        });
        let failures = self.report_failures();
        self.report_sel = self.report_sel.min(failures.len().saturating_sub(1));
        let mut at = 0;
        for (i, t) in failures.iter().enumerate() {
            let fl = &t["failure"];
            let open = Some(Act::Failure(text(&t["process"]).into(), fl["cursor"].as_str().map(String::from)));
            let took = t["duration_ms"].as_u64().map_or(String::new(), |ms| format!("  {}", secs(ms)));
            if i == self.report_sel {
                at = rows.len() + 1;
            }
            let name = Line::from(vec![format!("✗ {}", text_form(text(&t["name"]))).red(), took.dim()]);
            rows.push((if focused && i == self.report_sel { name.reversed() } else { name }, open.clone()));
            let line = fl["line"].as_u64().map_or(String::new(), |l| format!(":{l}"));
            let loc = fl["file"].as_str().map_or(String::new(), |file| format!("{}{line}  ", Path::new(file).file_name().unwrap_or_default().to_string_lossy()));
            rows.push((Line::from(format!("  {loc}{}", text_form(text(&fl["message"])).replace('\n', " "))).dim(), open));
        }
        let mut timed: Vec<&Value> = self.report_data.iter().filter(|(r, _)| *r == id).flat_map(|(_, v)| v["tests"].as_array().into_iter().flatten()).filter(|t| t["duration_ms"].is_u64()).collect();
        if !timed.is_empty() {
            timed.sort_by_key(|t| std::cmp::Reverse(t["duration_ms"].as_u64()));
            rows.push((Line::from("slowest").dim().italic(), None));
            for t in timed.into_iter().take(5) {
                rows.push((Line::from(format!("  {:>7}  {}", secs(t["duration_ms"].as_u64().unwrap_or_default()), text_form(text(&t["name"])))).dim(), None));
            }
        }
        let h = inner.height as usize;
        let first = (at + 1).saturating_sub(h);
        for (k, (line, act)) in rows.into_iter().enumerate().skip(first).take(h) {
            let r = Rect::new(inner.x, inner.y + (k - first) as u16, inner.width, 1);
            f.render_widget(line, r);
            if let Some(a) = act {
                self.hits.push((r, a));
            }
        }
    }

    /// The metadata toggles, as a popup under the `meta` chip. Drawn last, like `draw_list`.
    fn draw_meta(&mut self, f: &mut Frame, bottom_y: u16) {
        if self.input != Some(Input::Meta) {
            return;
        }
        let Some(anchor) = self.hits.iter().find(|(_, a)| matches!(a, Act::Edit(Input::Meta))).map(|(r, _)| *r) else {
            self.input = None;
            return;
        };
        let screen = f.area();
        let (w, h) = (16u16.min(screen.width), (META.len() as u16 + 2).min(bottom_y.saturating_sub(anchor.y + 1)));
        if h < 3 {
            self.input = None;
            self.msg = "pane too short for the metadata list".into();
            return;
        }
        let area = Rect::new(anchor.x.min(screen.right().saturating_sub(w)), anchor.y + 1, w, h);
        f.render_widget(ratatui::widgets::Clear, area);
        let block = Block::bordered().title(" metadata ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.hits.push((screen, Act::Cancel));
        let on = [self.meta.time, self.meta.level, self.meta.source, self.meta.pid];
        for (i, label) in META.iter().enumerate().take(inner.height as usize) {
            let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
            let line = Line::from(format!(" [{}] {label:<width$}", if on[i] { "✓" } else { " " }, width = inner.width as usize));
            f.render_widget(if i == self.pick { line.reversed() } else { line }, row);
            self.hits.push((row, Act::Meta(i)));
        }
    }

    fn draw_bottom(&mut self, f: &mut Frame, area: Rect) {
        let key = |s: &str| Span::styled(format!(" {s} "), Style::new().reversed());
        let spans: Vec<(Span<'static>, Option<Act>)> = match &self.prompt {
            Some(Prompt::Take(it, by, only)) => vec![
                (format!("{} is held by {by}. Take it over? ", it.name).yellow(), None),
                (key("y take over"), Some(Act::Take(only.clone()))),
                (" ".into(), None),
                (key("n cancel"), Some(Act::Cancel)),
            ],
            Some(Prompt::Lease(id)) => vec![
                (format!("dibs {id}: ").into(), None),
                (key("x release"), Some(Act::Release(*id))),
                (" ".into(), None),
                (key("n renew 2h"), Some(Act::Renew(*id))),
                (" ".into(), None),
                (key("esc cancel"), Some(Act::Cancel)),
            ],
            None => {
                let sim = match self.sim_choice() {
                    Some((_, c)) if c["physical"].is_object() => "i destination  ",
                    Some(_) => "d device  i runtime  h head  ",
                    None => "",
                };
                let exec = self.shown_exec();
                let run = match (exec, self.current().is_some_and(|it| self.only_builds(&it))) {
                    (true, _) => "",
                    (false, true) => "r build  ",
                    (false, false) => "r run  ",
                };
                let z = if self.sidebar { "z hide sidebar" } else { "z sidebar" };
                let help = match self.input {
                    Some(Input::Runtime) => "↑/↓ move  enter pick  esc close".into(),
                    Some(Input::Device) => "type to filter  ↑/↓ move  enter pick  esc close".into(),
                    Some(Input::Meta) => "↑/↓ move  space toggle  esc close".into(),
                    _ if self.focus == Focus::Logs && self.report_on() => {
                        let r = if self.report_failures().is_empty() { run } else { "r rerun it  F rerun failed  " };
                        format!("↑/↓ failure  enter open in logs  {r}t logs  s stop  {z}  q quit")
                    }
                    _ if self.focus == Focus::Logs && !self.sidebar => {
                        format!("↑/↓ line  enter expand  {z}  1-5 tabs  {run}s stop  v meta  n/N match  g/G top/end  q quit")
                    }
                    _ if self.focus == Focus::Logs => format!("↑/↓ line  enter expand  tab next  esc back  {run}s stop  {z}  v meta  n/N match  g/G top/end  q quit"),
                    _ if self.focus == Focus::Running => {
                        format!("↑/↓ select  ←/→ section  tab next  1-5 tabs  {run}s stop  o open  {z}  f filter  / search  v meta  q quit")
                    }
                    _ if self.focus == Focus::History => format!("↑/↓ run  ←/→ section  tab next  1-5 tabs  {run}{z}  f filter  / search  v meta  q quit"),
                    _ if self.focus == Focus::Dibs => format!("↑/↓ select  enter release/renew  tab next  1-5 tabs  {z}  q quit"),
                    _ if self.focus == Focus::Problems => format!("↑/↓ move  ←/→ fold  enter edit  b all/new  w kind  u rescan  tab next  1-5 tabs  {z}  q quit"),
                    _ if self.focus == Focus::Tests => {
                        let r = if self.marked().is_empty() { "r run it" } else { "r run marked  esc unmark" };
                        format!("↑/↓ select  ←/→ fold  enter fold/open failure  space mark  {r}  F rerun failed  t report  tab next  1-5 tabs  {z}  q quit")
                    }
                    _ => format!("↑/↓ select  tab next  1-5 tabs  {run}s stop  o open  {sim}{z}  f filter  / search  v meta  n/N match  [/] marks  e expand  c clear  q quit"),
                };
                let msg = (!self.msg.is_empty()).then(|| (format!("{}  │  ", self.msg).into(), None));
                let stale = crate::STALE.lock().unwrap().as_ref().map(|w| (format!("{w}  │  ").yellow(), None));
                stale.into_iter().chain(msg).chain([(help.dim(), None)]).collect()
            }
        };
        self.strip(f, area, spans);
    }
}

/// The first and last of the log rows (top down) that show the same entry as row `at`; `entries` holds each row's view
/// entry, as `rows()` tags them.
fn record_span(entries: &[usize], at: usize) -> (usize, usize) {
    let same = |k: &usize| entries[*k] == entries[at];
    let first = (0..=at).rev().take_while(same).last().unwrap_or(at);
    let last = (at..entries.len()).take_while(same).last().unwrap_or(at);
    (first, last)
}

/// The log scroll (rows from the bottom) that brings a record's first row into a window of `page` rows, and the whole
/// record when it fits, scrolling no further than it needs; `top` and `bottom` are its first and last rows, from the
/// bottom too.
fn reveal(scroll: usize, page: usize, top: usize, bottom: usize) -> usize {
    let fits = top - bottom < page;
    if top >= scroll + page || (top < scroll && !fits) {
        top + 1 - page
    } else if bottom < scroll && fits {
        bottom
    } else {
        scroll
    }
}

/// The first of `h` rows to show so row `at` is in view, scrolling no further than it needs; `header[i]` marks section
/// headers. Back one row when that row is a header and `at` still fits, so a section's first item keeps its header.
fn window(header: &[bool], at: usize, h: usize) -> usize {
    let first = (at + 1).saturating_sub(h);
    if first > 0 && header[first - 1] && at + 1 < first + h { first - 1 } else { first }
}

/// Whether row `i` of rows in sections `secs` starts a section with a header: each non-empty one, except `yours` when
/// it is all there is.
fn heads(secs: &[&str], i: usize) -> bool {
    let first = i == 0 || secs[i - 1] != secs[i];
    first && secs.iter().any(|s| *s != "yours")
}

fn path_of(t: &Value) -> Vec<&str> {
    t["path"].as_array().into_iter().flatten().filter_map(Value::as_str).collect()
}

/// The group keys (`TreeRow::key`) above each failed test in `target`'s results `v`, the target's own included.
fn failing_groups(target: &str, v: &Value) -> Vec<String> {
    let sep = v["sep"].as_str().unwrap_or("/");
    let mut keys = vec![];
    for t in v["tests"].as_array().into_iter().flatten().filter(|t| t["status"] == "failed") {
        let p = path_of(t);
        keys.push(target.to_string());
        keys.extend((1..p.len()).map(|k| format!("{target}\t{}", p[..k].join(sep))));
    }
    keys
}

/// The problems tab's rows from `problems`' `items`, those shown (`new_only`, `kind`): each file, then while it is
/// unfolded its problems by line. Files with errors first, then those with new problems, then by count and path. A
/// file starts unfolded when it has an error or a new problem; `toggled` holds the files folded the other way.
fn problem_rows(items: &[Value], new_only: bool, kind: Option<&str>, toggled: &HashSet<String>) -> Vec<TreeRow> {
    let mut files: std::collections::BTreeMap<&str, Vec<&Value>> = Default::default();
    for i in items.iter().filter(|i| (!new_only || i["new"] == true) && kind.is_none_or(|k| i["kind"] == k)) {
        files.entry(i["path"].as_str().unwrap_or_default()).or_default().push(i);
    }
    let mut files: Vec<_> = files.into_iter().collect();
    let errors = |ps: &[&Value]| ps.iter().filter(|p| p["kind"] == "error").count();
    let new = |ps: &[&Value]| ps.iter().any(|p| p["new"] == true);
    files.sort_by_key(|(path, ps)| (errors(ps) == 0, !new(ps), std::cmp::Reverse(ps.len()), *path));
    let mut rows = vec![];
    for (path, mut ps) in files {
        let key = format!("problems\t{path}");
        let unfolded = (errors(&ps) > 0 || new(&ps)) != toggled.contains(&key);
        rows.push(TreeRow { depth: 0, key, label: path.into(), item: None, failed: errors(&ps), total: ps.len() });
        if unfolded {
            ps.sort_by_key(|p| p["line"].as_u64());
            for (k, p) in ps.into_iter().enumerate() {
                let label = p["message"].as_str().unwrap_or_default().into();
                rows.push(TreeRow { depth: 1, key: format!("problems\t{path}\t{k}"), label, item: Some(p.clone()), failed: usize::from(p["kind"] == "error"), total: 1 });
            }
        }
    }
    rows
}

/// `target`'s rows given its results `v` (`tests`' target form, null for none): its own, then while it is in `open`
/// its groups and tests, in result order; a level whose tests all go on into one group is one row with it.
fn tree(target: &str, v: &Value, open: &HashSet<String>) -> Vec<TreeRow> {
    let tests: Vec<&Value> = v["tests"].as_array().into_iter().flatten().collect();
    let failed = tests.iter().filter(|t| t["status"] == "failed").count();
    let mut rows = vec![TreeRow { depth: 0, key: target.into(), label: target.into(), item: None, failed, total: tests.len() }];
    if open.contains(target) {
        level(target, v["sep"].as_str().unwrap_or("/"), &tests, 0, 1, open, &mut rows);
    }
    rows
}

/// `tree`'s rows for `tests`, which share their path up to `at`.
// ponytail: quadratic in a level's tests; index by path component if a suite of thousands shows lag.
fn level(target: &str, sep: &str, tests: &[&Value], at: usize, depth: usize, open: &HashSet<String>, rows: &mut Vec<TreeRow>) {
    let mut names: Vec<&str> = vec![];
    for t in tests {
        if let Some(n) = path_of(t).get(at).filter(|n| !names.contains(n)) {
            names.push(n);
        }
    }
    for name in names {
        // A test named like a group (cargo's `foo` beside `foo::bar`) is a row of its own before it.
        let (leaves, ms): (Vec<&Value>, Vec<&Value>) = tests.iter().copied().filter(|t| path_of(t).get(at) == Some(&name)).partition(|t| path_of(t).len() == at + 1);
        for t in leaves {
            let failed = usize::from(t["status"] == "failed");
            let label = text_form(t["name"].as_str().unwrap_or(name));
            rows.push(TreeRow { depth, key: format!("{target}\t{}", path_of(t).join(sep)), label, item: Some(t.clone()), failed, total: 1 });
        }
        let Some(&first) = ms.first() else { continue };
        let failed = ms.iter().filter(|t| t["status"] == "failed").count();
        let p = path_of(first);
        let mut end = at;
        while ms.iter().all(|t| path_of(t).len() > end + 2 && path_of(t)[end + 1] == p[end + 1]) {
            end += 1;
        }
        let key = format!("{target}\t{}", p[..=end].join(sep));
        let open_now = open.contains(&key);
        rows.push(TreeRow { depth, key, label: p[at..=end].join(sep), item: None, failed, total: ms.len() });
        if open_now {
            level(target, sep, &ms, end + 1, depth + 1, open, rows);
        }
    }
}

fn chip(label: &str, on: bool, a: Act) -> (Span<'static>, Option<Act>) {
    let s = format!(" {label} ");
    (if on { s.reversed() } else { s.into() }, Some(a))
}

/// A history row's outcome and when it ended, then (dim) how long it ran: `✓ 4m ago`, `✗ 65 2h ago`, `⏱ 1h ago`
/// timed out; ` · 1m12s`. Last, so a narrow row cuts the duration first.
fn run_badge(r: &Value, now: u64) -> [Span<'static>; 2] {
    let end = r["ended_at"].as_u64().unwrap_or(now);
    let when = format!("{} ago", ago(now.saturating_sub(end)));
    let outcome = match (text(&r["state"]), r["code"].as_i64(), r["signal"].as_i64()) {
        ("exited", ..) => format!("✓ {when}").green(),
        ("timed-out", ..) => format!("⏱ {when}").red(),
        (_, Some(c), _) => format!("✗ {c} {when}").red(),
        (_, None, Some(s)) => format!("✗ sig {s} {when}").red(),
        (state, None, None) => format!("✗ {state} {when}").red(),
    };
    [outcome, format!(" · {}", ago(end.saturating_sub(r["since"].as_u64().unwrap_or(end)))).dim()]
}

fn badge(p: &Value) -> Span<'static> {
    let state = text(&p["state"]);
    let code = match (p["code"].as_i64(), p["signal"].as_i64()) {
        (Some(c), _) => format!("({c})"),
        (None, Some(s)) => format!("(sig {s})"),
        (None, None) => String::new(),
    };
    let port = p["port"].as_u64().map_or(String::new(), |n| format!(" :{n}"));
    let sim = p["simulator"]["name"].as_str().or(p["device"]["name"].as_str()).map_or(String::new(), |n| format!(" {n}"));
    let lock = if p["blocked"] == true { " · waiting for a lock" } else { "" };
    let s = format!("[{state}{code}{port}{sim}{lock}]");
    match state {
        "running" => s.green(),
        "building" | "queued" => s.yellow(),
        "crashed" | "timed-out" => s.red(),
        _ => s.dim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_group_by_file_errors_and_new_first() {
        let p = |path: &str, kind: &str, line: u64, new: bool| json!({"path": path, "kind": kind, "line": line, "message": "m", "new": new});
        let items = [p("b.swift", "warning", 9, false), p("b.swift", "warning", 2, false), p("c.swift", "warning", 1, true), p("a.swift", "error", 5, false), p("d.swift", "todo", 3, false)];
        let shown = |rows: Vec<TreeRow>| rows.iter().map(|r| (r.depth, r.label.clone(), r.item.as_ref().map(|i| i["line"].clone()))).collect::<Vec<_>>();
        let none = HashSet::new();
        // Errors first, then new ones, then by count: unfolded where there's an error or something new.
        let rows = shown(problem_rows(&items, false, None, &none));
        let files: Vec<_> = rows.iter().filter(|r| r.0 == 0).map(|r| r.1.as_str()).collect();
        assert_eq!(files, ["a.swift", "c.swift", "b.swift", "d.swift"]);
        assert_eq!(rows.len(), 4 + 2, "a's and c's problems shown; b and d folded");
        // Unfolded by hand: by line.
        let toggled = HashSet::from(["problems\tb.swift".to_string()]);
        let b: Vec<_> = shown(problem_rows(&items, false, None, &toggled)).into_iter().filter(|r| r.0 == 1).map(|r| r.2).collect();
        assert_eq!(b, [Some(json!(5)), Some(json!(1)), Some(json!(2)), Some(json!(9))]);
        assert_eq!(shown(problem_rows(&items, true, None, &none)).len(), 2, "new only: c and its one");
        assert_eq!(shown(problem_rows(&items, false, Some("todo"), &toggled)).iter().filter(|r| r.0 == 0).count(), 1);
        assert_eq!(elide("Sources/Feed/Parsing/FeedParser.swift", 20), "Source…dParser.swift");
        assert_eq!(elide("short.swift", 20), "short.swift");
    }

    #[test]
    fn the_log_cursor_moves_and_shows_whole_records() {
        // Rows top down, by view entry: a (3 rows: wrapped, expanded, metadata), a mark (1), b (2).
        let entries = [0, 0, 0, 1, 2, 2];
        assert_eq!(record_span(&entries, 1), (0, 2));
        assert_eq!(record_span(&entries, 3), (3, 3), "a mark is an entry of its own");
        assert_eq!(record_span(&entries, 5), (4, 5));
        // From the bottom: a's rows are 5..=3, the mark 2, b 1..=0. A 2-row window.
        assert_eq!(reveal(0, 2, 2, 2), 1, "the mark, just above the window");
        assert_eq!(reveal(0, 3, 5, 3), 3, "a fits: all of it, at the bottom of the window");
        assert_eq!(reveal(0, 2, 5, 3), 4, "a doesn't fit: its first row on top");
        assert_eq!(reveal(4, 2, 5, 3), 4, "its first row already shows: no move");
        assert_eq!(reveal(4, 2, 1, 0), 0, "b, below the window");
        assert_eq!(reveal(0, 2, 1, 0), 0, "in view already");
    }

    #[test]
    fn the_target_window_follows_the_selection() {
        // header, a, b, header, c, d
        let header = [true, false, false, true, false, false];
        assert_eq!(window(&header, 1, 3), 0, "fits from the top");
        assert_eq!(window(&header, 5, 3), 3, "the last item, with its header above it");
        assert_eq!(window(&header, 4, 3), 2, "c's header stays: b, header, c");
        assert_eq!(window(&header, 4, 1), 4, "one row: only the selection");
    }

    #[test]
    fn a_history_row_says_when_a_run_ended_and_how_long_it_took() {
        let min = 60_000;
        let text = |r: Value| run_badge(&r, 10 * min).iter().map(|s| s.content.to_string()).collect::<String>();
        assert_eq!(text(json!({"state": "exited", "since": 4 * min, "ended_at": 6 * min})), "✓ 4m ago · 2m");
        assert_eq!(text(json!({"state": "crashed", "code": 65, "since": 0, "ended_at": 9 * min + 48_000})), "✗ 65 12s ago · 9m");
        assert_eq!(run_badge(&json!({"state": "exited", "since": 0, "ended_at": 0}), 0)[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn clock_is_local_time_of_day() {
        // 2026-10-29 09:35:27.012 UTC
        let ms = 1_793_266_527_012;
        assert_eq!(clock(ms, 0), "09:35:27.012");
        assert_eq!(clock(ms, 3600), "10:35:27.012");
        assert_eq!(clock(ms, -10 * 3600), "23:35:27.012");
    }

    #[test]
    fn wrap_splits_long_lines_and_keeps_span_styles() {
        let line = Line::from(vec![Span::styled("abcde", Style::new().red()), "fgh".dim()]);
        let rows = wrap(line, 3);
        assert_eq!(rows.iter().map(line_text).collect::<Vec<_>>(), ["abc", "def", "gh"]);
        assert_eq!(rows[1].spans[0].style, Style::new().red());
        assert_eq!(rows[1].spans[1].style, Style::new().dim());
        assert_eq!(wrap(Line::from("ab"), 3).len(), 1);
        // By display width: `✅` is one char but two cells, so it doesn't fit after `ab` in 3.
        let rows = wrap(Line::from("ab✅cd"), 3);
        assert_eq!(rows.iter().map(line_text).collect::<Vec<_>>(), ["ab", "✅c", "d"]);
    }

    #[test]
    fn an_emoji_presentation_glyph_logs_in_its_one_cell_form() {
        let mut app = App::new(mpsc::channel().0, "/w".into());
        app.meta = Meta { time: false, level: false, source: false, pid: false };
        let line = "2026-10-29 16:04:10.946737+0100 Demo_Dev[41709:12037739] [system] ℹ\u{fe0f} INFO: [nav-check] syncToolbar ✅ done";
        let r = Record { ts: 0, source: Source::Stdout, level: Level::Info, line: line.into() };
        for width in [20, 33, 47, 60] {
            let rows = app.record_rows(0, &r, width);
            assert!(rows.iter().all(|l| l.width() <= width && !line_text(l).contains('\u{fe0f}')), "{width}: {rows:?}");
            assert!(rows.iter().map(line_text).collect::<String>().contains("ℹ INFO"));
        }
    }

    #[test]
    fn rows_split_into_yours_agents_and_elsewhere() {
        let root = Path::new("/w/repo");
        let it = |dir: &str, local: bool| Item { name: "x".into(), dir: dir.into(), local };
        let p = |holder: &str, agent: Option<&str>, state: &str| json!({"holder": holder, "agent": agent, "state": state});
        let s = |it: &Item, p: Option<Value>| section(root, it, p.as_ref());
        // A target: idle or the human's run is yours, an agent's live run is the agents', and it returns once ended.
        assert_eq!(s(&it("/w/repo", true), None), "yours");
        assert_eq!(s(&it("/w/repo", true), Some(p("w1:p1", None, "running"))), "yours", "a pane without an agent");
        assert_eq!(s(&it("/w/repo", true), Some(p("host:42", None, "running"))), "yours", "a shell outside herdr");
        assert_eq!(s(&it("/w/repo", true), Some(p("w1:p2", Some("claude"), "building"))), "agents");
        assert_eq!(s(&it("/w/repo", true), Some(p("claude:s1", None, "running"))), "agents");
        assert_eq!(s(&it("/w/repo", true), Some(p("claude:s1", None, "exited"))), "yours");
        // Processes that aren't targets: by holder inside the worktree (any subdir), else elsewhere.
        assert_eq!(s(&it("/w/repo/crates/a", false), Some(p("claude:s1", None, "exited"))), "agents");
        assert_eq!(s(&it("/w/repo", false), Some(p("w1:p1", None, "running"))), "yours");
        assert_eq!(s(&it("/w/repo-2", false), Some(p("claude:s1", None, "running"))), "elsewhere", "a sibling worktree");
        assert_eq!(s(&it("/w/other", false), Some(p("w1:p1", None, "running"))), "elsewhere");
        // A dir reached through a symlink is in the worktree it resolves into, either way round.
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/sections");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("repo/sub")).unwrap();
        std::os::unix::fs::symlink(tmp.join("repo"), tmp.join("link")).unwrap();
        let via = |dir: &Path| Item { dir: dir.into(), ..it("", false) };
        let mine = Some(p("w1:p1", None, "running"));
        assert_eq!(section(&tmp.join("repo"), &via(&tmp.join("link/sub")), mine.as_ref()), "yours");
        // `App` resolves its root once; a root reached through the link resolves to the repo.
        let link_root = std::fs::canonicalize(tmp.join("link")).unwrap();
        assert_eq!(section(&link_root, &via(&tmp.join("repo/sub")), mine.as_ref()), "yours");
        assert_eq!(section(&link_root, &via(&tmp.join("link/sub")), mine.as_ref()), "yours");
        assert_eq!(section(&link_root, &via(&tmp), mine.as_ref()), "elsewhere");
    }

    #[test]
    fn running_targets_and_history_split_live_from_ended() {
        let mut app = App::new(mpsc::channel().0, "/w/repo".into());
        let target = |name: &str| {
            let t = json!({"name": name, "ecosystem": "cargo", "dir": "/w/repo", "kind": "binary", "package": null, "needs_port": false, "port": null, "target_dir": null});
            serde_json::from_value::<Target>(t).unwrap()
        };
        app.local = vec![target("api"), target("web")];
        let p = |id: &str, target: &str, dir: &str, kind: &str, holder: &str, ended: Option<u64>| {
            let state = if ended.is_some() { "exited" } else { "running" };
            json!({"id": id, "target": target, "dir": dir, "kind": kind, "holder": holder, "agent": null, "state": state, "since": id.parse::<u64>().unwrap(), "ended_at": ended})
        };
        app.status = json!({"processes": [
            p("1", "api", "/w/repo", "binary", "w1:p1", None),
            p("2", "srv", "/w/other", "binary", "w1:p1", None),
            p("3", "web", "/w/repo", "binary", "claude:s1", None),
            p("4", "check", "/w/repo", "exec", "w1:p1", None),
            // Ended this session: still in `status`, not yet in `history`.
            p("5", "web", "/w/repo", "binary", "w1:p1", Some(6)),
            p("6", "check", "/w/repo", "exec", "w1:p1", Some(7)),
        ]});
        app.history = vec![p("0", "api", "/w/repo", "binary", "w1:p1", Some(1))];
        app.rebuild_items();
        let running: Vec<_> = app.running.iter().map(|(s, p)| (*s, text(&p["id"]))).collect();
        assert_eq!(running, [("yours", "1"), ("yours", "4"), ("agents", "3"), ("other projects", "2")]);
        assert_eq!(app.items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["api", "web"]);
        assert_eq!(app.runs.iter().map(|r| text(&r["id"])).collect::<Vec<_>>(), ["6", "5", "0"]);
        // A pinned process that ends stays pinned, found in `status`; then in `history` once a rerun drops it from `status`.
        app.run = Some("6".into());
        assert_eq!(app.pinned().map(|p| text(&p["id"])), Some("6"));
        let six = app.status["processes"][5].clone();
        app.status["processes"].as_array_mut().unwrap().pop();
        app.history.push(six);
        app.rebuild_items();
        assert_eq!(app.pinned().map(|p| text(&p["id"])), Some("6"));
        // Found nowhere: unpinned, so the view falls back to the selected target.
        app.history.pop();
        app.rebuild_items();
        assert_eq!(app.run, None);
    }

    #[test]
    fn the_test_tree_unfolds_only_what_holds_a_failure() {
        let t = |id: &str, status: &str| json!({"id": id, "path": id.split("::").collect::<Vec<_>>(), "name": id.rsplit("::").next(), "status": status});
        let v = json!({"sep": "::", "tests": [t("a::x::one", "failed"), t("a::x::two", "passed"), t("b::three", "passed"), t("b::four", "passed"), t("top", "passed")]});
        let open: HashSet<String> = failing_groups("t", &v).into_iter().collect();
        let rows = tree("t", &v, &open);
        let shown: Vec<_> = rows.iter().map(|r| (r.depth, r.label.as_str(), r.failed, r.total)).collect();
        // `a` holds only `x`, so they are one row; `b` stays folded.
        assert_eq!(shown, [(0, "t", 1, 5), (1, "a::x", 1, 2), (2, "one", 1, 1), (2, "two", 0, 1), (1, "b", 0, 2), (1, "top", 0, 1)]);
        assert_eq!(rows[2].item.as_ref().map(|t| &t["id"]), Some(&json!("a::x::one")));
        assert_eq!(tree("t", &v, &HashSet::new()).len(), 1, "a folded target is its row");
        assert_eq!(tree("t", &Value::Null, &open).iter().map(|r| r.total).collect::<Vec<_>>(), [0], "never run");
        // A test whose path leads another's keeps a row of its own.
        let v = json!({"sep": "::", "tests": [t("foo", "passed"), t("foo::bar", "failed"), t("foo::baz", "passed")]});
        let open: HashSet<String> = failing_groups("t", &v).into_iter().collect();
        let shown: Vec<_> = tree("t", &v, &open).iter().map(|r| (r.depth, r.label.clone(), r.item.is_some())).collect();
        let want = [(0, "t", false), (1, "foo", true), (1, "foo", false), (2, "bar", true), (2, "baz", true)].map(|(d, l, t)| (d, l.to_string(), t));
        assert_eq!(shown, want);
        // What `r` and marks run: a test's id, a group's path with a trailing `sep`, nothing for the target.
        let mut app = App::new(mpsc::channel().0, "/w".into());
        app.results.insert("t".into(), ("r1".into(), v.clone()));
        let ids: Vec<_> = tree("t", &v, &open).iter().map(|r| app.row_id(r)).collect();
        assert_eq!(ids, [None, Some("foo".into()), Some("foo::".into()), Some("foo::bar".into()), Some("foo::baz".into())]);
    }

    #[test]
    fn metadata_goes_on_one_line_under_the_record() {
        let mut app = App::new(mpsc::channel().0, "/w".into());
        app.utc_offset = 0;
        app.meta = Meta { time: true, level: true, source: true, pid: false };
        // 2026-10-29 09:35:27.012 UTC
        let r = Record { ts: 1_793_266_527_012, source: Source::Stderr, level: Level::Error, line: "abcdef".into() };
        let rows = app.record_rows(0, &r, 4);
        assert_eq!(rows.iter().map(line_text).collect::<Vec<_>>(), ["abcd", "ef", "  09:35:27.012 · E · err"], "the text at full width");
        assert_eq!(rows[2].spans[3].style.fg, Some(Color::Red), "the level keeps its colour");
        app.meta = Meta { time: false, level: false, source: false, pid: false };
        assert_eq!(app.record_rows(0, &r, 4).len(), 2, "no line without columns");
    }

    #[test]
    fn a_grouped_diagnostic_shows_as_its_header_until_opened() {
        let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/swiftc-diagnostic.txt")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let block = crate::daemon::group(&lines, |h, l| crate::eco::Ecosystem::continues(&crate::eco::Xcode, h, l)).remove(0);
        let mut app = App::new(mpsc::channel().0, "/w".into());
        app.meta = Meta { time: false, level: true, source: false, pid: false };
        let r = Record { ts: 0, source: Source::Build, level: Level::Warn, line: block };
        let rows: Vec<String> = app.record_rows(0, &r, 400).iter().map(line_text).collect();
        assert_eq!(rows.len(), 2, "the header row, then the metadata under the whole record: {rows:#?}");
        assert!(rows[0].ends_with("nonisolated autoclosure ▸ +14 lines"), "{}", rows[0]);
        assert_eq!(rows[1].trim(), "W");
    }

    #[test]
    fn tui_json_keeps_meta_and_sidebar() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/tui/tui.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        assert!(load_ui(&path).1, "shown without a file");
        save_ui(&path, Meta { pid: true, ..Meta::default() }, false).unwrap();
        let (meta, sidebar) = load_ui(&path);
        assert!(meta.pid && !sidebar);
        save_ui(&path, Meta { time: false, ..meta }, sidebar).unwrap();
        let (meta, sidebar) = load_ui(&path);
        assert!(meta.pid && !meta.time && !sidebar, "a meta save keeps the sidebar");
        // A file from before the sidebar holds only `meta`.
        std::fs::write(&path, r#"{"meta":{"pid":true}}"#).unwrap();
        let (meta, sidebar) = load_ui(&path);
        assert!(meta.pid && sidebar);
    }
}
