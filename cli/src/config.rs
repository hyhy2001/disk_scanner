use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub teams: Vec<Team>,
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub end_scan: Option<String>,
    #[serde(default)]
    pub purge_time: Option<i64>,
    // Per-target scan overrides; None = fall back to the global default.
    #[serde(default)]
    pub tree_map: Option<bool>,
    #[serde(default)]
    pub level: Option<i64>,
    #[serde(default)]
    pub workers: Option<i64>,
    // Per-target sync target; when sync_host is set the scan auto-syncs this
    // target's output dir after merge/history.
    #[serde(default)]
    pub sync_host: Option<String>,
    #[serde(default)]
    pub sync_dest_dir: Option<String>,
    #[serde(default)]
    pub sync_user: Option<String>,
    // Per-target export directory for `export` (TUI + CLI); None = "exports".
    #[serde(default)]
    pub export_dir: Option<String>,
    // Per-target MS Teams webhook: when set, a scan auto-sends a summary card
    // after merge (so cron `duscan run` notifies without a separate step).
    #[serde(default)]
    pub webhook_url: Option<String>,
    // Per-target sync auth: when true, rsync uses `sshpass -e` reading the
    // password from the SSHPASS env var (no key setup). The password itself is
    // NEVER stored here — only this opt-in flag.
    #[serde(default)]
    pub sync_pass: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub name: String,
    pub team_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub team_id: i64,
}

/// A team plus its member usernames — the ergonomic shape used by `set-target`
/// and `apply` (team_ids are assigned internally, callers never deal with them).
#[derive(Debug, Clone, Default)]
pub struct TeamSpec {
    pub name: String,
    pub users: Vec<String>,
}

/// A complete target declaration: everything needed to (re)build one `Target`
/// in a single operation, so config is written once instead of per-field.
#[derive(Debug, Clone, Default)]
pub struct TargetSpec {
    pub name: String,
    pub path: String,
    pub teams: Vec<TeamSpec>,
    pub end_scan: Option<String>,
    pub purge_time: Option<i64>,
}

/// On-disk shape of `duscan.toml`: global settings only. Targets no longer live
/// here — each target is its own file under `targets/` (see `TargetFile`). This
/// keeps `duscan.toml` small and lets each target be reviewed/versioned alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Globals {
    #[serde(default = "default_output_dir")]
    pub output_dir: String,
    #[serde(default = "default_workers")]
    pub workers: String,
    #[serde(default)]
    pub max_parallel_devices: i64,
    #[serde(default = "default_nfs_parallel")]
    pub nfs_parallel: i64,
    #[serde(default = "default_hdd_parallel")]
    pub hdd_parallel: i64,
    #[serde(default = "default_ssd_parallel")]
    pub ssd_parallel: i64,
    /// Optional LSF batch-submit settings. When `enabled`, a headless
    /// `duscan run` re-submits itself to the cluster via the `bs` wrapper
    /// (a site-local `bsub`) instead of scanning on the login node.
    #[serde(default)]
    pub lsf: LsfConfig,
}

impl Default for Globals {
    fn default() -> Self {
        Self {
            output_dir: default_output_dir(),
            workers: default_workers(),
            max_parallel_devices: 0,
            nfs_parallel: default_nfs_parallel(),
            hdd_parallel: default_hdd_parallel(),
            ssd_parallel: default_ssd_parallel(),
            lsf: LsfConfig::default(),
        }
    }
}

/// LSF submit settings (the `[lsf]` table in duscan.toml). A headless
/// `duscan run` with `enabled = true` and the `cmd` wrapper on PATH re-submits
/// itself as a batch job: `bs -os <os> -M <mem_mb> [-q <queue>] [extra_args…]
/// <abs duscan> run <original args>`. Anything left empty is simply omitted
/// from the command line, so a minimal `[lsf]` (just `enabled = true`) submits
/// with the cluster defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LsfConfig {
    /// Master switch. When false (default) `duscan run` always scans locally.
    #[serde(default)]
    pub enabled: bool,
    /// The submit wrapper to invoke. Site default is `bs` (a custom `bsub`).
    #[serde(default = "default_lsf_cmd")]
    pub cmd: String,
    /// OS selector passed as `-os <os>` (e.g. "RHEL8"). Empty = omit.
    #[serde(default)]
    pub os: String,
    /// Memory reservation in MB passed as `-M <mem_mb>` (e.g. 20000). 0 = omit.
    #[serde(default)]
    pub mem_mb: i64,
    /// Optional queue passed as `-q <queue>`. Empty = omit.
    #[serde(default)]
    pub queue: String,
    /// Extra raw args inserted before the binary (e.g. ["-n", "4"]). Each entry
    /// is passed as its own argument (no shell splitting).
    #[serde(default)]
    pub extra_args: Vec<String>,
}

