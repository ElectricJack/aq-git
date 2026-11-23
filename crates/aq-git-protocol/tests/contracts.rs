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
        if !matches_literal(schema, value) || !self.check_composition(schema, value, file) {
            return false;
        }
        match value {
            Value::Object(object) => self.check_object(schema, object, file),
            Value::Array(items) => self.check_array(schema, items, file),
            Value::String(text) => check_string(schema, text),
            Value::Number(_) => value
                .as_f64()
                .is_none_or(|n| within(n, schema["minimum"].as_f64(), schema["maximum"].as_f64())),
            _ => true,
        }
    }

    fn check_composition(&self, schema: &Value, value: &Value, file: &str) -> bool {
        let check = |child: &Value| self.check(child, value, file);
        if schema["oneOf"]
            .as_array()
            .is_some_and(|opts| opts.iter().filter(|item| check(item)).count() != 1)
        {
            return false;
        }
        if schema["anyOf"]
            .as_array()
            .is_some_and(|opts| !opts.iter().any(check))
        {
            return false;
        }
        if schema["allOf"]
            .as_array()
            .is_some_and(|opts| !opts.iter().all(check))
        {
            return false;
        }
        if schema.get("not").is_some_and(check) {
            return false;
        }
        if schema.get("if").is_some_and(check) {
            return schema.get("then").is_none_or(check);
        }
        true
    }

    fn check_object(
        &self,
        schema: &Value,
        object: &serde_json::Map<String, Value>,
        file: &str,
    ) -> bool {
        let has_required = schema["required"].as_array().is_none_or(|required| {
            required
                .iter()
                .all(|key| object.contains_key(key.as_str().unwrap()))
        });
        if !has_required
            || schema["minProperties"]
                .as_u64()
                .is_some_and(|min| object.len() < min as usize)
        {
            return false;
        }
        object
            .iter()
            .all(|(key, value)| match schema["properties"].get(key) {
                Some(property) => self.check(property, value, file),
                None => schema["additionalProperties"] != false,
            })
    }

    fn check_array(&self, schema: &Value, values: &[Value], file: &str) -> bool {
        if schema["maxItems"]
            .as_u64()
            .is_some_and(|max| values.len() > max as usize)
        {
            return false;
        }
        schema
            .get("items")
            .is_none_or(|item| values.iter().all(|value| self.check(item, value, file)))
    }
}

fn matches_literal(schema: &Value, value: &Value) -> bool {
    if schema
        .get("const")
        .is_some_and(|constant| constant != value)
    {
        return false;
    }
    if schema["enum"]
        .as_array()
        .is_some_and(|options| !options.contains(value))
    {
        return false;
    }
    let Some(kind) = schema.get("type") else {
        return true;
    };
    let matches = |kind: &Value| matches_type(kind.as_str(), value);
    match kind.as_array() {
        Some(kinds) => kinds.iter().any(matches),
        None => matches(kind),
    }
}

fn matches_type(kind: Option<&str>, value: &Value) -> bool {
    if kind == Some("integer") {
        return value.is_i64() || value.is_u64();
    }
    matches!(
        (kind, value),
        (Some("object"), Value::Object(_))
            | (Some("array"), Value::Array(_))
            | (Some("string"), Value::String(_))
            | (Some("null"), Value::Null)
            | (Some("boolean"), Value::Bool(_))
            | (Some("number"), Value::Number(_))
    )
}

fn within<T: PartialOrd>(value: T, min: Option<T>, max: Option<T>) -> bool {
    min.is_none_or(|min| value >= min) && max.is_none_or(|max| value <= max)
}

fn check_string(schema: &Value, text: &str) -> bool {
    if !within(
        text.chars().count() as u64,
        schema["minLength"].as_u64(),
        schema["maxLength"].as_u64(),
    ) {
        return false;
    }
    if schema["format"] == "uuid" && validate_id(text).is_err() {
        return false;
    }
    schema["pattern"]
        .as_str()
        .is_none_or(|pattern| matches_pattern(pattern, text))
}

fn matches_pattern(pattern: &str, text: &str) -> bool {
    match pattern {
        "^[0-9a-f]{40}$" => {
            text.len() == 40 && text.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
        }
        "^refs/heads/" => {
            text.starts_with("refs/heads/") && aq_git_protocol::validate_ref(text).is_ok()
        }
        "^refs/heads/.+/$" => text
            .strip_prefix("refs/heads/")
            .and_then(|rest| rest.strip_suffix('/'))
            .is_some_and(|middle| !middle.is_empty()),
        _ if pattern.starts_with("^[0-9a-f]{8}") => validate_id(text).is_ok(),
        _ if pattern.starts_with("^[^\\x00-") => !text.chars().any(char::is_control),
        _ => panic!("Extend contract checker for schema pattern {pattern}"),
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
fn assert_strict_parameters(catalog: &Catalogue, operation: &str, value: &Value) {
    let request_schema = &catalog.0["request.schema.json"];
    let rule = request_schema["allOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| {
            s["if"]["properties"]["operation"]["const"] == operation
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
        operation
    );
}

fn assert_error_examples(root: &Path, catalog: &Catalogue) {
    let response_schema = &catalog.0["response.schema.json"];
    for entry in std::fs::read_dir(root.join("examples/errors")).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        let response: Response = serde_json::from_slice(&bytes).unwrap();
        assert!(!response.ok && response.error.is_some());
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(catalog.check(response_schema, &value, "response.schema.json"));
    }
}

fn assert_response_example(root: &Path, operation: &str, catalog: &Catalogue) {
    let response_schema = &catalog.0["response.schema.json"];
    let response_path = root
        .join("examples")
        .join(format!("{}.response.json", operation.replace('.', "-")));
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

#[test]
fn every_operation_has_strict_parameter_schema_and_wire_fixtures() {
    let (root, catalog) = catalog();
    for schema in catalog.0.values() {
        supported_schema(schema);
    }
    let request_schema = &catalog.0["request.schema.json"];
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
        assert_strict_parameters(&catalog, &request.operation, &value);
        assert_response_example(&root, &request.operation, &catalog);
    }
    assert_eq!(
        operations.len(),
        34,
        "update fixture catalogue when operation list changes"
    );
    assert_error_examples(&root, &catalog);
}
