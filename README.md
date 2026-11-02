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

| Environment variable | Effect |
| --- | --- |
| `COLLIE_HOLDER` | Who you are to collie, when you want separate ownership within one session |
| `COLLIE_STATE_DIR` | Isolated state, socket and logs (default `~/.local/state/collie`) |
| `SIMCTL_CHILD_<NAME>` / `DEVICECTL_CHILD_<NAME>` | Passed to the iOS app as `<NAME>` |

## Who owns what

Every request carries a **holder**:

- In herdr, your pane. Closing it stops and releases everything you started.
- In Claude Code, your session (`claude:<session id>`), shared by its subagents.
- Otherwise `COLLIE_HOLDER`, or the calling shell.

Dibs last until released or their 2h TTL; processes run until stopped. `--take` stops another holder's process and
takes the target over, so use it only on purpose.

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

## Roadmap

Ideas being explored, not built yet:

- **Remote execution** — run the daemon on another Mac (a Mac mini build box) and drive it from a laptop over SSH,
  with logs and status streaming back.
- **CI as a collie client** — CI jobs join the same queue as agents, with priority.
- **Build pressure limits** — cap concurrent builds by collie's own CPU and memory use, never below one.

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