impl Default for LsfConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cmd: default_lsf_cmd(),
            os: String::new(),
            mem_mb: 0,
            queue: String::new(),
            extra_args: Vec::new(),
        }
    }
}

fn default_lsf_cmd() -> String { "bs".into() }

/// Ergonomic on-disk shape of one `targets/<name>.toml` file: teams carry their
/// member usernames directly and `team_id` is never written — it's assigned
/// internally on load. This is the hand-editable format (same idea as `apply`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetFile {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub teams: Vec<TeamFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_scan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_time: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_map: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_dest_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_pass: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamFile {
    pub name: String,
    #[serde(default)]
    pub users: Vec<String>,
}

/// In-memory config. `targets` is assembled from the per-target files on load
/// and written back out (one file each) on save; it is not serialized as a whole.
#[derive(Debug, Clone)]
pub struct Config {
    pub output_dir: String,
    pub workers: String,
    pub max_parallel_devices: i64,
    pub nfs_parallel: i64,
    pub hdd_parallel: i64,
    pub ssd_parallel: i64,
    pub lsf: LsfConfig,
    pub targets: Vec<Target>,
}

fn default_output_dir() -> String { "reports".into() }
fn default_workers() -> String { "auto".into() }
// NFS is latency-bound: hiding per-RPC round-trip latency needs many metadata
// requests in flight, so the per-device walker cap defaults well above the old
// value of 4 (production traces showed ~25 threads productively blocked on
// metadata). Still overridable via `nfs_parallel` in duscan.toml.
fn default_nfs_parallel() -> i64 { 64 }

// HDD is seek-bound, but modern drives with NCQ can handle more concurrent
// readers than the old cap of 4. Default 8 balances throughput against
// thrash on single-spindle disks. Override via `hdd_parallel` in duscan.toml.
fn default_hdd_parallel() -> i64 { 8 }

// SSD defaults to 0 (no cap = use full per-group budget). Set a positive
// value to cap walker threads on a specific SSD device group.
fn default_ssd_parallel() -> i64 { 0 }

impl Default for Config {
    fn default() -> Self {
        let g = Globals::default();
        Self {
            output_dir: g.output_dir,
            workers: g.workers,
            max_parallel_devices: g.max_parallel_devices,
            nfs_parallel: g.nfs_parallel,
            hdd_parallel: g.hdd_parallel,
            ssd_parallel: g.ssd_parallel,
            lsf: g.lsf,
            targets: Vec::new(),
        }
    }
}

/// Build an internal `Target` (with assigned team_ids) from the ergonomic
/// on-disk `TargetFile`. Team ids are allocated sequentially; each declared
/// user is attached to its team's id. Mirrors the id-assignment in
/// `upsert_target_full` so both entry paths agree on shape.
fn target_from_file(tf: TargetFile) -> Target {
    let mut teams: Vec<Team> = Vec::new();
    let mut users: Vec<User> = Vec::new();
    let mut next_id: i64 = 1;
    for team in tf.teams {
        let team_id = next_id;
        next_id += 1;
        teams.push(Team { name: team.name, team_id });
        for uname in team.users {
            if !users.iter().any(|u| u.name == uname) {
                users.push(User { name: uname, team_id });
            }
        }
    }
    Target {
        name: tf.name,
        path: tf.path,
        teams,
        users,
        end_scan: tf.end_scan,
        purge_time: tf.purge_time,
        tree_map: tf.tree_map,
        level: tf.level,
        workers: tf.workers,
        sync_host: tf.sync_host,
        sync_dest_dir: tf.sync_dest_dir,
        sync_user: tf.sync_user,
        export_dir: tf.export_dir,
        webhook_url: tf.webhook_url,
        sync_pass: tf.sync_pass,
    }
}

