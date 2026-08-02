# duscan

> High-performance disk-usage scanner for Linux, written in Rust. Built for
> filesystems with tens of millions of files: parallel walk, bounded RAM,
> per-target history, an interactive config/report TUI, and atomic remote sync.

`duscan` walks a Linux filesystem in parallel, classifies usage by configured
teams/users, and writes a single SQLite `report.db` per target (detail, treemap,
history and permission tables) suitable for terminal inspection, per-user export,
and remote dashboards.

---

## Quick start

```bash
make build          # → ./duscan (release, stripped)
```

```bash
# 1. Declare a target with its teams + users (one command)
./duscan set-target backend /data/backend --team dev=alice,bob --team ops=carol

# 2. Scan it
./duscan run --target backend

# 3. Read the results (no need to repeat --output-dir; falls back to duscan.toml)
./duscan detail --user alice --target backend
./duscan history --target backend --compare
./duscan tree-show --target backend --level 3
```

Or just run `./duscan` with **no subcommand** to open the interactive TUI
(configure targets, scan, and browse reports without leaving the screen).

---

## Interactive TUI

Run `duscan` with no arguments to open the configuration TUI — five tabs:
**Targets**, **Teams & Users**, **Scan/Sync**, **Output**, **Settings**. Every
change is written straight to `duscan.toml` + `targets/*.toml` (saved per op).

**Keys:** `↹`/`1`..`5` switch tabs, `↑↓` move, `[`/`]` switch target (Teams,
Scan/Sync & Output tabs), `a` add, `e` edit, `d` delete, `u` add users (accepts
`alice,bob` and `@file`), `x` remove user, `/` filter long lists, `q`/`Esc` quit.
Local path inputs (scan path, `export_dir`, and the `@file` token) support `Tab`
completion and are stored as absolute paths.

**Run a scan in-place:** `r` scans the selected target, `R` scans all. The scan
runs without leaving the TUI — a live "Scan jobs" panel shows each target's stage
(Scanning → Building detail → Merging → History → Done) plus live file/dir counts.
You can keep navigating while it runs; only one scan runs at a time. When it
finishes the config reloads from disk.

**Output tab** — browse a target's results in five sub-views (`h`/`d`/`p`/`i`/`t`):

| Key | View | Shows |
|---|---|---|
| `h` | History | Per-day usage snapshots + top users |
| `d` | Detail | Pick a user → totals + top dirs/files (`x` export one, `X` export all) |
| `p` | Perm | The user's permission issues (Type / Error / Path) |
| `i` | Inode | Inode count (files + dirs) + per-dir file-count breakdown |
| `t` | Treemap | ncdu-style directory browser (`Enter` descend, `Backspace` up) |

`/` filters long lists live in every view. Reads each target's `report.db`; empty
states point you to press `r` to scan first.

---

## CLI reference

All reader commands fall back to `output_dir` from `duscan.toml`, so `--output-dir`
is optional once configured.

### Config

```bash
# One-shot (preferred): create/update a target with teams + users
./duscan set-target <name> <path> --team dev=alice,bob --team ops=carol \
    [--end-scan YYYYMMDD] [--purge-time DAYS] [--merge]
#   replace by default (config = the declaration); --merge adds without removing

# Declarative file (targets.toml / .json as source of truth)
./duscan apply <file> [--dry-run] [--merge]

# Granular CRUD (still supported)
./duscan add-target <name> <path> [--end-scan YYYYMMDD] [--purge-time DAYS]
./duscan remove-target <name>
./duscan add-team <name> --target <target>
./duscan add-user <user> [user...] --team <team> --target <target>
./duscan remove-user <user> [user...] --target <target>
./duscan list [--target <name>] [--team <name>] [--json]
./duscan import-legacy --dir <configs_dir> [--force]   # migrate legacy JSON configs
```

### Scan + read

