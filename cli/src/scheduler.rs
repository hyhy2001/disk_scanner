use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::config::{Config, Target};

#[derive(Debug, Clone)]
pub struct ScanPlan {
    pub groups: Vec<DeviceGroup>,
    // Retained for callers/inspection even though run_scan reads `out` directly.
    #[allow(dead_code)]
    pub output_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct DeviceGroup {
    pub st_dev: u64,
    pub dev_class: String,
    pub roots: Vec<PhysicalRoot>,
    pub workers: usize,
}

#[derive(Debug, Clone)]
pub struct PhysicalRoot {
    pub scan_path: PathBuf,
    pub views: Vec<TargetView>,
}

#[derive(Debug, Clone)]
pub struct TargetView {
    pub name: String,
    // Full configured path of the view; run_scan uses scan_path + prefix.
    #[allow(dead_code)]
    pub view_path: PathBuf,
    pub prefix: Option<PathBuf>,
    pub team_map: HashMap<String, i64>,
    pub team_names: HashMap<i64, String>,
    #[allow(dead_code)]
    pub output_subdir: PathBuf,
    pub end_scan: Option<String>,
    pub purge_time: Option<i64>,
    // Per-target scan overrides + sync config (None = global default).
    pub tree_map: Option<bool>,
    pub level: Option<i64>,
    pub workers: Option<i64>,
    pub sync_host: Option<String>,
    pub sync_dest_dir: Option<String>,
    pub sync_user: Option<String>,
    pub sync_pass: Option<bool>,
    pub webhook_url: Option<String>,
}

/// Build a device-aware scan plan from config targets.
/// When `explicit_workers` is true, `--workers` was passed on the CLI and
/// device-class caps are skipped — the full budget is used per group.
pub fn build_scan_plan(config: &Config, budget: usize, explicit_workers: bool) -> ScanPlan {
    let output_dir = PathBuf::from(&config.output_dir);

    // Resolve targets: stat each, group by device
    let mount_info = read_mount_info();
    let mut by_dev: HashMap<u64, Vec<&Target>> = HashMap::new();

    for t in &config.targets {
        match fs::metadata(&t.path) {
            Ok(meta) => {
                #[cfg(unix)]
                let dev = {
                    use std::os::unix::fs::MetadataExt;
                    meta.dev()
                };
                #[cfg(not(unix))]
                let dev = 0u64;
                by_dev.entry(dev).or_default().push(t);
            }
            Err(e) => {
                // Surface unreachable targets instead of silently dropping them.
                eprintln!("Warning: cannot stat target '{}' ({}): {} — skipping", t.name, t.path, e);
            }
        }
    }

    let mut groups = Vec::new();
    let n_groups = by_dev.len().max(1);
    let workers_per_group = (budget / n_groups).max(1);

    let nfs_cap = config.nfs_parallel.max(1) as usize;
    let hdd_cap = config.hdd_parallel.max(1) as usize;
    let ssd_cap = config.ssd_parallel.max(0) as usize; // 0 = no cap (use full budget)
    for (dev, targets) in by_dev {
        let class = classify_device(dev, &mount_info);
        let group_workers = if explicit_workers {
            workers_per_group // --workers explicit: bypass device caps
        } else {
            match class.as_str() {
                "nfs" => workers_per_group.min(nfs_cap),
                "hdd" => workers_per_group.min(hdd_cap),
                "ssd" => if ssd_cap > 0 { workers_per_group.min(ssd_cap) } else { workers_per_group },
                _ => workers_per_group,
            }
        };

        // Sort shortest path first for nesting detection
        let mut sorted = targets.clone();
        sorted.sort_by_key(|t| t.path.len());

        let mut roots: Vec<PhysicalRoot> = Vec::new();

        for t in &sorted {
            let tp = PathBuf::from(&t.path);
            let make_view = |prefix: Option<PathBuf>| -> TargetView {
                let team_map: HashMap<String, i64> = t.users.iter()
                    .map(|u| (u.name.clone(), u.team_id))
                    .collect();
                let team_names: HashMap<i64, String> = t.teams.iter()
                    .map(|tm| (tm.team_id, tm.name.clone()))
                    .collect();
                TargetView {
                    name: t.name.clone(),
                    view_path: tp.clone(),
                    prefix,
                    team_map,
                    team_names,
                    output_subdir: output_dir.join(&t.name),
                    end_scan: t.end_scan.clone(),
                    purge_time: t.purge_time,
                    tree_map: t.tree_map,
                    level: t.level,
                    workers: t.workers,
                    sync_host: t.sync_host.clone(),
                    sync_dest_dir: t.sync_dest_dir.clone(),
                    sync_user: t.sync_user.clone(),
                    sync_pass: t.sync_pass,
                    webhook_url: t.webhook_url.clone(),
                }
            };

            let mut placed = false;
            for root in &mut roots {
                if tp.starts_with(&root.scan_path) {
                    let prefix = if tp != root.scan_path { Some(tp.clone()) } else { None };
                    root.views.push(make_view(prefix));
                    placed = true;
                    break;
                }
            }
            if !placed {
                roots.push(PhysicalRoot {
                    scan_path: tp.clone(),
                    views: vec![make_view(None)],
                });
            }
        }

        groups.push(DeviceGroup {
            st_dev: dev,
            dev_class: class,
            roots,
            workers: group_workers,
        });
    }

    ScanPlan { groups, output_dir }
}

/// Parse /proc/self/mountinfo into a map `st_dev -> fstype`. Field 3 of each
/// line is `major:minor` (matches `major(st_dev):minor(st_dev)`); the token
/// right after the " - " separator is the filesystem type. Building this map
/// lets us classify each target by the fstype of *its own* device instead of
/// treating the whole host as NFS the moment any NFS mount exists.
fn read_mount_info() -> HashMap<u64, String> {
    let content = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut map = HashMap::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split(" - ").collect();
        if parts.len() != 2 { continue; }
        let left: Vec<&str> = parts[0].split_whitespace().collect();
        let right: Vec<&str> = parts[1].split_whitespace().collect();
        // left[2] = "major:minor", right[0] = fstype.
        if left.len() < 5 || right.is_empty() { continue; }
        let mm: Vec<&str> = left[2].split(':').collect();
        if mm.len() != 2 { continue; }
        if let (Ok(maj), Ok(min)) = (mm[0].parse::<u32>(), mm[1].parse::<u32>()) {
            let dev = makedev(maj, min);
            // First mount wins for a given device (later bind mounts reuse it).
            map.entry(dev).or_insert_with(|| right[0].to_string());
        }
    }
    map
}

