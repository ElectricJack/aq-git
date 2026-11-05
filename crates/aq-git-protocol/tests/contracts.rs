//! Structural catalogue checks for the deliberately small schema vocabulary used
//! here. This is not a general JSON Schema implementation. Domain semantics are
//! verified by core fixtures; schema changes introducing new keywords must extend
//! this checker or select a standards-compliant development validator.
use aq_git_protocol::{Request, Response, decode_request, validate_id};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};
struct Catalogue(BTreeMap<String, Value>);
impl Catalogue {
    fn check(&self, schema: &Value, value: &Value, file: &str) -> bool {
        if let Some(reference) = schema["$ref"].as_str() {
            let (name, pointer) = reference.split_once('#').unwrap_or((reference, ""));
            let name = if name.is_empty() { file } else { name };
            let Some(root) = self.0.get(name) else {
                return false;
            };
            let Some(target) = root.pointer(pointer) else {
                return false;
            };
            return self.check(target, value, name);
        }
        if let Some(constant) = schema.get("const")
            && constant != value
        {
            return false;
        }
        if let Some(options) = schema["enum"].as_array()
            && !options.contains(value)
        {
            return false;
        }
        if let Some(kind) = schema.get("type") {
            let matches = |kind: &str| match kind {
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "null" => value.is_null(),
                "boolean" => value.is_boolean(),
                "integer" => value.is_i64() || value.is_u64(),
                "number" => value.is_number(),
                _ => false,
            };
            if !kind.as_str().is_some_and(matches)
                && !kind
                    .as_array()
                    .is_some_and(|a| a.iter().any(|k| k.as_str().is_some_and(matches)))
            {
                return false;
            }
        }
        if let Some(options) = schema["oneOf"].as_array()
            && options
                .iter()
                .filter(|s| self.check(s, value, file))
                .count()
                != 1
        {
            return false;
        }
        if let Some(options) = schema["anyOf"].as_array()
            && !options.iter().any(|s| self.check(s, value, file))
        {
            return false;
        }
        if let Some(options) = schema["allOf"].as_array()
            && !options.iter().all(|s| self.check(s, value, file))
        {
            return false;
        }
        if let Some(not) = schema.get("not")
            && self.check(not, value, file)
        {
            return false;
        }
        if let Some(condition) = schema.get("if")
            && self.check(condition, value, file)
            && let Some(then) = schema.get("then")
            && !self.check(then, value, file)
        {
            return false;
        }
        if let Some(object) = value.as_object() {
            if let Some(required) = schema["required"].as_array()
                && required
                    .iter()
                    .any(|k| !object.contains_key(k.as_str().unwrap()))
            {
                return false;
            }
            if schema["minProperties"]
                .as_u64()
                .is_some_and(|n| object.len() < (n as usize))
            {
                return false;
            }
            for (key, value) in object {
                if let Some(property) = schema["properties"].get(key) {
                    if !self.check(property, value, file) {
                        return false;
                    }
                } else if schema["additionalProperties"] == false {
                    return false;
                }
            }
        }
        if let Some(array) = value.as_array() {
            if schema["maxItems"]
                .as_u64()
                .is_some_and(|n| array.len() > n as usize)
            {
                return false;
            }
            if let Some(item) = schema.get("items")
                && !array.iter().all(|v| self.check(item, v, file))
            {
                return false;
            }
        }
        if let Some(string) = value.as_str() {
            if schema["minLength"]
                .as_u64()
                .is_some_and(|n| string.chars().count() < n as usize)
                || schema["maxLength"]
                    .as_u64()
                    .is_some_and(|n| string.chars().count() > n as usize)
            {
                return false;
            }
            if schema["format"] == "uuid" && validate_id(string).is_err() {
                return false;
            }
            if let Some(pattern) = schema["pattern"].as_str() {
                let valid = if pattern == "^[0-9a-f]{40}$" {
                    string.len() == 40
                        && string
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                } else if pattern.starts_with("^[0-9a-f]{8}") {
                    validate_id(string).is_ok()
                } else if pattern == "^refs/heads/" {
                    string.starts_with("refs/heads/")
                        && aq_git_protocol::validate_ref(string).is_ok()
                } else if pattern == "^refs/heads/.+/$" {
                    string.len() > "refs/heads/".len() + 1
                        && string.starts_with("refs/heads/")
                        && string.ends_with('/')
                } else if pattern.starts_with("^[^\\x00-") {
                    !string.chars().any(char::is_control)
                } else {
                    panic!("Extend contract checker for schema pattern {pattern}")
                };
                if !valid {
                    return false;
                }
            }
        }
        if let Some(number) = value.as_f64()
            && (schema["minimum"].as_f64().is_some_and(|n| number < n)
                || schema["maximum"].as_f64().is_some_and(|n| number > n))
        {
            return false;
        }
        true
    }
}
fn supported_schema(schema: &Value) {
    let Some(object) = schema.as_object() else {
        return;
    };
    let known = [
        "$schema",
        "$id",
        "title",
        "description",
        "$ref",
        "type",
        "properties",
        "additionalProperties",
        "required",
        "$defs",
        "const",
        "enum",
        "oneOf",
        "anyOf",
        "allOf",
        "not",
        "if",
        "then",
        "items",
        "maxItems",
        "minProperties",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
        "format",
        "pattern",
    ];
    for (key, value) in object {
        assert!(
            known.contains(&key.as_str()),
            "Extend contract checker for schema keyword {key}"
        );
        match key.as_str() {
            "properties" | "$defs" => {
                for property in value.as_object().unwrap().values() {
                    supported_schema(property);
                }
            }
            "oneOf" | "anyOf" | "allOf" => {
                for option in value.as_array().unwrap() {
                    supported_schema(option);
                }
            }
            "not" | "if" | "then" | "items" => supported_schema(value),
            _ => {}
        }
    }
}
fn catalog() -> (std::path::PathBuf, Catalogue) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1");
    let schemas = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.ends_with(".schema.json") {
                return None;
            }
            Some((
                name,
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap(),
            ))
        })
        .collect();
    (root, Catalogue(schemas))
}
#[test]
fn every_operation_has_strict_parameter_schema_and_wire_fixtures() {
    let (root, catalog) = catalog();
    for schema in catalog.0.values() {
        supported_schema(schema);
    }
    let request_schema = &catalog.0["request.schema.json"];
    let response_schema = &catalog.0["response.schema.json"];
    let mut operations = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("examples")).unwrap() {
        let path = entry.unwrap().path();
        if !path.to_string_lossy().ends_with(".request.json") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let request: Request = decode_request(&bytes).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            catalog.check(request_schema, &value, "request.schema.json"),
            "{} violates request schema",
            path.display()
        );
        operations.insert(request.operation.clone(), ());
        let rule = request_schema["allOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| {
                s["if"]["properties"]["operation"]["const"] == request.operation
                    && s["then"]["properties"]["params"]["$ref"].is_string()
            })
            .unwrap();
        let schema = &rule["then"]["properties"]["params"];
        assert!(
            schema["$ref"].is_string(),
            "missing operation params schema"
        );
        let mut invalid = value.clone();
        invalid["params"]["unexpected_parameter"] = Value::Bool(true);
        assert!(
            !catalog.check(request_schema, &invalid, "request.schema.json"),
            "{} permits unknown parameters",
            request.operation
        );
        let response_path = root.join("examples").join(format!(
            "{}.response.json",
            request.operation.replace('.', "-")
        ));
        let bytes = std::fs::read(&response_path).unwrap();
        let response: Response = serde_json::from_slice(&bytes).unwrap();
        assert!(response.ok);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            catalog.check(response_schema, &value, "response.schema.json"),
            "{} violates response schema",
            response_path.display()
        );
    }
    assert_eq!(
        operations.len(),
        31,
        "update fixture catalogue when operation list changes"
    );
    for entry in std::fs::read_dir(root.join("examples/errors")).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        let response: Response = serde_json::from_slice(&bytes).unwrap();
        assert!(!response.ok && response.error.is_some());
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(catalog.check(response_schema, &value, "response.schema.json"));
    }
}
