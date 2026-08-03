// TreeMap path helpers — used by report_pipeline.rs to walk path strings.
// Heavy lifting (shard NDJSON, pid_seek index, etc.) is gone — output is now
// SQLite via `db_writer::build_treemap_db`.

pub fn normalize_root(root_dir: &str) -> String {
    let trimmed = root_dir.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn tm_parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed == "/" || trimmed.is_empty() {
        return "/";
    }
    match trimmed.rfind('/') {
        Some(0) => "/",
        Some(idx) => &trimmed[..idx],
        None => "/",
    }
}

pub fn tm_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed == "/" || trimmed.is_empty() {
        return trimmed.to_string();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}
