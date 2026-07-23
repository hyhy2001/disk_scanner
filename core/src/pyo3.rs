#![allow(non_camel_case_types, unused)]

use std::collections::HashMap;

pub mod exceptions {
    pub struct PyRuntimeError;
    impl PyRuntimeError {
        pub fn new_err(msg: String) -> String { msg }
    }
}

pub mod prelude {
    pub use super::*;
}

pub mod types {
    use std::collections::HashMap;

    pub type PyResult<T> = Result<T, String>;
    pub type PyErr = String;

    #[derive(Clone)]
    pub struct Python;
    impl Python {
        pub fn with_gil<F, R>(f: F) -> R where F: FnOnce(Python) -> R {
            f(Python)
        }
        pub fn allow_threads<F, R>(self, f: F) -> R where F: FnOnce() -> R {
            f()
        }
        pub fn check_signals(&self) -> PyResult<()> { Ok(()) }
    }

    pub struct PyDict {
        pub map: HashMap<String, serde_json::Value>,
    }
    impl PyDict {
        pub fn new(_py: Python) -> Self {
            Self { map: HashMap::new() }
        }
    }

    impl PyDict {
        pub fn set_item<K, V>(&mut self, key: K, value: V) -> PyResult<()>
        where K: Into<String> + std::fmt::Display,
              V: std::fmt::Display,
        {
            let k = key.into();
            let v_str = value.to_string();
            // Parse as number first, fall back to string
            self.map.insert(k, parse_json_value(&v_str));
            Ok(())
        }

        pub fn set_item_json<V: Into<serde_json::Value>>(&mut self, key: &str, value: V) -> PyResult<()> {
            self.map.insert(key.to_string(), value.into());
            Ok(())
        }
    }

    fn parse_json_value(s: &str) -> serde_json::Value {
        if let Ok(v) = s.parse::<i64>() { return serde_json::json!(v); }
        if let Ok(v) = s.parse::<u64>() { return serde_json::json!(v); }
        if let Ok(v) = s.parse::<f64>() { return serde_json::json!(v); }
        serde_json::Value::String(s.to_string())
    }

    impl From<PyDict> for serde_json::Value {
        fn from(d: PyDict) -> Self {
            serde_json::Value::Object(d.map.into_iter().collect())
        }
    }

    pub struct PyList {
        pub items: Vec<serde_json::Value>,
    }
    impl PyList {
        pub fn empty(_py: Python) -> Self {
            Self { items: Vec::new() }
        }
    }

    impl PyList {
        pub fn append<V: std::fmt::Display>(&mut self, value: V) -> PyResult<()> {
            let v_str = value.to_string();
            self.items.push(parse_json_value(&v_str));
            Ok(())
        }
    }

    impl From<PyList> for serde_json::Value {
        fn from(l: PyList) -> Self {
            serde_json::Value::Array(l.items)
        }
    }

    pub type PyObject = serde_json::Value;

    pub trait PyAnyMethods {
        fn extract<T: serde::de::DeserializeOwned>(&self) -> PyResult<T>;
        fn downcast<T>(&self) -> PyResult<&T>;
    }

    impl PyAnyMethods for serde_json::Value {
        fn extract<T: serde::de::DeserializeOwned>(&self) -> PyResult<T> {
            serde_json::from_value(self.clone()).map_err(|e| e.to_string())
        }
        fn downcast<T>(&self) -> PyResult<&T> {
            Err("PyAny::downcast not available".to_string())
        }
    }
}

pub use types::*;
pub use exceptions::PyRuntimeError;
