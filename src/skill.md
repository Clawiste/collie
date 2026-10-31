---
name: collie
description: "Run, watch, test and stop project processes (dev servers, binaries, iOS apps, one-off builds, checks and test commands) shared with a human and other agents, and call dibs on ports, simulators, devices and throwaway Postgres/Redis databases. Use when you need to start a project process or command, read its logs, wait for it to be ready or finished, or hold a port, simulator, device or a database."
---

# collie — project cockpit for a human and coding agents

`collie` runs project processes inside one shared daemon so a human and several agents can
see and use the same running server without starting duplicates or fighting over ports.
In herdr, the human watches everything in the Collie pane.

What it hosts: dev servers, binaries and iOS apps (`collie run <target>`), and one-off
commands such as builds, lints and test runs (`collie exec`, or a task declared in
`collie.toml`; see "Exec and tasks").

herdr is optional. Your identity (the "holder") depends on where you run:

- In herdr (`HERDR_ENV=1`), the holder is your herdr pane (`HERDR_PANE_ID`). When your pane
  closes, everything you started or called dibs on is stopped and released.
- Outside herdr, the holder is your Claude Code session (`claude:<session id>`), or whatever
  `COLLIE_HOLDER` pins it to; otherwise the shell you call `collie` from. Every agent of one
  session, subagents too, shares that holder: always name the target on `stop`, and check
  `collie status` before `run`, since a teammate's process restarts instead of failing `held`.
  To work separately, set `COLLIE_HOLDER=<name>` on each command. Nothing is released for you:
  dibs last until you `dibs release` them or their 2h TTL runs out, which also stops a process
  holding them (a port, simulator or device). Other processes run until you `stop` them.

## Learn the CLI

The installed binary is the authority. Start with `collie --help`, then `collie <verb> --help`.
Every verb takes `--json`; streams print NDJSON. Errors print
`{"error":{"code":..,"message":..}}` on stderr and exit 1.

A `{"warning":..}` line on stderr saying the daemon runs an older build means features in this
skill may be missing from it. Tell the user; never run `collie daemon stop` yourself: it stops
every process the daemon runs, other agents' and the user's included.

```bash
collie targets --json            # what can run here, and what each needs (e.g. a port)
collie run <target> --json       # call dibs, build, launch; prints process id, `leases`, port
collie status --json             # every process and dibs, and who holds it
collie stop [<target>]           # default: everything you hold
```

## Read logs without re-reading

Take a cursor before you act, then read only what happened after it:

```bash
cursor=$(collie logs --mark <target>)
# ... make a request, run a test, send a prompt ...
collie logs --since "$cursor" --json            # new records, then a {"cursor":..} line
collie logs --since "$cursor" --level warn      # only warnings and errors
```

Always `logs --mark` before a step and `logs --since` after it. Carry the returned cursor
forward to the next `--since`.

A cursor belongs to one run. If the target has been restarted since you took it, `logs --since`
still returns that old run's records, and warns on stderr:
`warning: cursor is for ended run <X>; latest is <Y> (…)`; with `--json` the warning is a
`{"warning": …}` line. Take a new `--mark` after a restart. A malformed cursor fails with
`bad_request`.

