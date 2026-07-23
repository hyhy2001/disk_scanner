//! scan_orchestrator.rs — multi-target orchestration.
//! Stubbed out for CLI build. Full implementation requires the `pyo3` feature.

/// Placeholder — real implementation requires pyo3.
pub fn run_scan_plan_impl(
    _plan_json: String,
    _build_treemap: bool,
    _max_level: usize,
    _timestamp: i64,
    _debug: bool,
) -> Result<String, String> {
    Err("scan_orchestrator requires pyo3 feature".to_string())
}
