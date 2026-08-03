# duscan — Rust CLI (build from source)

## Quick commands

```bash
make setup-env   # install a project-local Rust toolchain into ./.rust (one-time)
make build       # → ./duscan (uses the local toolchain)
make static-build # → static binary
make clean        # cargo clean + remove binaries
make clean-env    # remove ./.rust toolchain
```

`make setup-env` puts `rustup`/`cargo` under `./.rust` (`CARGO_HOME=./.rust/cargo`,
`RUSTUP_HOME=./.rust/rustup`) so the build is hermetic — no `~/.cargo` dependency.
All `make` targets run cargo from there; a global cargo is used only as a fallback
when setup-env hasn't run. `./.rust/` is gitignored.

## Interactive config TUI

Run `duscan` with **no subcommand** to open the interactive configuration TUI
(five tabs: Targets, Teams & Users, Scan/Sync, Output, Settings). It manages
targets/teams/users and writes every change straight to `duscan.toml` +
`targets/*.toml` (save on each op). Keys: `↹`/`1`..`5` switch tabs, `↑↓`
move, `[`/`]` switch target (on Teams, Scan/Sync & Output tabs), `a` add, `e`
edit, `d` delete, `u` add users (supports `alice,bob` and `@file`), `x` remove
user, `q`/`Esc` quit. Local path inputs (scan path, `export_dir`) and the `@file`
token in add-users support `Tab` completion and are stored as absolute paths;
`sync_dest_dir` (remote path) and `output_dir` (anchored to the binary dir) are
left as typed. `/` filters long lists (see the Output tab notes below).

**Run a scan from the TUI:** `r` scans the selected target, `R` scans all. The
scan runs **in-place** — the TUI stays open and a live "Scan jobs" panel appears
above the footer showing each target's current stage (Scanning → Building detail
→ Merging → History → Done) plus live file/dir counts. You can keep navigating
the config while it runs; `q` quits the TUI (the scan keeps running in the
background until its threads finish). Only one scan runs at a time (`r`/`R` are
ignored while one is in flight). When it finishes the config reloads from disk.

**Scan/Sync tab** — per-target overrides (empty = use the global default):
`tree_map`, `level`, `workers`, `sync_host`/`sync_dest_dir`/`sync_user`,
`export_dir` (destination for the Output/Detail `x`/`X` export; empty = `exports`),
`webhook_url` (Teams card auto-sent after each scan), and `sync_pass` (rsync via
`sshpass -e`, password read from the `SSHPASS` env — never stored). When
`sync_host` is set, that target's output is rsync'd to the remote automatically
after each scan. These are also stored in `targets/<name>.toml`.

**Output tab** — view a target's scan results without leaving the TUI. Five
sub-views switched with `h`/`d`/`p`/`i`/`t`: **History** (per-day usage snapshots
+ top users), **Detail** (pick a user → totals + top dirs/files; `x` exports the
selected user's usage txt, `X` exports every user), **Perm** (`p` — the selected
user's permission issues: Type/Error/Path), **Inode** (`i` — the user's inode
count = files + dirs, plus a per-dir file-count breakdown sorted by file count),
and **Treemap** (an ncdu-style directory browser: `↑↓` move, `Enter` descend into
a sub-directory, `Backspace` go up, sized bars per entry). `[`/`]` switch target.
Press `/` to filter long lists live (Detail/Perm/Inode user column, Treemap
entries, and the Teams&Users user column) — type to narrow, `Esc` clears. Reads each
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
./duscan list [--target <name>] [--team <name>] [--json]   # --target also shows "Other (unassigned)" users from the report; --json adds other_users

# Scan + read (all reader commands fall back to output_dir from duscan.toml — no need to repeat --output-dir)
./duscan run [--output-dir DIR] [--tree-map] [--workers N] [--level N] [--target <name>] [--debug]
#   --debug: emit core Phase 1/2/3 profiling + RSS diagnostics (headless/piped stdout; TUI mode suppresses core stdout)
#   per-target webhook_url auto-sends a Teams card after each scan (set on Scan/Sync tab) — no separate `notify` step for cron
./duscan status [--output-dir DIR] [--target <name>] [--watch] [--json]
#   reads each target's scan_status.json (stage/running/files/dirs/size/elapsed). --watch redraws every 2s;
#   a scan whose heartbeat is >30s old is flagged "running (stale)" (e.g. a killed/dead process).
./duscan detail --user <user> [--output-dir DIR] [--top N] [--target <name>] [--json]
#   --type report (default: top dirs/files by size) | permission (perm_issues, filter with --search KW) | inode (per-dir file counts)
./duscan tree-show [--output-dir DIR] [--level N] [--limit N] [--path P] [--search KW] [--target <name>]
./duscan export --user <user> [--output-dir DIR] [--export-dir DIR] [--target <name>]   # -> <export-dir>/<target>/usage_dir_<u>.txt + usage_file_<u>.txt
./duscan notify --webhook-url URL [--output-dir DIR] [--target <name>]
./duscan sync --host HOST --dest-dir DIR [--output-dir DIR] [--user USER] [--pass]
#   --pass: password auth via `sshpass -e` (reads password from SSHPASS env; password never stored). Per-target: sync_pass=true.
./duscan history [--output-dir DIR] [--target <name>] [--days N] [--json]   # per-day usage trend from report.db hist_* tables
#   --compare [--top N]: per-user growth/trend table across snapshots (cols = dates old→new, + Abs/%/Trend: ^ up, v down, ~ mixed)
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