`logs` reads a target's latest run: its last 200 records. `--all` reads every record of the run.
Earlier runs stay readable for up to 7 days (the oldest go sooner once all logs pass 500 MB; each
target's latest run is always kept):

```bash
collie logs --all --level warn <target>   # the whole latest run, warnings and errors
collie history [<target>] --json          # ended runs in this worktree, newest first; --all for every worktree
collie logs --run <id> --level error      # an earlier run's log, by the id `history` lists
```

A Swift compiler diagnostic is one record with its source excerpt and notes, so `logs --level warn`
returns each warning with its context.

## Wait, don't poll

```bash
collie wait --until built     --timeout 300000 <target>
collie wait --until port-open --timeout 60000  <target>   # server accepts connections
collie wait --until exited    --timeout 540000 <target>
```

`wait` fails fast with code `ended` if the process exits before reaching the state; read
the logs then.

Wait in the foreground. Never put a build or test in the background and end your turn: nothing
wakes you when it finishes. A Bash call is capped at 600000 ms, so keep every `wait --timeout` at
540000 or less, and when `wait` fails with code `timeout`, run the same `wait` again. A `wait`
timing out never stops the process. This replaces `sleep` and `grep` loops over the logs.

On a timeout `wait` exits 1 and also prints one line to stdout saying where the process is:
`<id> still running (4m elapsed, last: <its last log line>); it keeps running: wait again`
(`{"still": …}` with `--json`). To wait for several processes, chain them:
`collie wait --until exited --timeout 540000 a && collie wait --until exited --timeout 540000 b`.

Two different timeouts — don't mix them up:

| | what it limits | when it runs out |
|---|---|---|
| `collie wait --timeout MS` | how long *you* wait | `wait` fails with code `timeout`; the process keeps running |
| `collie run/exec --timeout MS` | how long the *process's run phase* may run (not its build) | collie **kills** it; it ends `timed-out` |

The kill limit is off by default, except for test targets: 30 minutes. `--timeout 0` turns it off.
A `timed-out` process is not `crashed` (`wait --until crashed` doesn't match it; `--until exited`
does), its last log record says it was killed, and a simulator it ran on is deleted, so the next
run gets a fresh one. Use a kill limit for anything that can hang, e.g. `collie exec --timeout 600000 …`.

Every process result (`run`, `exec`, `wait`, `status --json`) carries `since` and `ended_at`
(Unix ms; `ended_at` is null while it runs), so a duration is `ended_at - since`. A process killed
by a signal ends `crashed` with `code` null and `signal` set (e.g. 9); a timed-out one usually has
`signal` 15 (collie's TERM; 9 if it ignored that for 3 s). A run ended by `stop`, a takeover or a
rerun is recorded in `history` as `crashed` with `signal` 15.

## Sharing with others

- `run` on a target someone else is running fails with code `held` and names the holder.
  Use their running process (read its logs, hit its port) instead of starting another.
- Do not pass `--take` to stop another holder's process unless the user asked you to.
- Need a port for something collie does not run? `collie dibs acquire port --json`, and
  `collie dibs release <id>` when done. Dibs expire after 2h unless you `collie dibs renew <id>`.

## Exec and tasks

`collie exec` runs any command as a collie process: in your current directory, with your
environment, logged and shown in the Collie pane like a target. Use it for builds, checks and
test runs instead of running them in your own shell.

```bash
collie exec --json --name check -- cargo check --all --locked   # collie's flags go before `--`
collie wait --until exited --timeout 540000 check   # repeat while it fails with code `timeout`
collie logs --level warn check
```

- The result of `wait --until exited` is the exit: `state` is `exited` (code 0) or `crashed`
  with the command's `code`.
- Always pass `--name`. Without it the name is the command's first two words (`cargo check`),
  so two different `cargo check …` commands would replace each other. Your rerun under the same
  name stops your old process; another holder's running process with that name fails with
  `held`. Processes with different names run side by side, also in the same worktree.
- Name it `<action>` or `<action>-<subject>`, so the pane's running block and history read alike:
  - `<action>` is the closest of `build`, `test`, `lint`, `typecheck`, `format`, `generate`, `migrate`,
    `bench`, `probe` (one-off diagnostics), `script`, in lowercase;
  - `<subject>` is what it acts on (a scheme, test target, package, crate or module), spelled as the
    project spells it; leave it out for the whole project.
  - Never your role or round (`review-`, `prepr-`, `2`), the ticket, branch, worktree, date or attempt:
    the pane and history show those already.
  - Rerun under the same name, so history groups the attempts; a variant changes the subject, never a
    suffix (`test-ListCoreTests`, not `test-ListCoreTests-2`).
  - E.g. `build-Demo-Dev`, `test-DemoNavigationTests`, `test` (the whole suite), `lint`, `typecheck`,
    `generate` (tuist or openapi), `generate-api`, `migrate`, `probe-nav-stack`. Tasks in `collie.toml`
    keep their own names.
- Environment passes through, so set what the command needs in front of `collie`:
  `CARGO_TARGET_DIR=.tmp/x RUSTFLAGS=… collie exec --name build -- cargo build --workspace --timings`.
- Every process runs with `PYTHONUNBUFFERED=1` and `NSUnbufferedIO=YES` unless your environment sets
  them, so Python and Foundation output arrives line by line. C programs using stdio still buffer
  their output to a pipe.
- A cargo build waiting on a lock another build holds (`Blocking waiting for file lock on …`)
  logs it as `warn` and shows `"blocked": true` in `status --json` until it prints anything
  else. Give your build its own `CARGO_TARGET_DIR` instead of waiting for someone else's.
  (yarn's and xcodebuild's lock lines are matched too, but untested.)
- Lines that report a failed test (e.g. `test x ... FAILED` from cargo, `not ok` from node:test,
  `FAILED` from pytest, `--- FAIL:` from go test) are `error` records:
  `collie logs --level error <name>` lists the failures.
- The Collie pane lists a live `exec` run under "running" and an ended one in its history, not
  with the targets. It can stop one but not rerun it. Rerun it with `collie exec`.

A repo can declare its usual commands as tasks in `collie.toml`, at or above where you run collie:

```toml
[tasks.lint]
cmd = "cargo clippy --all-targets -- -D warnings"
```

Tasks are targets (`collie targets` lists them with `kind: "task"`): `collie run lint`, then
`wait`/`logs`/`stop` by name; the pane can rerun them. A task runs in the directory of its
`collie.toml`. If that file doesn't parse, or a task has the name of a detected target,
`targets` and `run` fail with code `bad_config`, naming the file and the problem: fix the file.

A repo whose project files are generated (Tuist, XcodeGen, CocoaPods) declares how to bring
them up to date:

```toml
[prepare]
cmd = "tuist generate --no-open"
watch = ["Project.swift", "Tuist", "Projects", "Package.swift", "Package.resolved"]
```

Before any target of that repo builds, collie runs `cmd` in the `collie.toml`'s directory when it
never succeeded there or a `watch` path changed since it last did (a directory counts every file
in it, so adding or removing one counts); with no `watch`, before every build. Its output is the
run's first `build` lines. If it fails, the run ends `crashed` without building. In a fresh
worktree, where nothing is generated yet, `collie run <scheme>` runs it first, then finds the
scheme; a failure there is code `prepare_failed` with the output's last lines. Don't generate the
project yourself before `collie run`; `collie exec` commands skip `[prepare]`.

`collie run <target> -- <args>` appends args to what the run starts: for a task, to the end of
its `cmd` line; for a binary or npm script, to its command line. Builds don't get them (except
`cargo test`'s, see "Tests"), and iOS apps refuse them (`bad_request`).

## Problems

`collie problems` lists this worktree's build warnings, errors and lint (from its targets' builds,
not `exec` runs) and its `TODO`/`FIXME`/`HACK` comments, one per line in the compiler's shape
(`path:line:col: warning: …  [target]`). It's display only: it never fails, and whether a warning
matters is the project's call, not collie's.

```bash
collie problems --new          # only what this branch added: on lines added since it left the default branch
collie problems --rescan       # scan the TODOs again (otherwise again only after a target's run ends)
collie problems --json         # items with their sources (target, run, log cursor), totals, the base branch
```

- A file's warnings stay until it's rebuilt: an incremental Xcode build doesn't repeat the warnings
  of files it didn't compile, so a no-op build doesn't clear them. A warning that went away because
  another file changed stays until its own file changes.
- `new` compares with `origin/HEAD` (else `main`/`master`), uncommitted edits included.
- An untracked file is invisible to both `new` and the TODO scan until you `git add` it.

## Tests

Cargo workspaces and npm packages have test targets, found with no config: `collie targets --json`
lists them with `kind: "test"` (`cargo test` per cargo workspace; the `test` and `test:*` scripts
of a package.json).

```bash
collie run "cargo test" --json                      # everything `cargo test` runs at the workspace root
collie run "cargo test" -- -p alpha parses_dates    # args go to `cargo test`: a package, a filter
collie run test -- --test-name-pattern=parses       # npm: args go to the script
collie wait --until exited --timeout 540000 "cargo test"   # repeat while it fails with code `timeout`
collie logs --level error "cargo test"              # the failing tests, panics with file:line, the summary
```

- `exited` (code 0) means the tests passed; `crashed` with a code means some failed (or didn't
  compile: then the build's `error` records say why, and `built` is false).
- For `cargo test`, the args narrow the build as well as the run: `-- -p alpha` compiles only
  alpha's tests. `-- --doc` runs only the doc tests; the build step leaves it out.
- For cargo, `wait --until built` returns once the tests are compiled. Compiler warnings come
  once, from the build; the run doesn't repeat them.
- Arguments for the test binaries go after a second `--`: `collie run "cargo test" -- -- --nocapture`.
- Use `collie run "cargo test"`, not `collie exec -- cargo test …` at the workspace root: that exec's
  default name is also `cargo test`, so it replaces the target's process.

### Test results

A cargo or Xcode test run's process (`run`, `wait`, `status`, `history`) carries
`"tests": {"passed", "failed", "skipped", "duration_ms"}`. While it runs these are counted from its
output and approximate. When it ends they are the parsed results. `duration_ms` is the sum of the
tests' durations, and null for cargo, which prints none. `tests` is null for other runners (npm),
for other kinds of target, and for a run whose build failed.

```bash
collie tests "cargo test"          # per test, its newest result across the target's runs here
collie tests --run <id> --json     # one run's results and totals (ids from `collie history`)
```

"Here" is the target in the dir nearest above yours, so a crate's own `cargo test` and the
workspace's are kept apart. A test is no longer listed once the newest full run (one without `--only`)
lacks it, unless an `--only` run since has it.

`--json` gives `{"sep", "tests": [{"id", "path", "name", "status", "duration_ms", "failure",
"process"}]}`, with `process` and `totals` too for `--run`:
- `id` is what the runner selects: `tests::parses` (cargo), `ListCoreTests/<Suite>/<test>()` (Xcode,
  as `-only-testing:` takes it).
- `path` is `id` split on `sep`. `name` is the test's own name, a Swift Testing display name included.
- `failure` is null or `{"message", "file", "line", "cursor"}`. Pass `cursor` to `collie logs --since`
  to read the logs from the failure on. It is null when no log record names the test.
- With no results, the code is `not_found`.

To run some tests only, pass ids from `collie tests` to `--only` (repeatable):

```bash
collie run "cargo test" --only tests::parses --only tests::dates::   # a test, and a group
collie run ListCoreTests --only ListCoreTests/ListTests/loads() --only ListCoreTests/CacheTests/
```

- A test's id runs exactly that test. A group's id is its path with a trailing `sep`: `tests::dates::`
  (cargo), `ListCoreTests/CacheTests/` (Xcode, a suite or class).
- Xcode: each id becomes `-only-testing:<id>`, with the trailing `/` dropped.
- Cargo: the ids are libtest filters after the first `--` in the args (one is added if there is none),
  with `--exact` unless an id is a group. Libtest filters loosely:
  - a filter applies in every test binary and to the doc tests;
  - a group matches as a substring, so `tests::` also runs `other::tests::x`;
  - `--exact` covers all filters, so with a group among the ids, the test ids match by substring too.
- npm and other targets refuse `--only` with `bad_request`.
- `collie tests` merges such a run into the target's list. Its process shows `"only": true` (in `status`,
  `history`, `wait`), so its `tests` totals count only the tests it ran. A stopped or timed-out run's
  results never remove tests from the list either.

### Xcode tests

Every scheme with tests (shared, or your own in `xcuserdata`, where Tuist puts the `…Tests` schemes)
is a `kind: "test"` target; an app scheme's own tests are `"<scheme> (test)"`. They run on a simulator
like an app (`sim`), building with `build-for-testing` and running `test-without-building`.

```bash
collie run ListCoreTests --runtime 26.5 --device "iPhone 17 Pro Max" --json
collie wait --until exited --timeout 540000 ListCoreTests   # repeat while it fails with code `timeout`
collie logs --level error ListCoreTests                     # failing tests and their file:line
collie run ListCoreTests --runtime 26.5 --device "iPhone 17 Pro Max" -- "-only-testing:ListCoreTests/<Suite>/<test>()"
```

- When a project's snapshot tests are recorded on one simulator kind, always pin it (`--runtime 26.5 --device "iPhone 17 Pro Max"` above).
- Builds may run in parallel, and each run gets its own simulator: nothing to gate on. Your next test
  run in the same worktree (any test target, same simulator kind) reuses the simulator the last one
  ended on, still booted, so 26 test schemes don't mean 26 simulators or boots. Only a physical
  device is one at a time: a run on a busy one queues (see "Physical devices").
- When you're done testing, `collie stop <test target>` (or `collie stop`): an ended test run keeps its
  simulator booted until a stop, its pane closing or the 2h TTL, and outside herdr no pane closes.
- Args after `--` reach `test-without-building` (e.g. `-only-testing:`), not the build.
- A target whose stored destination is a physical device tests there, which installs an XCTest
  runner app on that device; only the user chooses that, never you.
- Use `collie run <scheme>`. When a run must mirror CI, `collie exec --sim --name <name> -- tuist test <scheme> --
  -destination id={udid} -collect-test-diagnostics never` is fine, but `tuist test` regenerates that
  worktree's workspace: never while another run is building in the same worktree.

### Commands on a simulator or device of their own

A command that needs a destination (`xcodebuild test`, `tuist test`, a script) leases one with
`collie exec`, or `collie run <task>`. Don't share a named simulator, and don't gate on
`pgrep xcodebuild`: each `--sim` run gets a fresh `collie <id>` simulator, booted before the command.
`{udid}` in the command becomes its UDID, and `$COLLIE_UDID` holds it for scripts and task lines.

```bash
collie exec --sim --runtime 27.0 --device "iPhone 18 Pro" --name unit -- xcodebuild test -workspace W -scheme S -destination id={udid} -collect-test-diagnostics never
collie exec --sim --name ci-test -- tuist test S -- -destination id={udid} -collect-test-diagnostics never
collie exec --physical "Test iPhone" --name device-test -- xcodebuild test … -destination id={udid} -collect-test-diagnostics never   # queues while someone holds it
```

- Always pass `-collect-test-diagnostics never` to `xcodebuild test`: otherwise a failing run spends up to
  10 minutes collecting a `simctl diagnose` before it ends.

- Always pass `--name`: two `xcodebuild test` execs with the default name replace each other.
- A rerun of the same `--name` gets the same simulator back, still booted. `collie stop <name>` when you're
  done releases it; otherwise it stays booted until your pane closes or the 2h TTL.
- `{udid}` without `--sim` or `--physical` fails with `bad_request`. `--sim`/`--physical` are for exec and
  tasks only; an Xcode target's destination is its own (`sim`).
- Each test may run 600 s at most before xcodebuild fails it as hung, and a whole run 30 minutes
  before collie kills it (`timed-out`, simulator replaced); `--timeout MS` on `run` changes that.

## Databases

For schema, migration or query work, call dibs on a throwaway Postgres or Redis in Docker, and
use its URL. Each dib is its own fresh container on a free port, bound to 127.0.0.1.

```bash
collie dibs acquire postgres --json                  # postgres:17; --image pgvector/pgvector:pg15 for another
collie dibs acquire redis --json                     # redis:7
# the result's service.url, e.g. postgres://collie:collie@127.0.0.1:3001/collie or redis://127.0.0.1:3002
DATABASE_URL=<service.url> collie exec --name migrate -- diesel migration run
collie dibs release <id>                             # deletes the container and its data
```

- The call returns once the database accepts connections. The first use of an image pulls it,
  which can take minutes: `--timeout MS` (default 120000) bounds the pull and then the start. On
  code `timeout` while pulling, the pull goes on: acquire again a little later.
- Code `no_docker`: Docker isn't running. Ask the user to start Docker Desktop; collie never starts it.
- They are dibs, not part of a `run`: they stay until you release them, your herdr pane closes,
  or their 2h TTL runs out (`collie dibs renew <id>`), and survive a daemon restart or
  `collie daemon stop`. Release them when done; their data is gone with the container.
- `--preferred` and `--wait` don't apply (`bad_request`); every acquire starts a new container.

## iOS apps (Xcode schemes)

Schemes that launch an `.app` show up in `collie targets` with `needs: [{"kind":"simulator"}]`
and where they run (`sim`: a simulator kind, plus `sim.physical` when a physical device is chosen;
see "Physical devices"). Scheme names often contain spaces: quote them.

Framework and module schemes (no `.app` to launch, no tests) are `kind: "build"` targets: `collie run
<Scheme>` just builds them, for a generic simulator destination with no device, and ends `exited` when
the build succeeds. That's the way to compile one module on its own.

```bash
collie run "MyApp DEV" --json                          # result has simulator.udid, or device.udid on a physical device
collie wait --until launched --timeout 540000 "MyApp DEV"   # build + first boot take minutes: repeat on `timeout`
```

- Each run calls dibs on its own simulator device (`collie <n>`), created fresh and deleted when you
  let go (stop, pane close, the app exiting, `dibs release`, TTL). A fresh device starts with
  empty app data, so the app is signed out (see "Signing in"), unless the target has a template
  (see "Simulator templates"). Running the same target again from
  the same pane (outside herdr: the same session), while
  the simulator kind is unchanged, keeps the device booted and the app's data, and only reinstalls
  and relaunches. Another holder taking the target over (`--take`) retires your device.
- Need a different simulator for one run: `collie run "<scheme>" --device "iPhone 17 Pro" --runtime 26.5`.
  This doesn't change what the target runs on next time. Use `collie sims set "<scheme>" --device … --runtime …`
  only when the user asks to change it; it applies to every worktree of the repo. `collie sims` lists what is installed.
- The app's `Logger` output and stdout are the target's logs; build warnings and errors are `warn`/`error` records.
- On a simulator a crash isn't reported as `crashed`: `simctl launch` exits 0 when the app dies. Read the logs.

### Driving the app

collie builds, installs, launches and logs. On a simulator, taps, typing and screenshots go through the Xcode MCP:

1. Take your UDID from `collie run --json` or `collie status --json` (`simulator.udid`) and only ever
   pass a UDID you hold.
2. `DeviceInteractionStartSession` with `deviceIdentifier: <udid>`, not
   `DeviceInteractionStartWorkspaceSession` or `DeviceInteractionInstallAndRun`: collie already
   built and installed the app, and an MCP install would fight it.
3. `DeviceInteractionSynthesize` to tap, type and capture.
4. `DeviceInteractionEndSession` before you `collie stop` or release your dibs.

The MCP doesn't accept every runtime (it has refused iOS 26.5 devices while accepting iOS 27.0
ones). If it refuses your UDID, its error lists the eligible devices: run on their runtime
(e.g. `--runtime 27.0`). Never pick one of the listed devices that isn't yours.

