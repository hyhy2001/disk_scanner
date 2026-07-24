# duscan — Rust CLI (build from source)

## Quick commands

```bash
make build       # → ./duscan
make static-build # → static binary
make clean
```

## Interactive config TUI

Run `duscan` with **no subcommand** to open the interactive configuration TUI
(five tabs: Targets, Teams & Users, Scan/Sync, Output, Settings). It manages
targets/teams/users and writes every change straight to `duscan.toml` +
`targets/*.toml` (save on each op). Keys: `↹`/`1`..`5` switch tabs, `↑↓`
move, `[`/`]` switch target (on Teams, Scan/Sync & Output tabs), `a` add, `e`
edit, `d` delete, `u` add users (supports `alice,bob` and `@file`), `x` remove
user, path inputs support `Tab` directory completion, `q`/`Esc` quit.

**Run a scan from the TUI:** `r` scans the selected target, `R` scans all. The
TUI hands the screen to the scan monitor, then returns when the scan finishes.

**Scan/Sync tab** — per-target overrides (empty = use the global default):
`tree_map`, `level`, `workers`, `sync_host`/`sync_dest_dir`/`sync_user`, and
`export_dir` (destination for the Output/Detail `x`/`X` export; empty = `exports`).
When `sync_host` is set, that target's output is rsync'd to the remote
automatically after each scan. These are also stored in `targets/<name>.toml`.

**Output tab** — view a target's scan results without leaving the TUI. Three
sub-views switched with `h`/`d`/`t`: **History** (per-day usage snapshots + top
users), **Detail** (pick a user → totals + top dirs/files; `x` exports the
selected user's usage txt, `X` exports every user), and **Treemap** (an
ncdu-style directory browser: `↑↓` move, `Enter` descend into a sub-directory,
`Backspace` go up, sized bars per entry). `[`/`]` switch target. Reads each
target's `report.db`; empty states point you to press `r` to scan first.

`duscan run` remains a separate read-only scan monitor.

## Usage

```bash
# Config — one-shot (preferred): create/update a target with teams+users in one command
./duscan set-target <name> <path> --team dev=alice,bob --team ops=carol [--end-scan YYYYMMDD] [--purge-time DAYS] [--merge]
#   replace by default (config = the declaration); --merge adds without removing

# Config — declarative file (targets.toml or .json with [[targets]] + teams{name,users})
./duscan apply <file> [--dry-run] [--merge]   # file is source of truth; --dry-run shows the diff

# Config — granular CRUD (still supported)
./duscan add-target <name> <path> [--end-scan YYYYMMDD] [--purge-time DAYS]
./duscan remove-target <name>
./duscan add-team <name> --target <target>
./duscan add-user <user> [user...] --team <team> --target <target>
./duscan remove-user <user> [user...] --target <target>
./duscan list [--target <name>] [--team <name>] [--json]

# Scan + read (all reader commands fall back to output_dir from duscan.toml — no need to repeat --output-dir)
./duscan run [--output-dir DIR] [--tree-map] [--workers N] [--level N] [--target <name>]
./duscan detail --user <user> [--output-dir DIR] [--top N] [--target <name>] [--json]
./duscan tree-show [--output-dir DIR] [--level N] [--limit N] [--path P] [--search KW] [--target <name>]
./duscan export --user <user> [--output-dir DIR] [--export-dir DIR] [--target <name>]   # -> <export-dir>/<target>/usage_dir_<u>.txt + usage_file_<u>.txt
./duscan notify --webhook-url URL [--output-dir DIR] [--target <name>]
./duscan sync --host HOST --dest-dir DIR [--output-dir DIR] [--user USER]
./duscan history [--output-dir DIR] [--target <name>] [--days N] [--json]   # per-day usage trend from report.db hist_* tables
./duscan import-legacy --dir <configs_dir> [--force]                        # migrate legacy JSON configs -> duscan.toml
```

Example `targets.toml` for `apply`:

```toml
[[targets]]
name = "backend"
path = "/data/backend"
end_scan = "20270101"
purge_time = 90
  [[targets.teams]]
  name = "dev"
  users = ["alice", "bob"]
  [[targets.teams]]
  name = "ops"
  users = ["carol"]
```

Config layout (TOML, auto-created next to binary or in current dir):

```
duscan.toml            # global settings only: output_dir, workers, max_parallel_devices, nfs_parallel
targets/
├── backend.toml       # one target per file (ergonomic: teams carry users, no team_id)
├── frontend.toml
└── logs.toml
```

Each `targets/<name>.toml` is the hand-editable, version-controllable declaration of a
single target — `team_id`s are assigned internally on load, never written. `set-target`,
`apply`, and the CRUD commands all write these files (one atomic write each) and delete
orphaned files automatically, so `remove-target` / `apply --replace` reconcile the dir.

```toml
# targets/backend.toml
name = "backend"
path = "/data/backend"
end_scan = "20270101"
purge_time = 90

[[teams]]
name = "dev"
users = ["alice", "bob"]

[[teams]]
name = "ops"
users = ["carol"]
```

Per-target output: `<output-dir>/<target>/` holds `report.db`, `permission_issues.db`, `scan_status.json`, and `logs/scan_<ts>.log` (one log per scan, legacy-style phase summary).

## Architecture

```
├── Cargo.toml              # Workspace
├── core/src/               # Scanning engine (pure Rust, no PyO3)
│   ├── scan_core.rs        # Phase 1 parallel filesystem walk
│   ├── report_pipeline.rs  # Phase 2+3 SQLite builder
│   ├── db_writer.rs        # DDL, bulk insert, merge
│   └── pyo3.rs             # Stub for pyo3 compatibility
├── cli/src/                # CLI binary
│   ├── main.rs             # Clap dispatch + scan loop
│   ├── config.rs           # TOML config CRUD
│   ├── scheduler.rs        # Device-aware scan plan
│   └── ui.rs               # Ratatui live TUI
├── legacy/                 # Python reference code (read-only)
└── src/rust_scanner/       # Original PyO3 crate (.so build)
```

## Build notes

- Release: `make build` → `./duscan` (dynamically linked)
- Static: `RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-unknown-linux-gnu -p duscan`
