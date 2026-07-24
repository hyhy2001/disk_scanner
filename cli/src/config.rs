use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_output_dir")]
    pub output_dir: String,
    #[serde(default = "default_workers")]
    pub workers: String,
    #[serde(default)]
    pub max_parallel_devices: i64,
    #[serde(default = "default_nfs_parallel")]
    pub nfs_parallel: i64,
    #[serde(default)]
    pub targets: Vec<Target>,
}

fn default_output_dir() -> String { "reports".into() }
fn default_workers() -> String { "auto".into() }
fn default_nfs_parallel() -> i64 { 4 }

impl Default for Config {
    fn default() -> Self {
        Self {
            output_dir: default_output_dir(),
            workers: default_workers(),
            max_parallel_devices: 0,
            nfs_parallel: default_nfs_parallel(),
            targets: Vec::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut p = cwd.join("duscan.toml");
        if p.exists() {
            return p;
        }
        p = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("duscan")
            .join("config.toml");
        if p.exists() {
            return p;
        }
        // Default: next to binary
        if let Ok(exe) = std::env::current_exe() {
            let sibling = exe.parent().unwrap_or(&cwd).join("duscan.toml");
            if sibling.exists() {
                return sibling;
            }
        }
        cwd.join("duscan.toml")
    }

    pub fn load() -> Self {
        let p = Self::path();
        if !p.exists() {
            return Config::default();
        }
        match fs::read_to_string(&p) {
            Ok(content) => {
                toml::from_str(&content).unwrap_or_else(|e| {
                    eprintln!("Warning: config parse error ({}): {}", p.display(), e);
                    Config::default()
                })
            }
            Err(e) => {
                eprintln!("Warning: config read error ({}): {}", p.display(), e);
                Config::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir: {}", e))?;
        }
        let content = toml::to_string_pretty(self).map_err(|e| format!("serialize: {}", e))?;
        // Atomic write: write to .tmp then rename
        let tmp = p.with_extension("toml.tmp");
        fs::write(&tmp, &content).map_err(|e| format!("write: {}", e))?;
        fs::rename(&tmp, &p).map_err(|e| format!("rename: {}", e))?;
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
            teams: Vec::new(),
            users: Vec::new(),
            end_scan,
            purge_time,
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
            });
        }
    }
}