A screenshot without the MCP: `xcrun simctl io <udid> screenshot shot.png`.

Simulators run headless: collie opens the device in DeviceHub only when the target's `head` is on. You don't need it;
never turn it on (`collie sims set … --head on`) unless the user asks. DeviceHub keeps a device's window after
collie deletes the device (it offers no way to close it from outside); close it with ⌘W.

Signing in: type test credentials through the MCP. If the app reads credentials from its
environment, set `SIMCTL_CHILD_<NAME>=…` in the environment of `collie run`; `simctl launch`
passes it to the app as `<NAME>`. Never echo credentials into a command line or a log.

### Slim simulators

A slim simulator ([simslim](https://github.com/MobAI-App/simslim)) has about 180 background daemons
switched off and uses about half the memory. Each target stores whether its simulators are slim, for
every worktree of the repo; nothing stored means stock. **Run with the target's stored setting**: pass
neither flag unless the run needs a feature slimming switches off.

```bash
collie sims set "<scheme>" --slim on                  # store: slim (or off, or the categories to keep)
collie sims set "<scheme>" --slim icloud,web,store    # store: slim except those (ids: `simslim profiles`)
collie run "<scheme>" --stock                         # this run stock, whatever is stored (= --slim=off)
collie run "<scheme>" --slim=icloud                   # this run slim except iCloud
collie exec --sim --slim --name test-X -- …           # an exec's --sim simulator, slimmed
```

Run `--stock`, or keep the category (`--slim=<ids>`), when the run needs:
- login, which keeps its tokens in the Keychain: `icloud`;
- purchases (StoreKit, RevenueCat) or push: `store`;
- universal links or deep links: `web`;
- widgets or Live Activities: `widgets`;
- a final manual check before a release: stock.

- A new slim simulator takes about a minute more: simslim reconfigures it and boots it once. A re-run
  keeps it while its setting matches. Before iOS 18.5 a slim device lasts only until it reboots, so
  each run there gets a new one.
- The `simulator` in `run`/`status --json` and in `history` carries `slim`: null for stock, else the
  categories it kept. collie sets it only after `simslim verify` passed.
- Templates are stock (a clone of a slim device comes out stock), so a slim run slims its clone too.
- Without simslim installed a slim run fails with `simslim_missing`: `brew install mobai-app/tap/simslim`.
  `slim_failed` means simslim failed: the run ends, its device is deleted (never reused as stock), and
  there's no fallback to stock — rerun, or run `--stock`. Physical devices refuse the flags.

### Simulator templates

A template is a starting point the user prepares (signed in, onboarding done) so every new
simulator of a target starts from it instead of empty. `collie targets --json` shows `sim.template: true`
when one exists, and `collie sims` lists them.

- Only make or delete one when the user asks:
  `collie sims template "<scheme>"` saves your current simulator of that target (it stops your
  process, clones the device, and replaces any older template); `collie sims template "<scheme>" --rm`
  deletes it. One per target, shared by every worktree of the repo. If saving fails, your simulator
  is kept as dibs (the error names the id); `collie dibs release <id>` when you no longer need it.
- A new simulator is cloned from the template only when the run's kind is the template's (same device
  and runtime, e.g. `--runtime 26.5 --device "iPhone 17 Pro Max"`). Otherwise it starts empty, and the
  run's log has a `warn` record saying so. A clone's first log record names its template.
- Clones copy the app's data (files, databases). Whether a keychain sign-in survives a clone is untested:
  if the app is signed out anyway, tell the user.
- Disk: a template of an app's simulator is about 1.4 GB, but clones share its blocks (about 10 MB each
  until they change).

### Physical devices

A target runs on a physical iPhone or iPad only when the human has chosen one for it:
`collie targets --json` then shows `sim.physical` (`{udid,name,model,os,state,developer_mode}`),
and `collie run` builds with signing, installs and launches there. `collie sims` lists the devices
and their `state` (`connected`, `disconnected`, `unavailable`).

- Never run `collie sims set … --physical` yourself unless the user asks: these are people's own
  devices. `--device`/`--runtime` on `sims set` switch the target back to a simulator.
- One holder at a time. A `collie run` or `exec` on a device someone else holds queues for it: it
  builds meanwhile, then shows state `queued` until the device is free, and runs then. `wait` waits
  through it, and its timeout line says `still queued for <device> behind <holder>`. `--no-queue`
  fails with `busy` instead; or run on a simulator this once (`collie run "<scheme>" --runtime 27.0`).
  `unavailable` means the device can't be used now (not reachable, not paired, Developer Mode
  off, or not listed): tell the user; don't retry in a loop.
