# Keitora — Logistics Dispatch

> A dispatch control room for your Claude Code worktrees.


## What is Keitora?

Keitora is a cross-platform (macOS-first) desktop app that unifies **task tracking** and the **live
Claude Code terminal sessions** running across a multi-worktree repo — plus an **advisory dispatcher**
that matches open tasks to idle worktrees.

If you run several Claude Code sessions at once — one per git worktree — you know the pain: a scatter
of terminal tabs, no shared view of who's working on what, and no easy way to hand the next task to
whichever session is free. Keitora replaces that with a single window.

It's a **dispatch control room**: a task board and every worktree's embedded terminal side by side,
each session showing whether it's idle, busy, or waiting for you. When a session frees up, the
dispatcher proposes the next task to give it — and on your approval, injects the prompt directly into
that session's terminal. The Claude in each worktree stays the executor; Keitora just orchestrates.

## Features

- **Terminal grid** — one embedded [xterm.js](https://xtermjs.org/) pane per worktree, each bound to a
  raw cross-platform PTY running `claude` in that worktree. No tmux, no external tabs.
- **Task board** — a live kanban over a shared SQLite task DB (the `keitora-tasks` crate). Claim,
  update, and complete tasks; edits round-trip with the `./keitora task` CLI in real time.
- **Advisory dispatcher** — ranks task→worktree suggestions; injects a prompt only on your approval,
  and only into a session that's idle. The human stays in the loop.
- **Session status** — idle / busy / waiting / offline lights per worktree, driven by Claude Code
  hooks so they're accurate in real time (no output-scraping).

<!-- TODO: replace with a real in-app screenshot once captured -->
<!-- ![Keitora in action](docs/screenshot-app.png) -->

## Getting started

### Prerequisites

- A recent **stable Rust** toolchain (the workspace builds on Rust edition 2021; there is no declared
  workspace-wide minimum version).
- **Node.js** and **Yarn**.
- **macOS:** the **Xcode Command Line Tools** (`xcode-select --install`) — Tauri's Rust build needs
  the C/linker toolchain they provide.
- **Windows** is a fast-follow; see [`docs/windows-backend.md`](docs/windows-backend.md) for the port
  status.

### Install & run

```sh
yarn install        # install frontend deps + set up the pre-commit hook
yarn tauri dev      # launch Keitora in development
```

Other useful commands:

| Command | What it does |
| --- | --- |
| `yarn dev:isolated` | Dev launch against an **isolated herdr** and throwaway project (see below; `yarn dev:herdr` is the same) |
| `yarn build` | Typecheck (`tsc`) + build the frontend (`vite build`) |
| `yarn tauri:bundle` | Build a packaged app (stages the `keitora` CLI as a sidecar) |
| `yarn test` | Frontend tests (Vitest) |
| `cargo test` | Rust workspace tests |

### Running dev alongside an installed Keitora

Keitora runs every terminal in a herdr server, and **herdr is
required** — it is the only PTY backend. Keitora takes the first of these that is an executable file:

1. `[pty] herdr_bin` in `keitora.toml`
2. `KEITORA_HERDR_BIN`
3. `herdr` on Keitora's own process `PATH` (`herdr.exe` on Windows): the usual hit on Windows and when
   Keitora is started from a terminal
4. macOS and Linux: `/opt/homebrew/bin/herdr`, `/usr/local/bin/herdr`, then `~/.local/bin/herdr`
5. the newest `~/.herdr/packages/standalone/releases/<version>-<triple>/herdr` (`herdr.exe` on Windows;
   by version, not by name)
6. macOS and Linux: the first `herdr` on your login-shell `PATH`. A Dock or Finder launch has a minimal
   process `PATH`, so this is how an install somewhere else (`~/.cargo/bin`, say) is found. It is the
   only step that runs your login shell, and only when steps 1 to 5 found nothing; it runs in the
   background, so a slow shell startup never delays the window.

If none of these exists, Keitora still tries a bare `herdr` when it launches the server. Windows has no
steps 4 and 6: no fixed install directories and no login shell.

Steps 1 and 2 name one exact binary. From step 3 on herdr is found for you, and a herdr server that is
already running is recognised by name rather than by path, so it stays recognised when an update moves
the binary under it.

Panes get a `PATH` made of your login-shell `PATH` first, in your order (so your asdf, mise, nvm or
pyenv shims keep winning), then the three directories of step 4, then Keitora's process `PATH`.

If herdr can't be found the app still opens, and the rail's offline card says so: "herdr was not found.
Looked in … Install herdr, or set [pty] herdr_bin in keitora.toml to its full path, then press Restart."
(hover the card for the whole message; every place searched is listed in `keitora.log`). Install herdr or point `herdr_bin` at it, then press
**Restart** on that card; Keitora searches again and starts the server without a relaunch.

On macOS and Linux herdr's control sockets live under `$KEITORA_HOME/herdr/config/herdr/`, and a unix
socket path is limited to 103 bytes on macOS (107 on Linux). A `KEITORA_HOME` (or `KEITORA_DEV_HOME`)
long enough to go over is rejected at startup with a message naming the path and the limit; use a
shorter one.

A plain `yarn tauri dev` runs with its cwd inside this repo, so it auto-registers the real project and
spawns agents into every one of its worktrees — hijacking sessions you're using. To develop safely
alongside an installed Keitora, launch dev with:

```sh
yarn dev:isolated           # same as `yarn dev:herdr`
```

It builds and runs the app directly (bypassing `tauri dev`, which can't set the app's cwd) against a
`yarn dev` Vite server on :1420. The dev home is `~/.keitora-dev/<checkout>-<hash>-herdr` — **keyed per
checkout**, so several git worktrees can each run a dev instance without colliding. Override the base
with `KEITORA_DEV_HOME`. There is no HMR-against-real-registry flow any more; the app is rebuilt on each
launch.

| | |
| --- | --- |
| **Isolated** | Its own herdr server and state (purged every run), a fresh registry holding only a throwaway git project (also the app's cwd), and no herdr agent-integration install into your real `~/.claude` / `~/.codex` |
| **Shared** | The task DB, and the herdr binary itself |

Sharing the task DB means a dev build can write to your real tasks. Dogfooding against your real projects is a deliberate,
separate act and not what this launcher does.

### Upgrading from ptyd

Earlier builds ran terminals in a bundled `keitora-ptyd` daemon, which has been removed.

- The stale `~/.keitora/ptyd.sock`, `ptyd.lock`, `ptyd-state.json` and `ptyd.log` can be deleted.
- If an old `keitora-ptyd` process is still running, stop it **by PID** — never by image name, which
  would also kill other Keitora processes.
- Install herdr (see above). Without it the rail shows why instead of terminals.

### The task CLI

The `./keitora` wrapper (repo root) is the command-line half of Keitora — the same shared task DB the
app reads. It prefers a built binary and falls back to `cargo run -p keitora-cli`:

```sh
./keitora task list           # open + active tasks
./keitora task claim <id>     # claim a task for this worktree
./keitora task done <id>      # complete it (reports newly-unblocked tasks)
./keitora session <state>     # (used by Claude Code hooks) record idle/busy/waiting
./keitora config …            # manage project configuration
```

Projects are configured with a `keitora.toml` at the repo root (`keitora config init` scaffolds one).
A minimal example:

```toml
[project]
name = "keitora"
worktrees_dir = "../keitora-worktrees"

[tasks]
db_path = "../keitora-worktrees/shared/tasks.db"   # shared SQLite coordination bus (WAL)
worktrees = ["alpha", "bravo", "charlie"]

[dispatch]
steps = ["/clear", "/workflow {task_number}"]      # typed into a worktree on dispatch
```

The `db_path` DB is the coordination bus: every worktree session and the app read and write the same
store, so a claim in one is visible everywhere.

## Architecture

Keitora is a [Tauri](https://tauri.app/) app: a **Rust backend** in `src-tauri/` (owns the PTYs, the
task DB, session state, and the dispatcher) and a **React 19 + Vite** frontend in `src/`. Task logic
lives once in the `crates/keitora-tasks` library and is shared by both the `./keitora` CLI and the app
— they're peers over one store, never forks of the logic. See
[`docs/build-plan.md`](docs/build-plan.md) for the full design.

## Development

### Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

### Linting & formatting

The repo enforces a strict, zero-warning baseline:

- **TypeScript / React** — [Biome](https://biomejs.dev) (`biome.json`) is the unified
  linter + formatter (`noExplicitAny` is an error). Plus `tsc --noEmit` for types.
- **Rust** — `rustfmt` (`rustfmt.toml`) and Clippy with `clippy::all` + rustc `warnings`
  **denied** workspace-wide (`[workspace.lints]`).

Scripts:

| Command | What it does |
| --- | --- |
| `yarn lint` | `biome check src` (lint + format check) |
| `yarn format` | `biome format --write src` |
| `yarn lint:rust` | `cargo fmt --all --check` + `cargo clippy --workspace --all-targets` |
| `cargo lint` | clippy across the workspace (alias) |

A **pre-commit hook** (husky + lint-staged) runs automatically. It installs itself on
`yarn install` (or run `yarn prepare` once). On each commit it formats/lints the staged
files, runs `cargo clippy` when Rust is touched, and `tsc --noEmit` when TS is touched.

Notes:
- The hook no-ops in worktrees that haven't run `yarn install` (it guards on `.husky/_`),
  so it never blocks commits elsewhere.
- lint-staged formats **staged** content; if a file is partially staged, stage the rest
  before committing to avoid surprises.
- Bypass in a pinch with `git commit --no-verify` (CI/review still expects green).

## Building a Signed Release

Use `scripts/build-dmg.sh` to produce a signed, notarized `.dmg` for distribution. Run from the repo root after setting the required environment variables.

### Required environment variables

Set **`APPLE_SIGNING_IDENTITY`** plus **one** of the two notarization flows:

**Flow A — Apple ID (simpler for solo developers)**

| Variable | Value |
|----------|-------|
| `APPLE_SIGNING_IDENTITY` | Full cert name, e.g. `Developer ID Application: Jane Smith (AB12CD34EF)` |
| `APPLE_ID` | Apple ID email address used for notarization |
| `APPLE_PASSWORD` | App-specific password from appleid.apple.com (NOT your Apple ID password) |
| `APPLE_TEAM_ID` | 10-character team ID, e.g. `AB12CD34EF` |

**Flow B — API key (recommended for CI; no 2FA dependency)**

| Variable | Value |
|----------|-------|
| `APPLE_SIGNING_IDENTITY` | Full cert name as above |
| `APPLE_API_KEY` | Key ID from App Store Connect, e.g. `ABCD123456` |
| `APPLE_API_ISSUER` | Issuer UUID from App Store Connect |
| `APPLE_API_KEY_PATH` | Absolute path to the `.p8` private key file |

To find your signing identity, run `security find-identity -v -p codesigning`. Never commit any of these values.

### Running the build

```bash
scripts/build-dmg.sh
```

The signed `.dmg` is produced at `src-tauri/target/release/bundle/dmg/`.

## Learn more

- [`docs/build-plan.md`](docs/build-plan.md) — the full design and phased build plan.
- Keitora also has an optional GitHub-Projects task backend, documented in [`docs/build-plan.md`](docs/build-plan.md).

## License

[MIT](LICENSE) © 2026 Andrew Rudoi
