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
}