- The first device build of a worktree is a full build plus signing; installing over Wi-Fi
  is slow: when `wait --timeout 540000` fails with `timeout`, repeat it.
- Logs: the app's `Logger` output, debug level included, arrives on `stderr` as plain text
  (`… MyApp[pid:tid] [category] message`), one record per message with its further lines. Its level
  is the level word leading the message (`DEBUG`, `INFO`, `WARN`, `ERROR`…, past `[tags]` and
  emoji), else `error` when it says error/fault/critical, else `info`, so `--level warn` and
  `--level error` work. Crash and abort lines (`Fatal error: …`, `Precondition failed: …`, an uncaught
  exception) are `error` records of their own.
- A crash shows: the process ends `crashed` (code 1) after devicectl's `stdout` line
  `App terminated due to signal 11.`. `collie stop` ends the app on the device.
- Letting go (stop, pane close, release, TTL, a failed build or launch) leaves the app
  installed with its data: the next holder may find it signed in.
- Untested: a locked phone may fail the install or launch. If a run ends `crashed`, read the
  logs and ask the user to unlock it.
- Don't drive a physical device through the Xcode MCP unless the user asks: it is untested
  with collie, and the MCP matches `deviceIdentifier` fuzzily, so a near miss lands on someone's
  phone. If asked, pass the full UDID from `collie status --json` (`device.udid`) and check which
  device the session reports.
- Environment: `DEVICECTL_CHILD_<NAME>=…` in `collie run`'s environment reaches the app as `<NAME>`.
