use crate::pyo3::exceptions::PyRuntimeError;
use crate::pyo3::prelude::*;
use std::fs;
use std::path::Path;

pub fn recreate_dir(path: &Path) -> PyResult<()> {
    if path.exists() {
        fs::remove_dir_all(path)
            .map_err(|e| PyRuntimeError::new_err(format!("rm dir {}: {}", path.display(), e)))?;
    }
    fs::create_dir_all(path)
        .map_err(|e| PyRuntimeError::new_err(format!("mkdir {}: {}", path.display(), e)))
}

pub fn ensure_dir(path: &Path) -> PyResult<()> {
    fs::create_dir_all(path)
        .map_err(|e| PyRuntimeError::new_err(format!("mkdir {}: {}", path.display(), e)))
}
