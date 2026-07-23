#!/usr/bin/env python3
"""Mechanical transformation: replace PyO3 patterns with native Rust equivalents."""

import re
import sys

TRANSFORMS = [
    # Remove pyo3 imports
    (r'use pyo3::.*;', ''),
    # Remove Python<'_> parameter (with comma before)
    (r',?\s*py:\s*Python<'_>', ''),
    (r',?\s*py:\s*Python', ''),
    # PyResult<T> -> Result<T, String>
    (r'PyResult<', 'Result<'),
    (r'>\s*\)\s*->\s*Result<', ') -> Result<'),
    # PyRuntimeError::new_err(s) -> s.to_string()
    (r'PyRuntimeError::new_err\(', ''),
    (r'\)(?=\s*\))?', '.to_string()'),
    # Python::with_gil(|py| { ... }) -> just { ... }
    (r'Python::with_gil\(\|py\|\s*\{', '{'),
    (r'Python::with_gil\(\|_\|', ''),
    (r'\)\s*\)\s*\)\s*\.map_err', ').map_err'),
    # py.allow_threads(move || { ... }) -> just { ... }
    (r'py\.allow_threads\(move\s*\|\|\s*\{', '{'),
    (r'py\.allow_threads\(move\s*\|\|\s*->\s*Result<', '-> Result<'),
    # Remove err on "?") after ))?
]

def transform_content(content):
    for pattern, replacement in TRANSFORMS:
        content = re.sub(pattern, replacement, content)
    return content

if __name__ == '__main__':
    for path in sys.argv[1:]:
        with open(path) as f:
            content = f.read()
        original = content
        content = transform_content(content)
        if content != original:
            with open(path, 'w') as f:
                f.write(content)
            print(f'Fixed: {path}')
        else:
            print(f'Unchanged: {path}')