/// Recreate a `dev_t` value from major/minor to match `metadata.dev()`.
fn makedev(major: u32, minor: u32) -> u64 {
    libc::makedev(major, minor) as u64
}

/// Classify a target's device: network filesystems -> "nfs"; local block
/// devices -> "hdd"/"ssd" via /sys/dev/block/<maj>:<min>/queue/rotational
/// (1 = rotational HDD, 0 = SSD). Falls back to "ssd" (least-restrictive) when
/// the fstype or rotational flag can't be determined.
fn classify_device(dev: u64, mounts: &HashMap<u64, String>) -> String {
    let network_fs = ["nfs", "nfs4", "cifs", "smb", "smb3", "fuse", "fuseblk"];
    if let Some(fstype) = mounts.get(&dev) {
        let base = fstype.split('.').next().unwrap_or(fstype);
        if network_fs.iter().any(|n| base == *n || base.starts_with("fuse")) {
            return "nfs".into();
        }
    }
    // Local device: consult rotational flag.
    let maj = libc::major(dev as libc::dev_t);
    let min = libc::minor(dev as libc::dev_t);
    let rot_path = format!("/sys/dev/block/{}:{}/queue/rotational", maj, min);
    match fs::read_to_string(&rot_path) {
        Ok(s) if s.trim() == "1" => "hdd".into(),
        Ok(_) => "ssd".into(),
        Err(_) => "ssd".into(),
    }
}
