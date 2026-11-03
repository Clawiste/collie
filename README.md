# collie

**A project cockpit for a human and their coding agents.**

collie runs your project's processes — dev servers, binaries, iOS apps, builds, tests, lints — inside one shared
daemon. You and every agent working in the repo see the same running server, the same logs and the same test
results, instead of each starting a duplicate and fighting over ports, simulators and devices.

```console
$ collie targets
$ collie run web                                  # build, call dibs on a port, launch
$ collie wait --until port-open web
$ cursor=$(collie logs --mark web)
$ curl localhost:3000/health
$ collie logs --since "$cursor" --level warn      # only what happened since, only what matters
```

## Why

Put two agents and a human in one repo and you get three `npm run dev`s on three ports, a `cargo build` stuck on a
lock someone else holds, an agent `grep`-polling a log file in a `sleep` loop, and a simulator that someone else's
test run just erased.

collie is the shared layer underneath:

- **One process per target, owned by someone.** A second `run` fails with `held` and names the holder, so the
  agent uses the running server instead of starting another.
- **Dibs, not collisions.** Ports, simulators, physical devices and throwaway databases are leased to one holder
  and released when they're done (or after a 2h TTL).
- **Logs an agent can read cheaply.** Cursors (`--mark` / `--since`) return only what's new; levels are parsed from
  compiler, test-runner and app output, so `--level error` means something.
- **Wait, don't poll.** `collie wait --until built|launched|port-open|exited` blocks until the state is reached
  and fails fast if the process dies first.

## Features

- **Zero-config target detection** — Cargo workspaces (binaries, examples, `cargo test`), npm / pnpm / yarn / bun
  scripts, and Xcode schemes (apps, frameworks, test schemes).
- **`collie exec`** — run any one-off command as a logged, stoppable, shared process.
- **Tasks and prepare steps** in an optional `collie.toml` (e.g. `tuist generate` before a build, only when its
  inputs changed).
- **Test results** — per-test pass/fail for Cargo and Xcode, rerun just the failures with `--only`.
- **Problems** — build warnings, errors and `TODO`/`FIXME` for the worktree, or only what this branch added
  (`--new`).
