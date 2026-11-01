//! Small canonical-schema repair pipeline adapted from FlyCockpit's Cockpit.
//! Repairs schema-declared shape; filesystem normalization stays in the shared resolver.
use crate::{schema::ToolCall, tools::ToolDefinition, wire_schema};
use serde_json::{Value, json};

pub fn repair(call: &mut ToolCall, definitions: &[ToolDefinition]) -> Result<(), String> {
    let normalized = call
        .name
        .trim()
        .trim_matches(['\'', '"', '<', '>'])
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if definitions.iter().any(|d| d.name == normalized) {
        call.name = normalized;
    }
    let definition = definitions
        .iter()
        .find(|d| d.name == call.name)
        .ok_or_else(|| {
            format!(
                "unknown tool {}. Use an available tool: {}",
                call.name,
                definitions
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let schema = &definition.schema;
    if let Value::String(s) = &call.arguments
        && let Ok(v @ Value::Object(_)) = serde_json::from_str(s)
    {
        call.arguments = v;
    }
    call.arguments = wire_schema::strip_wire_nulls(schema, call.arguments.clone());
    let args = call
        .arguments
        .as_object_mut()
        .ok_or("arguments must be an object")?;
    // Aliases belong to canonical schemas, so renamed tools behave identically.
    if let Some(properties) = schema["properties"].as_object() {
        for (key, prop) in properties {
            let present: Vec<_> = prop["x-cockpit-aliases"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|alias| args.contains_key(*alias))
                .collect();
            if args.contains_key(key) {
                for alias in present {
                    if args.get(alias) == args.get(key) {
                        args.remove(alias);
                    }
                }
            } else if present.len() == 1 {
                let value = args.remove(present[0]).unwrap();
                args.insert(key.clone(), value);
            }
        }
    }
    if let Some(properties) = schema["properties"].as_object() {
        for (key, prop) in properties {
            let Some(value) = args.get_mut(key) else {
                continue;
            };
            match prop["type"].as_str().unwrap_or("") {
                "integer" | "number" => {
                    if let Some(s) = value.as_str()
                        && let Ok(parsed @ Value::Number(_)) =
                            serde_json::from_str::<Value>(s.trim())
                    {
                        *value = parsed;
                    }
                }
                "array" => {
                    if let Some(s) = value.as_str() {
                        *value = serde_json::from_str::<Value>(s)
                            .ok()
                            .filter(Value::is_array)
                            .unwrap_or_else(|| json!([s]));
                    }
                }
                _ => {}
            }
        }
    }
    validate(&call.arguments, schema, "arguments")
}
pub(crate) fn validate(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let kind = schema["type"].as_str().unwrap_or("");
    let valid = match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.is_u64() || value.is_i64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        _ => true,
    };
    if !valid {
        return Err(format!("{path} must be {kind}"));
    }
    if let Some(values) = schema["enum"].as_array()
        && !values.contains(value)
    {
        return Err(format!("{path} has an unsupported value"));
    }
    if let Some(n) = value.as_f64()
        && (schema["minimum"].as_f64().is_some_and(|min| n < min)
            || schema["maximum"].as_f64().is_some_and(|max| n > max))
    {
        return Err(format!("{path} is outside the allowed range"));
    }
    if let Some(map) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    return Err(format!("missing {path}.{key}"));
                }
            }
        }
        for (key, value) in map {
            match schema["properties"].get(key) {
                Some(s) => validate(value, s, &format!("{path}.{key}"))?,
                None if schema["additionalProperties"] == false => {
                    return Err(format!("unknown argument {path}.{key}"));
                }
                _ => {}
            }
        }
    }
    if let Some(array) = value.as_array()
        && let Some(items) = schema.get("items")
    {
        for value in array {
            validate(value, items, path)?;
        }
    }
    Ok(())
}