/// Collapse an internal `Target` back into the ergonomic on-disk shape: users
/// are grouped under their team by `team_id`. Teams appear in their stored
/// order; users within a team follow their stored order.
fn target_to_file(t: &Target) -> TargetFile {
    let teams: Vec<TeamFile> = t.teams.iter().map(|tm| {
        let users: Vec<String> = t.users.iter()
            .filter(|u| u.team_id == tm.team_id)
            .map(|u| u.name.clone())
            .collect();
        TeamFile { name: tm.name.clone(), users }
    }).collect();
    TargetFile {
        name: t.name.clone(),
        path: t.path.clone(),
        teams,
        end_scan: t.end_scan.clone(),
        purge_time: t.purge_time,
        tree_map: t.tree_map,
        level: t.level,
        workers: t.workers,
        sync_host: t.sync_host.clone(),
        sync_dest_dir: t.sync_dest_dir.clone(),
        sync_user: t.sync_user.clone(),
        export_dir: t.export_dir.clone(),
        webhook_url: t.webhook_url.clone(),
        sync_pass: t.sync_pass,
    }
}

/// Turn a target name into a safe file stem: path separators and other unsafe
/// characters become `_` so a target name can never escape `targets/`.
fn sanitize_stem(name: &str) -> String {
    name.chars()
        .map(|c| if c == '/' || c == '\\' || c == '.' || c.is_control() { '_' } else { c })
        .collect()
}