**Config location is always next to the binary.** `duscan.toml` + `targets/` are
read/written from the directory containing the `duscan` executable, independent
of the current working directory (no cwd or `~/.config` lookup). This makes cron
reliable: a job running from `$HOME` or `/` finds the same config as an
interactive shell. A **relative** `output_dir` (default `"reports"`) is likewise
anchored to the binary dir, so reports land next to the binary — not under cron's
cwd. An **absolute** `output_dir`, or an explicit `--output-dir`, is used as-is.
So a cron entry is just: `0 2 * * * /path/to/duscan run --target <name>`.

Config layout (TOML, auto-created next to the binary):

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

Per-target output: `<output-dir>/<target>/` holds `report.db`, `scan_status.json`, and `logs/scan_<ts>.log` (one log per scan, legacy-style phase summary). Permission issues are **merged into `report.db`** (the `perm_issues` table, read by `detail --type permission` and the TUI Perm view); the intermediate `permission_issues.db` scratch file is deleted after the merge, so `report.db` is the single source of truth.

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

## Scan performance (NFS)

Phase 1 (the parallel walk in `core/src/scan_core.rs`) is metadata-I/O-bound: on
NFS every `lstat`/`statx` is a network RPC, so metadata dominates wall time. Two
things keep it fast on NFS:

- **One statx per entry.** Both the file and directory hot-paths issue a single
  `statx_lite()` syscall that returns dev+ino+mnt_id+blocks+uid+nlink at once —
  covering the bind-mount, filesystem-boundary (`du -x`), hardlink/loop dedup and
  inode-owner checks together. On old kernels where `statx()` isn't usable it
  falls back to `entry.metadata()` (correct, just more syscalls). Local
  filesystems keep the plain `metadata()` path (already one syscall).
- **`nfs_parallel`** (in `duscan.toml`, default **16**) caps walker threads per
  NFS device. NFS is latency-bound, so more RPCs in flight hide round-trip
  latency; raise it for high-latency mounts, lower it if the NFS server is the
  bottleneck. HDDs stay capped low (seek-bound); SSD/NVMe get the full budget.

## Memory / VSZ under virtual-memory caps (LSF `-M`, cgroups)

The binary uses **mimalloc** as its global allocator. This is great for RSS and
throughput (see `cli/src/main.rs`), but two mimalloc behaviours can push the
**virtual** address space (VSZ) far above real RSS — which then **kills the
process under a `RLIMIT_AS` / `ulimit -v` cap** even though RSS is low:

- **Arena pre-reservation.** mimalloc reserves arenas in 1GiB blocks and grows
  each new reservation exponentially (`1 << arena_count/8`, up to 2^16×). With
  many walker threads this balloons VSZ (observed ~27GB) while RSS stays low
  (~10GB). The next arena `mmap` then fails with `ENOMEM`, and Rust aborts with
  `memory allocation of N bytes failed` → SIGABRT → core dump (exit 134), even
  though real RAM is plentiful.
- **Allocation churn.** Freed segments are not returned to the OS eagerly; under
  heavy allocation the VmPeak keeps climbing.

Duscan mitigates both in code (`cli/src/main.rs` `configure_mimalloc()`,
`core/src/report_pipeline.rs` `PathTree::build`):

- `mi_option_set(mi_option_arena_reserve /*23*/, 0)` at startup — disables arena
  pre-reservation; large allocations fall back to exact-size OS mmaps, keeping
  VSZ ≈ RSS.
- `trim_heap()` = `mi_collect(true)` at phase boundaries instead of `malloc_trim`
  (a **no-op** with mimalloc — glibc's trim does nothing) — forces freed
  segments back to the OS.
- `PathTree::build` walks ancestors **only until a known dir key** instead of all
  the way to root, cutting `~2×depth×N` String allocations down to `~2×N` on
  deep trees (millions of dirs at depth 15-20 used to balloon VSZ past the cap).

Diagnostics: run with `--debug` and grep the `[MEM checkpoint]` lines — each
prints `RSS | VSZ | VmPeak`. VSZ is what a `ulimit -v` cap enforces, not RSS. A
`memory allocation of <small-bytes> failed` message means the process is out of
**address space** (cap hit), not out of RAM.

Operational notes:
- A full-scan VmPeak of ~17-18GB is normal at 58M files / 9.1M dirs. LSF
  `-M 20000` (→ `ulimit -v` ~24GB) is sufficient after the fixes above; if VSZ
  still approaches the cap, raise `-M` (e.g. `-M 40000`) — it widens RLIMIT_AS,
  not RSS.
- If `memory allocation of N bytes failed` appears with a **small** N while RSS
  is low, it is an address-space cap, not an OOM: check `ulimit -v` / `ulimit -d`
  inside the job, and `vm.overcommit_memory` / `CommitLimit` on shared nodes.

## Build notes

- Toolchain: `make setup-env` (project-local rustup/cargo in `./.rust`; hermetic).
- Release: `make build` → `./duscan` (dynamically linked, via local toolchain).
- Static: `make static-build` → `./duscan-static`
  (equivalent to `cargo rustc --release -p duscan -- -C target-feature=+crt-static`).
