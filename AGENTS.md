# duscan — Rust CLI (build from source)

## Quick commands

```bash
make build       # → ./duscan
make static-build # → static binary
make clean
```

## Usage

```bash
# Config CRUD
./duscan add-target <name> <path> [--end-scan YYYYMMDD] [--purge-time DAYS]
./duscan remove-target <name>
./duscan add-team <name> --target <target>
./duscan add-user <user> [user...] --team <team> --target <target>
./duscan remove-user <user> [user...] --target <target>
./duscan list [--target <name>] [--team <name>] [--json]
./duscan run [--output-dir DIR] [--tree-map] [--workers N] [--level N] [--target <name>]
./duscan detail --user <user> --output-dir DIR [--top N] [--target <name>] [--json]
./duscan tree-show --output-dir DIR [--level N] [--limit N] [--path P] [--search KW] [--target <name>]
./duscan export --user <user> --output-dir DIR [--export-dir DIR] [--target <name>]   # -> <export-dir>/<target>/usage_dir_<u>.txt + usage_file_<u>.txt
./duscan notify --webhook-url URL --output-dir DIR [--target <name>]
./duscan sync --host HOST --dest-dir DIR --output-dir DIR [--user USER]
./duscan history --output-dir DIR [--target <name>] [--days N] [--json]   # per-day usage trend from report.db hist_* tables
./duscan import-legacy --dir <configs_dir> [--force]                      # migrate legacy JSON configs -> duscan.toml
```

Config file: `duscan.toml` (TOML, auto-created next to binary or in current dir).

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