impl Config {
    /// Directory the running binary lives in. This is the single anchor for all
    /// config: `duscan.toml` and `targets/` are always read/written here,
    /// independent of the current working directory — so `cron` (which runs from
    /// $HOME or /) always finds the same config as an interactive shell.
    /// Falls back to cwd only if the executable path cannot be resolved.
    pub fn base_dir() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    }

    /// Config file path: always `<binary dir>/duscan.toml`.
    pub fn path() -> PathBuf {
        Self::base_dir().join("duscan.toml")
    }

    /// Directory holding the per-target files: always `<binary dir>/targets`.
    pub fn targets_dir() -> PathBuf {
        Self::base_dir().join("targets")
    }

    /// The configured `output_dir` resolved to an absolute path: a relative
    /// value is anchored to the binary dir (same anchor as the config), so cron
    /// — which runs from $HOME or / — writes reports next to the config rather
    /// than scattering them under its cwd. An absolute `output_dir` is returned
    /// unchanged. The stored string is left untouched (so `save()` round-trips
    /// the user's original value); only readers that resolve paths call this.
    pub fn resolved_output_dir(&self) -> String {
        let od = std::path::Path::new(&self.output_dir);
        if od.is_absolute() {
            self.output_dir.clone()
        } else {
            Self::base_dir().join(od).to_string_lossy().into_owned()
        }
    }

    /// Load global settings from `duscan.toml` and assemble `targets` from every
    /// `targets/<name>.toml`. Both layers degrade gracefully: a missing or
    /// unparseable file yields defaults / is skipped with a warning rather than
    /// aborting, matching the previous single-file behavior.
    pub fn load() -> Self {
        let p = Self::path();
        let globals: Globals = if p.exists() {
            match fs::read_to_string(&p) {
                Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
                    eprintln!("Warning: config parse error ({}): {}", p.display(), e);
                    Globals::default()
                }),
                Err(e) => {
                    eprintln!("Warning: config read error ({}): {}", p.display(), e);
                    Globals::default()
                }
            }
        } else {
            Globals::default()
        };

        let mut cfg = Config {
            output_dir: globals.output_dir,
            workers: globals.workers,
            max_parallel_devices: globals.max_parallel_devices,
            nfs_parallel: globals.nfs_parallel,
            hdd_parallel: globals.hdd_parallel,
            ssd_parallel: globals.ssd_parallel,
            lsf: globals.lsf,
            targets: Vec::new(),
        };

        // Read every targets/*.toml into a Target. Sorted for stable order.
        let dir = Self::targets_dir();
        if let Ok(entries) = fs::read_dir(&dir) {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
                .collect();
            files.sort();
            for f in files {
                let text = match fs::read_to_string(&f) {
                    Ok(t) => t,
                    Err(e) => { eprintln!("Warning: cannot read {}: {}", f.display(), e); continue; }
                };
                match toml::from_str::<TargetFile>(&text) {
                    Ok(tf) => {
                        if cfg.targets.iter().any(|t| t.name == tf.name) {
                            eprintln!("Warning: duplicate target '{}' in {} — keeping first", tf.name, f.display());
                            continue;
                        }
                        cfg.targets.push(target_from_file(tf));
                    }
                    Err(e) => eprintln!("Warning: invalid target file {}: {}", f.display(), e),
                }
            }
        }
        cfg
    }

    /// Write global settings to `duscan.toml` and one file per target under
    /// `targets/`. Each write is atomic (tmp + rename). Target files whose stem
    /// no longer matches a configured target are deleted, so `remove-target` and
    /// `apply` (replace) reconcile the directory without extra bookkeeping.
    pub fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir: {}", e))?;
        }

        // 1. Globals -> duscan.toml (atomic).
        let globals = Globals {
            output_dir: self.output_dir.clone(),
            workers: self.workers.clone(),
            max_parallel_devices: self.max_parallel_devices,
            nfs_parallel: self.nfs_parallel,
            hdd_parallel: self.hdd_parallel,
            ssd_parallel: self.ssd_parallel,
            lsf: self.lsf.clone(),
        };
        let content = toml::to_string_pretty(&globals).map_err(|e| format!("serialize: {}", e))?;
        let tmp = p.with_extension("toml.tmp");
        fs::write(&tmp, &content).map_err(|e| format!("write: {}", e))?;
        fs::rename(&tmp, &p).map_err(|e| format!("rename: {}", e))?;

        // 2. One file per target (atomic), tracking the stems we own.
        let dir = Self::targets_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir targets: {}", e))?;
        let mut kept: std::collections::HashSet<String> = std::collections::HashSet::new();
        for t in &self.targets {
            let stem = sanitize_stem(&t.name);
            kept.insert(stem.clone());
            let tf = target_to_file(t);
            let body = toml::to_string_pretty(&tf).map_err(|e| format!("serialize target '{}': {}", t.name, e))?;
            let dest = dir.join(format!("{}.toml", stem));
            let tmp = dest.with_extension("toml.tmp");
            fs::write(&tmp, &body).map_err(|e| format!("write target '{}': {}", t.name, e))?;
            fs::rename(&tmp, &dest).map_err(|e| format!("rename target '{}': {}", t.name, e))?;
        }

        // 3. Delete orphaned target files (stem not among current targets).
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("toml") { continue; }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if !kept.contains(stem) {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn find_target(&self, name: &str) -> Option<&Target> {
        self.targets.iter().find(|t| t.name == name)
    }

    pub fn find_target_mut(&mut self, name: &str) -> Option<&mut Target> {
        self.targets.iter_mut().find(|t| t.name == name)
    }

    pub fn add_target(&mut self, name: &str, path: &str, end_scan: Option<String>, purge_time: Option<i64>) -> Result<(), String> {
        if self.targets.iter().any(|t| t.name == name) {
            return Err(format!("Target '{}' already exists", name));
        }
        let target = Target {
            name: name.to_string(),
            path: path.to_string(),
            end_scan,
            purge_time,
            ..Default::default()
        };
        self.targets.push(target);
        self.save()
    }

    pub fn remove_target(&mut self, name: &str) -> Result<(), String> {
        let idx = self.targets.iter().position(|t| t.name == name)
            .ok_or_else(|| format!("Target '{}' not found", name))?;
        self.targets.remove(idx);
        self.save()
    }

    pub fn add_team(&mut self, team_name: &str, target_name: &str) -> Result<(), String> {
        let t = self.find_target_mut(target_name)
            .ok_or_else(|| format!("Target '{}' not found", target_name))?;
        if t.teams.iter().any(|tm| tm.name == team_name) {
            return Err(format!("Team '{}' already exists in '{}'", team_name, target_name));
        }
        let next_id = t.teams.iter().map(|tm| tm.team_id).max().unwrap_or(0) + 1;
        t.teams.push(Team { name: team_name.to_string(), team_id: next_id });
        self.save()
    }

    pub fn add_user(&mut self, username: &str, team_name: &str, target_name: &str) -> Result<(), String> {
        let t = self.find_target_mut(target_name)
            .ok_or_else(|| format!("Target '{}' not found", target_name))?;
        let team_id = t.teams.iter()
            .find(|tm| tm.name == team_name)
            .ok_or_else(|| format!("Team '{}' not found in '{}'", team_name, target_name))?
            .team_id;
        if t.users.iter().any(|u| u.name == username) {
            return Ok(()); // already exists
        }
        t.users.push(User { name: username.to_string(), team_id });
        self.save()
    }

    pub fn remove_user(&mut self, username: &str, target_name: &str) -> Result<(), String> {
        let t = self.find_target_mut(target_name)
            .ok_or_else(|| format!("Target '{}' not found", target_name))?;
        t.users.retain(|u| u.name != username);
        self.save()
    }

    /// Remove a team from a target and drop every user that belonged to it.
    /// Counterpart to `add_team`; used by the config TUI.
    pub fn remove_team(&mut self, team_name: &str, target_name: &str) -> Result<(), String> {
        let t = self.find_target_mut(target_name)
            .ok_or_else(|| format!("Target '{}' not found", target_name))?;
        let team_id = t.teams.iter()
            .find(|tm| tm.name == team_name)
            .ok_or_else(|| format!("Team '{}' not found in '{}'", team_name, target_name))?
            .team_id;
        t.teams.retain(|tm| tm.name != team_name);
        t.users.retain(|u| u.team_id != team_id);
        self.save()
    }

    /// Create or update one target from a complete `TargetSpec` in a single
    /// operation. Does NOT save — the caller batches `save()` once after all
    /// upserts, avoiding the "one file write per field" problem.
    ///
    /// `merge` controls team/user reconciliation on an existing target:
    /// - `false` (Replace, default): teams/users become exactly what the spec
    ///   declares — anything absent from the spec is dropped.
    /// - `true` (Merge): spec teams/users are added on top; existing teams and
    ///   users are preserved.
    ///
    /// `path`/`end_scan`/`purge_time` are always updated to the spec's values.
    pub fn upsert_target_full(&mut self, spec: &TargetSpec, merge: bool) {
        // Build the fresh teams/users the spec declares, assigning team_ids.
        let mut new_teams: Vec<Team> = Vec::new();
        let mut new_users: Vec<User> = Vec::new();
        let mut next_id: i64 = 1;
        for ts in &spec.teams {
            let team_id = next_id;
            next_id += 1;
            new_teams.push(Team { name: ts.name.clone(), team_id });
            for uname in &ts.users {
                if !new_users.iter().any(|u: &User| u.name == *uname) {
                    new_users.push(User { name: uname.clone(), team_id });
                }
            }
        }

        if let Some(existing) = self.find_target_mut(&spec.name) {
            existing.path = spec.path.clone();
            existing.end_scan = spec.end_scan.clone();
            existing.purge_time = spec.purge_time;
            if merge {
                // Add teams that don't already exist (by name), reusing/allocating ids.
                let mut max_id = existing.teams.iter().map(|t| t.team_id).max().unwrap_or(0);
                for ts in &spec.teams {
                    let team_id = match existing.teams.iter().find(|t| t.name == ts.name) {
                        Some(t) => t.team_id,
                        None => {
                            max_id += 1;
                            existing.teams.push(Team { name: ts.name.clone(), team_id: max_id });
                            max_id
                        }
                    };
                    for uname in &ts.users {
                        if !existing.users.iter().any(|u| u.name == *uname) {
                            existing.users.push(User { name: uname.clone(), team_id });
                        }
                    }
                }
            } else {
                existing.teams = new_teams;
                existing.users = new_users;
            }
        } else {
            self.targets.push(Target {
                name: spec.name.clone(),
                path: spec.path.clone(),
                teams: new_teams,
                users: new_users,
                end_scan: spec.end_scan.clone(),
                purge_time: spec.purge_time,
                ..Default::default()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TargetFile round-trips through target_from_file -> target_to_file with
    /// team_ids assigned internally and users grouped back under their team.
    #[test]
    fn target_file_roundtrip_assigns_and_groups() {
        let tf = TargetFile {
            name: "backend".into(),
            path: "/data/backend".into(),
            teams: vec![
                TeamFile { name: "dev".into(), users: vec!["alice".into(), "bob".into()] },
                TeamFile { name: "ops".into(), users: vec!["carol".into()] },
            ],
            end_scan: Some("20270101".into()),
            purge_time: Some(90),
            tree_map: Some(true),
            level: Some(4),
            workers: None,
            sync_host: Some("h".into()),
            sync_dest_dir: None,
            sync_user: None,
            export_dir: Some("exp".into()),
            webhook_url: Some("https://x/hook".into()),
            sync_pass: Some(true),
        };
        let t = target_from_file(tf);
        // team_ids assigned sequentially from 1; users attached to their team.
        assert_eq!(t.teams.len(), 2);
        assert_eq!(t.teams[0].team_id, 1);
        assert_eq!(t.teams[1].team_id, 2);
        let alice = t.users.iter().find(|u| u.name == "alice").unwrap();
        assert_eq!(alice.team_id, 1);
        let carol = t.users.iter().find(|u| u.name == "carol").unwrap();
        assert_eq!(carol.team_id, 2);

        // Collapse back: users regrouped under their team by team_id, fields kept.
        let back = target_to_file(&t);
        assert_eq!(back.name, "backend");
        assert_eq!(back.teams.len(), 2);
        assert_eq!(back.teams[0].users, vec!["alice".to_string(), "bob".to_string()]);
        assert_eq!(back.teams[1].users, vec!["carol".to_string()]);
        assert_eq!(back.tree_map, Some(true));
        assert_eq!(back.level, Some(4));
        assert_eq!(back.export_dir.as_deref(), Some("exp"));
        assert_eq!(back.webhook_url.as_deref(), Some("https://x/hook"));
        assert_eq!(back.sync_pass, Some(true));
    }

    /// resolved_output_dir passes absolute paths through unchanged and anchors
    /// relative ones under the binary dir (so it never stays cwd-relative).
    #[test]
    fn resolved_output_dir_absolute_vs_relative() {
        let mut cfg = Config::default();

        cfg.output_dir = "/data/reports".into();
        assert_eq!(cfg.resolved_output_dir(), "/data/reports");

        cfg.output_dir = "reports".into();
        let resolved = cfg.resolved_output_dir();
        assert!(std::path::Path::new(&resolved).is_absolute(),
            "relative output_dir must resolve to an absolute path, got {}", resolved);
        assert!(resolved.ends_with("reports"));
        // Anchored to the binary dir, not the current working directory.
        assert_eq!(resolved, Config::base_dir().join("reports").to_string_lossy());
    }

    /// sanitize_stem neutralizes path separators, dots, and control chars so a
    /// target name can never escape the targets/ dir.
    #[test]
    fn sanitize_stem_blocks_traversal() {
        assert_eq!(sanitize_stem("normal"), "normal");
        assert_eq!(sanitize_stem("a/b"), "a_b");
        assert_eq!(sanitize_stem("../etc"), "___etc");
        assert_eq!(sanitize_stem("a\\b"), "a_b");
        assert_eq!(sanitize_stem("with.dot"), "with_dot");
    }

    /// Replace (merge=false) makes teams/users exactly the spec; Merge (true)
    /// adds without dropping existing.
    #[test]
    fn upsert_replace_vs_merge() {
        let mut cfg = Config::default();
        cfg.upsert_target_full(&TargetSpec {
            name: "t".into(), path: "/p".into(),
            teams: vec![TeamSpec { name: "dev".into(), users: vec!["alice".into()] }],
            end_scan: None, purge_time: None,
        }, false);
        assert_eq!(cfg.targets[0].users.len(), 1);

        // Merge: add ops+bob, keep dev+alice.
        cfg.upsert_target_full(&TargetSpec {
            name: "t".into(), path: "/p".into(),
            teams: vec![TeamSpec { name: "ops".into(), users: vec!["bob".into()] }],
            end_scan: None, purge_time: None,
        }, true);
        let t = &cfg.targets[0];
        assert_eq!(t.teams.len(), 2);
        assert!(t.users.iter().any(|u| u.name == "alice"));
        assert!(t.users.iter().any(|u| u.name == "bob"));

        // Replace: only frontend/carol remains.
        cfg.upsert_target_full(&TargetSpec {
            name: "t".into(), path: "/p2".into(),
            teams: vec![TeamSpec { name: "frontend".into(), users: vec!["carol".into()] }],
            end_scan: None, purge_time: None,
        }, false);
        let t = &cfg.targets[0];
        assert_eq!(t.path, "/p2");
        assert_eq!(t.teams.len(), 1);
        assert_eq!(t.teams[0].name, "frontend");
        assert_eq!(t.users.len(), 1);
        assert_eq!(t.users[0].name, "carol");
    }
}
