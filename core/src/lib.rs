// Shadow the pyo3 crate with stub types.
// Other modules use `use crate::pyo3::*` instead of `use pyo3::*`.
#[path = "pyo3.rs"]
mod pyo3;

pub mod db_writer;
pub mod pipe_events;
pub mod pipe_io;
pub mod pipe_permission;
pub mod pipe_treemap;
pub mod pipe_types;
pub mod report_history;
pub mod report_pipeline;
pub mod scan_constants;
pub mod scan_core;
pub mod scan_orchestrator;
pub mod scan_state;
pub mod scan_utils;

/// Sanitise a raw byte string so the result is valid UTF-8 JSON:
/// replace surrogates and control chars with U+FFFD.
pub(crate) fn sanitise_path(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c == '\u{FFFD}' || (c.is_control() && c != '\t') {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}