- **iOS simulators** — a fresh simulator per holder, reused between runs, optionally
  [slimmed](https://github.com/MobAI-App/simslim) to half the memory, or cloned from a signed-in template.
- **Physical iPhones and iPads** — signed builds, install and launch, with a queue when someone else holds the
  device.
- **Throwaway Postgres and Redis** — one Docker container per dib, on a free port, deleted on release.
- **Build slots** — only so many builds, `exec` commands and test runs at once; fewer while the slot holders'
  processes use too much CPU or memory, with every build but one slowed to background priority (never below one).
  CI jobs on the same machine join the queue, ahead of agents.
- **Offloading** — with no slot free, tests, lints and framework builds run on another Mac over SSH (see
  [Offloading to another Mac](#offloading-to-another-mac)), mirrored locally.
- **History** — ended runs and their logs, kept 7 days.
- **A terminal UI** — targets, tests, history, problems and live logs in one pane (`collie tui`), standalone
  or inside herdr.
- **Built for agents** — every command takes `--json` (streams are NDJSON), errors carry stable codes, and
  `collie skill` prints a complete usage guide for an agent's context.

## Install

Requires Rust (edition 2024). macOS for the iOS features; the rest also runs on Linux.

```sh
git clone https://github.com/Clawiste/collie.git
cd collie
cargo install --path .
```

Optional extras:

| For | Install |
| --- | --- |
| iOS apps and tests | Xcode (`xcrun simctl`, `devicectl`) |
| Slim simulators | `brew install mobai-app/tap/simslim` |
| Postgres / Redis dibs | Docker Desktop |

### Teach your agent

```sh
mkdir -p ~/.claude/skills/collie
collie skill > ~/.claude/skills/collie/SKILL.md
```

`collie skill` prints the full agent guide (Markdown with skill front matter); point any other agent harness at the
same text.

## Quick start

```sh
cd your-project
collie targets                       # what can run here, and what each needs
collie run api                       # starts the daemon on first use
collie status                        # every process and dib, and who holds it
collie logs --follow api
collie stop api
```

One-off commands go through `exec` so they're shared and logged too:

```sh
collie exec --name lint -- cargo clippy --all-targets -- -D warnings
collie wait --until exited --timeout 540000 lint
collie logs --level warn lint
```

## Usage

### Logs without re-reading

```sh
cursor=$(collie logs --mark api)     # before a step
# … make a request, run a test …
collie logs --since "$cursor" --json # after it: new records, then the next cursor
collie logs --all --level warn api   # the whole latest run
collie history api                   # ended runs; read one with `collie logs --run <id>`
```

A Swift compiler diagnostic is a single record with its source excerpt and notes. Failed-test lines from cargo,
node:test, pytest and go test are `error` records.

### Waiting

```sh
collie wait --until built     --timeout 300000 api
collie wait --until port-open --timeout 60000  api
collie wait --until exited    --timeout 540000 test
```

`wait --timeout` only stops *waiting*; `run/exec --timeout` is a kill limit on the process itself.

### Tests

```sh
collie run "cargo test" -- -p alpha             # args reach cargo test
collie tests "cargo test"                       # newest result per test
collie run "cargo test" --only tests::parses    # one test, or a group (`tests::dates::`)
collie run ListCoreTests --runtime 26.5 --device "iPhone 17 Pro Max"
```

### iOS

```sh
collie run "MyApp DEV"                          # own simulator, build, install, launch
collie wait --until launched --timeout 540000 "MyApp DEV"
collie sims                                     # installed runtimes, device types, physical devices
collie sims set "MyApp DEV" --slim on           # store: slim simulators for this target
collie sims template "MyApp DEV"                # save your signed-in simulator as the starting point
collie exec --sim --name unit -- xcodebuild test … -destination id={udid}
```

Taps, typing and screenshots go through the Xcode MCP's `DeviceInteraction*` tools on the UDID collie gave you.

### Dibs

```sh
collie dibs acquire port --json
collie dibs acquire postgres --json            # result has service.url
collie dibs list
collie dibs renew <id>
collie dibs release <id>
```

## Configuration

Everything is detected; `collie.toml` (at or above where you run collie) only adds to it.

```toml
# Override a detected target
[targets.api]
port = 3000

# A command you run often, as a target of its own
[tasks.lint]
cmd = "cargo clippy --all-targets -- -D warnings"

# Bring generated project files up to date before any build, when these paths changed
[prepare]
cmd = "tuist generate --no-open"
watch = ["Project.swift", "Tuist", "Package.swift", "Package.resolved"]
```

Machine-wide limits live in `~/.config/collie/config.toml`, read again every 5 seconds, so an edit applies without
restarting the daemon:

```toml
[builds]
max = 2       # most builds at once; default: cores / 4, at least 1
cpu = 70      # % of all cores the slot holders' processes may use before the cap drops
memory = 50   # % of RAM
```

Over a threshold for 10 s lowers the cap by one (never below 1) and moves every build but one to background
priority (the oldest CI job keeps full speed, else the oldest build); 30 s under 80% of both raises it again.
`collie status` shows the cap, the queue and each process's CPU and memory, summed over its whole process tree.

| Environment variable | Effect |
| --- | --- |
| `COLLIE_HOLDER` | Who you are to collie, when you want separate ownership within one session |
| `COLLIE_STATE_DIR` | Isolated state, socket and logs (default `~/.local/state/collie`) |
| `SIMCTL_CHILD_<NAME>` / `DEVICECTL_CHILD_<NAME>` | Passed to the iOS app as `<NAME>` |

### CI on the same machine

A GitHub Actions self-hosted runner on a collie host can make each job hold a build slot, ahead of waiting agents,
with no workflow change: the runner's job hooks take the slot for the job's worker process and give it back.

```sh
# job-started.sh
export COLLIE_HOLDER=ci
/Users/<you>/.local/bin/collie dibs acquire build --wait --priority ci --pid "$PPID" \
  --name "$GITHUB_REPOSITORY#$GITHUB_RUN_NUMBER" || echo "collie: no build slot taken"
```

```sh
# job-completed.sh
COLLIE_HOLDER=ci /Users/<you>/.local/bin/collie dibs release --pid "$PPID" || true
```

In the runner's `.env`, then restart the runner service:

```
ACTIONS_RUNNER_HOOK_JOB_STARTED=/abs/path/job-started.sh
ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/abs/path/job-completed.sh
```

- The slot belongs to `$PPID`, the job's worker: its process tree counts toward the limits, and the slot is freed
  within about 5 s of it exiting (a cancelled job included), even if the completed hook never runs.
- `|| …`: a failing hook can fail the job, and collie being down must never break CI; the job then runs without a
  slot. A job waiting for a slot when the daemon restarts carries on without one too.
- Absolute paths: the runner's `PATH` may lack `~/.local/bin`.
- The runner must run as your own macOS user, with the same `HOME` (and `COLLIE_STATE_DIR`, if set): the socket is
  per user and state dir, and another account would get a daemon and a queue of its own. `collie status` from a hook
  shows which one it reached.
- Keep the daemon out of the runner's reach: started from a job, it would be the job's child, and the runner kills
  what a job leaves behind — the daemon, and every process it runs. So `collie` doesn't start a daemon inside a job
  (`RUNNER_TRACKING_ID` set); run it as a LaunchAgent instead, in `~/Library/LaunchAgents/dev.collie.daemon.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.collie.daemon</string>
  <key>ProgramArguments</key>
  <array><string>/Users/you/.local/bin/collie</string><string>daemon</string><string>--foreground</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict>
</plist>
```

`launchctl load ~/Library/LaunchAgents/dev.collie.daemon.plist` starts it (stop a running daemon first).

## Who owns what

Every request carries a **holder**:

- In herdr, your pane. Closing it stops and releases everything you started.
- In Claude Code, your session (`claude:<session id>`), shared by its subagents.
- Otherwise `COLLIE_HOLDER`, or the calling shell.

Dibs last until released or their 2h TTL, and a build dib until its process exits; processes run until stopped.
`--take` stops another holder's process and takes the target over, so use it only on purpose.

## herdr plugin

The repo includes a herdr plugin manifest (`herdr-plugin.toml`), not yet published to any plugin registry. It builds
the release binary, starts the daemon, and adds a **Collie** pane plus *Open Collie* and *Run current project*
actions. The pane is the human's view of what every agent is running.

## How it works

- A single daemon per user listens on a Unix socket in the state dir and owns every process, lease and log.
  `collie` commands are thin clients that start it on first use.
- Each process runs in its own process group, so stop and timeouts take its whole tree down.
- Logs live in `~/Library/Caches/collie/logs` (macOS) or `$XDG_CACHE_HOME/collie/logs`, pruned after 7 days or
  past 500 MB.
- Database dibs survive a daemon restart; processes don't. `collie daemon stop` stops everything it runs, for
  every holder.

## Offloading to another Mac

A second Mac running collie (a Mac mini build box, say) can take slot work off a laptop. When the laptop has no free
build slot and the mini does, collie copies the worktree there with rsync, starts the same run on the mini's daemon
over an SSH-forwarded socket, and mirrors it locally: state, logs (paths mapped back), test results, problems and the
exit. You and every agent keep using the one local `collie`; a process that runs there shows
`"remote": {"name": "macmini", "id": "<its id there>"}` in `status`. Processes already queued on the laptop move to the
mini as soon as it frees up (`id` is null while one moves).

On the laptop, in `~/.config/collie/config.toml`:

```toml
[remote]
host = "macmini"                                   # ssh destination
socket = "/Users/you/.local/state/collie/collie.sock"  # the mini daemon's socket, on the mini
[remote.paths]
"/Users/you/Projects" = "/Users/you/collie-mirror" # where the copies go there; one to one, absolute
```

The mirror dir on the mini must be collie's alone: never your own checkouts there. collie syncs with `--delete` and
deletes copies of worktrees you remove, so it marks each copy it makes (`.collie-copy`) and refuses to sync over or
delete a git checkout or a dir it didn't make.

`collie status` shows the mini's slots as `builds.remote` (`cap`, `running`, `queued`, `version`, and `error` when
it takes no work: unreachable, or another commit).

**What goes:** test targets (cargo, npm, Xcode test schemes on a simulator), Xcode framework `build` targets (their
build output stays on the mini), and tasks and `exec --name` commands named `test`, `lint` or `typecheck` (or
`test-…`, `lint-…`, `typecheck-…`). **What never goes:** apps and servers, physical devices, `exec --sim/--physical`,
anything else. Per run, `--local` keeps it here and `--remote` sends it there even with a free slot here (and fails if
it can't). Per target, in `collie.toml`:

```toml
[targets."cargo test"]
offload = false     # e.g. tests that hit a server on the laptop's localhost, or read the build's output here

[tasks.check]
cmd = "make check"
offload = true      # a task that may go whatever its name
```

**Setting up the mini:**

- The same collie, built from the **same commit** as the laptop's: every reply carries the daemon's commit, and a
  mismatch (or `unknown`, a build outside git) stops offloading with a warning in `builds.remote.error`.
- Its daemon as a LaunchAgent (see "CI on the same machine"), started through a login shell so the runs get your
  `PATH` and toolchain: `<array><string>/bin/zsh</string><string>-lc</string><string>exec ~/.local/bin/collie daemon
  --foreground</string></array>`. The laptop sends only the env that isn't machine-specific.
- The laptop's daemon runs `ssh` itself, often without an ssh agent (a LaunchAgent has no `SSH_AUTH_SOCK`): the key
  must load from `~/.ssh/config` (`IdentityFile`, `UseKeychain yes`), since it runs with `BatchMode`. Tailscale SSH may
  not forward Unix sockets; plain sshd over Tailscale does. Keep the socket paths under 104 bytes.
- rsync: macOS's own (openrsync) does. The copy gets what git tracks and what it doesn't ignore, never `.git`; what
  the tree ignores stays untouched on the mini (its `target/`, generated projects), so its builds stay incremental.
- For Xcode tests: the Xcode a project's `.xcode-version` names, and the simulator runtimes and device types its
  tests pin (snapshot suites). A run the mini can't do (a missing runtime) falls back to the laptop.
- Files git ignores but the build needs (`.env`, `Secrets.xcconfig`) aren't synced: put them on the mini once.

Not synced either: `.git`. Builds that read git themselves (a Cargo `build.rs` calling `git`, an Xcode run script
running `git describe`) see no repo on the mini: give them `offload = false`. Simulators on the mini start empty
(laptop templates don't carry over), and a test run reuses the simulator of the last one only on the same machine.

**Cleanup is automatic.** collie records each copy (`remote.json` in the state dir); within about 5 s of a worktree
disappearing on the laptop (`wt remove`, `git worktree remove`), the mini deletes its copy, its ended processes there
(and the simulators they kept) and the Xcode DerivedData built from it, unless something still runs there. It never
deletes a mapped prefix itself, `/`, a home dir, a git checkout or a dir without its `.collie-copy` marker.

## Development

```sh
cargo build
cargo test          # integration tests in tests/cli.rs use fixtures in tests/fixtures
```

Run your dev build against an isolated daemon so it doesn't disturb the one you're using:

```sh
COLLIE_STATE_DIR=.tmp/state cargo run -- status
```

## License

Not yet licensed.