```bash
./duscan run [--target <name>] [--tree-map] [--workers N] [--level N] [--debug]
#   Headless text output: per-target phases, live files/dirs/s + elapsed + memory.
#   --workers N: explicit workers bypass device-class caps (use full budget).
#   --debug: Phase 1/2/3 profiling + RSS diagnostics.
#   per-target webhook_url auto-sends a Teams card after each scan (no separate step).
#
#   Example output:
#     Device group dev=64512 class=hdd workers=8 targets=[Test]
#     Scanning 1 target(s)...
#     [Test] Started scanning...
#     [Test] 277108 files, 34188 dirs | 2.1s 45 MB (138451 files/s)
#     [Test] Building detail DB... (1506009 files, 225996 dirs, 87.6 GB | 14.5s 320 MB)
#     [Test] Done: 1506009 files, 225996 dirs, 87.6 GB | 26.5s 82 MB
#     Scan complete: 1506009 files, 225996 dirs | 26.5s

./duscan status [--target <name>] [--watch] [--json]
#   reads scan_status.json (stage/running/files/dirs/size/elapsed). Works for local
#   and background scans (status file is on shared storage).
#   --watch redraws every 2s; a heartbeat >30s old is flagged "running (stale)".

./duscan detail --user <user> [--target <name>] [--top N] [--json]
#   --type report (default: top dirs/files) | permission (--search KW) | inode

./duscan tree-show [--target <name>] [--level N] [--limit N] [--path P] [--search KW]
./duscan export --user <user> [--target <name>] [--export-dir DIR]
#   Headless export with per-user progress and file/dir counts:
#     Exporting 'alice'...
#       usage_dir_alice.txt   — 1234 dirs
#       usage_file_alice.txt  — 56789 files
#     Exported 32 user(s) in 45.3s
./duscan history [--target <name>] [--days N] [--json]
#   --compare [--top N]: per-user growth/trend table across snapshots
./duscan notify --webhook-url URL [--target <name>]
./duscan sync --host HOST --dest-dir DIR [--user USER] [--pass]
#   --pass: sshpass -e auth (password read from SSHPASS env; never stored)
```

---

## Configuration

Config lives **next to the binary** — `duscan.toml` + `targets/` are read/written
from the directory containing the `duscan` executable, independent of the current
working directory (no cwd or `~/.config` lookup). This makes cron reliable: a job
running from `$HOME` or `/` finds the same config as an interactive shell.

```
duscan.toml            # globals: output_dir, workers, max_parallel_devices, nfs_parallel
targets/
├── backend.toml       # one target per file (teams carry users, no team_id)
├── frontend.toml
└── logs.toml
```

```toml
# duscan.toml
output_dir = "reports"        # relative → anchored to the binary dir; absolute → used as-is
workers = "auto"
max_parallel_devices = 0      # 0 = unlimited concurrent device groups
nfs_parallel = 64             # walker-thread cap per NFS device (latency-bound; default 64)
hdd_parallel = 8              # walker-thread cap per HDD device (seek-bound; default 8)
ssd_parallel = 0              # walker-thread cap per SSD device (0 = unlimited)
```

```toml
# targets/backend.toml — hand-editable, version-controllable
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

Per-target scan overrides (empty = global default) also live here: `tree_map`,
`level`, `workers`, `sync_host`/`sync_dest_dir`/`sync_user`, `export_dir`,
`webhook_url`, `sync_pass`. When `sync_host` is set, that target's output is
rsync'd to the remote automatically after each scan. A cron entry is just:

```cron
0 2 * * * /path/to/duscan run --target backend
```

---

## Output layout

Per target: `<output-dir>/<target>/` holds a single `report.db`, the heartbeat,
and one log per scan.

```text
<output-dir>/<target>/
├── report.db            # detail + treemap + history + perm_issues (single source of truth)
├── scan_status.json     # heartbeat: stage, elapsed, running, file/dir/size counts
└── logs/
    └── scan_<ts>.log    # one log per scan (legacy-style phase summary)
```

Permission issues are **merged into `report.db`** (the `perm_issues` table); the
intermediate `permission_issues.db` scratch file is deleted after the merge.

---

## `report.db` schema

A single SQLite DB per target with prefixed table groups.

| Group | Tables | Purpose |
|---|---|---|
| Meta | `meta` | scan_root, scan_name/path, timestamp, totals, treemap_db |
| Detail | `detail_users`, `detail_dirs`, `detail_file_names`, `detail_files` | Per-user files/dirs/exts, keyset-pagination indexed |
| Treemap | `treemap_dirs`, `treemap_names`, `treemap_owners` | Directory tree + per-dir real inode owner (`st_uid`) |
| History | `hist_snapshots`, `hist_user_usage`, `hist_team_usage` | Per-day usage trend for `history`/`--compare` |
| Permissions | `perm_issues` | Indexed access-error log (uid, kind, errcode, path) |

`treemap_dirs.owner_uid` is the directory's **real inode owner** (`st_uid` from the
Phase 1 walk), resolved via `treemap_owners` — not the user consuming the most
space. Dirs that couldn't be stat'd fall back to the smallest known uid.

---

## Architecture

```
├── Cargo.toml              # Workspace
├── core/src/               # Scanning engine (pure Rust)
│   ├── scan_core.rs        # Phase 1 parallel filesystem walk
│   ├── scan_state.rs       # Per-thread buffers + binary spill writers
│   ├── report_pipeline.rs  # Phase 2+3 SQLite builder
│   ├── db_writer.rs        # DDL, bulk insert, merge, atomic rename
│   └── pipe_*.rs           # Spill format, permission, treemap helpers
├── cli/src/                # CLI binary (duscan)
│   ├── main.rs             # Clap dispatch + scan orchestration + status
│   ├── config.rs           # TOML config CRUD
│   ├── scheduler.rs        # Device-aware scan plan (classify + cap workers)
│   └── config_tui.rs       # Ratatui config + report TUI
├── legacy/                 # Python reference code (read-only)
└── src/rust_scanner/       # Original PyO3 crate (.so build, reference)
```

### Pipeline phases

```
Phase 1: parallel WalkBuilder walk
  → per-thread binary spill files (events, dir aggregates, dir owners, perm TSV)
