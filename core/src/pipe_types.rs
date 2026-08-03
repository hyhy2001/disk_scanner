// ─────────────────────────────────────────────────────────────────────────────
// Shared types, constants, and utility functions
// ─────────────────────────────────────────────────────────────────────────────

use std::fs;

/// Constants
pub const FILE_PART_RECORDS: usize = 100_000;

/// Internal event structures

#[allow(dead_code)]
#[derive(Default)]
pub struct UserOutputMeta {
    pub team_id: String,
    pub total_dirs: i64,
    pub total_used: i64,
}

#[allow(dead_code)]
pub struct UserBuildResult {
    pub username: String,
    pub team_id: String,
    pub total_dirs: i64,
    pub total_files: i64,
    pub total_used: i64,
}

#[derive(Clone)]
pub struct PermissionEvent {
    pub uid: u32,
    pub kind: String,
    pub errcode: String,
    pub path: String,
}

pub struct ScanEvent {
    pub uid: u32,
    pub size: u64,
    pub path: String,
}

pub struct DirAggEvent {
    pub uid: u32,
    pub size: i64,
    pub path: String,
}

pub struct DirOwnerEvent {
    pub uid: u32,
    pub path: String,
}

/// Parse a scan event line from TSV (F\tuid\tsize\tpath)
/// Parse a permission issue line from TSV (P\tuid\tkind\terrcode\tpath)
pub fn parse_permission_line(line: &str) -> Option<PermissionEvent> {
    let mut parts = line.splitn(5, '\t');
    if parts.next()? != "P" {
        return None;
    }
    let uid: u32 = parts.next()?.trim().parse().ok()?;
    let kind = parts.next()?.to_string();
    let errcode = parts.next()?.to_string();
    let path = parts.next().map(str::to_string).unwrap_or_default();
    Some(PermissionEvent {
        uid,
        kind,
        errcode,
        path,
    })
}

/// Extract parent path (removes trailing slash)
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed == "/" || trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(idx) => Some(trimmed[..idx].to_string()),
        None => None,
    }
}

/// Get file extension (without dot)
pub fn extension_for_path(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Get RSS memory in MB (Linux /proc)
#[cfg(target_os = "linux")]
pub fn get_rss_mb() -> f64 {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                if let Some(kb) = line.split_whitespace().nth(1) {
                    if let Ok(val) = kb.parse::<f64>() {
                        return val / 1024.0;
                    }
                }
            }
        }
    }
    0.0
}

/// Read RSS memory in MB (macOS fallback — returns 0.0)
#[cfg(not(target_os = "linux"))]
pub fn get_rss_mb() -> f64 {
    0.0
}

/// Memory stats from `/proc/self/status` in MB.
/// `vsz_mb` is the virtual address space (`VmSize`) — the value `RLIMIT_AS`
/// caps, so under LSF/cgroup limits it is what kills the process even when
/// RSS looks small. `peak_mb` is the high-water mark (`VmPeak`), the largest
/// virtual size the process has ever held.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemStats {
    pub rss_mb: f64,
    pub vsz_mb: f64,
    pub peak_mb: f64,
}

#[cfg(target_os = "linux")]
pub fn get_mem_stats() -> MemStats {
    let mut s = MemStats::default();
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            let field = line.split_whitespace().collect::<Vec<_>>();
            if field.len() >= 2 {
                let kb: f64 = match field[1].parse() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let mb = kb / 1024.0;
                match field[0] {
                    "VmRSS:" => s.rss_mb = mb,
                    "VmSize:" => s.vsz_mb = mb,
                    "VmPeak:" => s.peak_mb = mb,
                    _ => {}
                }
            }
        }
    }
    s
}

#[cfg(not(target_os = "linux"))]
pub fn get_mem_stats() -> MemStats {
    MemStats::default()
}

/// Formatted `[MEM checkpoint]` line for --debug output: RSS | VSZ | Peak.
/// VSZ (`VmSize`) is what RLIMIT_AS caps, so a crash with low RSS but a
/// capped virtual limit shows up here as VSZ approaching the ceiling.
pub fn mem_checkpoint(label: &str) -> String {
    let s = get_mem_stats();
    format!(
        "[MEM checkpoint] {}: RSS {:.1} MB | VSZ {:.1} MB | VmPeak {:.1} MB",
        label, s.rss_mb, s.vsz_mb, s.peak_mb
    )
}