Phase 2: detail pipeline (Rayon parallel)
  → compact spill re-encoding → path tree → per-user detail (rayon par_iter)
  → detail tables → persist treemap aggregates (only with --tree-map)
Phase 3: treemap pipeline (only with --tree-map)
  → load aggregates → treemap tables (filtered by --level)
Merge + History: fold everything into report.db, append the day's snapshot
```

---

## Scan performance

Phase 1 is metadata-I/O-bound: on NFS every `lstat`/`statx` is a network RPC, so
metadata dominates wall time. Two things keep it fast:

- **One statx per entry.** Both the file and directory hot-paths issue a single
  `statx_lite()` syscall returning dev+ino+mnt_id+blocks+uid+nlink at once —
  covering bind-mount, filesystem-boundary (`du -x`), hardlink/loop dedup and
  inode-owner checks together. Falls back to `entry.metadata()` on old kernels
  where `statx()` isn't usable. Local filesystems keep the plain `metadata()` path.
- **`nfs_parallel`** (default **64**) caps walker threads per NFS device. NFS is
  latency-bound, so more RPCs in flight hide round-trip latency; raise it for
  high-latency mounts, lower it if the NFS server is the bottleneck.
- **`hdd_parallel`** (default **8**) caps walker threads per HDD device (seek-bound).
- **`ssd_parallel`** (default **0** = unlimited) caps walker threads per SSD device.
  All three caps are configurable in `duscan.toml` and editable in the Settings tab.
  Pass `--workers N` explicitly to bypass all caps and use the full budget.

Correctness on NFS: hardlink dedup keys on `(stx_ino, stx_mnt_id)` because
`st_dev` is unstable on NFS clients (the same inode reached via two paths can
report different `st_dev`, which would inflate size).

---

## Build

```bash
make setup-env      # install a project-local Rust toolchain into ./.rust (one-time)
make build          # → ./duscan (release, dynamically linked, stripped)
make static-build   # → ./duscan-static (fully static)
make test           # tests, via the local toolchain
make install        # copy ./duscan to /usr/local/bin
make clean          # cargo clean + remove built binaries
make clean-env      # remove the project-local toolchain in ./.rust
```

**Self-contained toolchain.** `make setup-env` installs `rustup` + `cargo` into
`./.rust` (with `CARGO_HOME=./.rust/cargo`, `RUSTUP_HOME=./.rust/rustup`) instead
of `~/.cargo`. The whole build is then hermetic — toolchain, registry cache, and
git deps all live under the project dir and never touch a system Rust install.
Every `make` target runs cargo from that local toolchain. If you already have a
global cargo on `PATH`, `make build` uses it as a fallback when `setup-env` hasn't
been run. To drive the local toolchain from your own shell:

```bash
export CARGO_HOME=$PWD/.rust/cargo RUSTUP_HOME=$PWD/.rust/rustup
export PATH=$PWD/.rust/cargo/bin:$PATH
```

`RUST_VERSION` overrides the toolchain channel (default `stable`):
`make setup-env RUST_VERSION=1.90.0`.

- **OS:** Linux x86_64
- **For sync (optional):** `ssh` with key-based auth, or `sshpass` for `--pass`

---

## Verification

```bash
make test                                    # workspace tests
./duscan run --target <name> --debug          # Phase 1/2/3 profiling + RSS
sqlite3 reports/<name>/report.db ".tables"   # inspect the merged DB
sqlite3 reports/<name>/report.db "SELECT * FROM meta"
```

