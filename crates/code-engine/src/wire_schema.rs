//! Strict-wire lowering and inverse null normalization, adapted from Cockpit.
use serde_json::{Map, Value, json};
use std::collections::HashSet;
pub(crate) fn for_responses(schema: &Value) -> Value {
    let mut transformed = schema.clone();
    lower_to_strict(&mut transformed);
    make_optional_properties_nullable(&mut transformed);
    close_object_schemas(&mut transformed);
    require_all_properties(&mut transformed);
    transformed
}

/// Lower a schema for a non-strict Chat Completions wire: only the shapes no
/// non-strict endpoint accepts are touched.
pub fn for_lenient(schema: &Value) -> Value {
    if !needs_lenient_lowering(schema) {
        return schema.clone();
    }
    let mut transformed = schema.clone();
    lower_to_lenient(&mut transformed);
    transformed
}

/// Keywords every wire accepts once [`for_responses`] has lowered `oneOf` to
/// `anyOf`. Canonical built-in schemas must stay inside this set (plus
/// `x-cockpit-*` annotations); the per-tool lint in the tests enforces it.
pub const PORTABLE_KEYWORDS: &[&str] = &[
    // Structure.
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "anyOf",
    "oneOf",
    "$defs",
    "$ref",
    // Annotations.
    "description",
    "title",
    "default",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "$comment",
    // Strings.
    "minLength",
    "maxLength",
    "pattern",
    "format",
    // Numbers.
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    // Arrays.
    "minItems",
    "maxItems",
];

/// `format` values OpenAI strict mode accepts; any other value is a 400.
pub const STRICT_STRING_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "ipv4",
    "ipv6",
    "uuid",
];

const NUMERIC_KEYWORDS: &[&str] = &[
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
];

/// The largest numeric keyword magnitude Cockpit sends. OpenAI rejects
/// `-2^63`; JSON-safe integers are exact in every JavaScript gateway.
pub const MAX_PORTABLE_SCHEMA_NUMBER: f64 = 9_007_199_254_740_991.0;

/// The type strict mode gets for a schema that canonically accepts anything.
/// Strict decoding cannot express "any value"; scalars are the widest closed
/// choice that never forces a fabricated object shape.
fn strict_any_type() -> Value {
    json!(["string", "number", "boolean"])
}

/// Whether a numeric keyword value can travel on every wire.
pub(crate) fn is_portable_schema_number(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|number| number.abs() <= MAX_PORTABLE_SCHEMA_NUMBER)
}

fn is_extension_keyword(key: &str) -> bool {
    key.starts_with("x-")
}

fn remove_unportable_numbers(object: &mut Map<String, Value>) {
    for keyword in NUMERIC_KEYWORDS {
        if object
            .get(*keyword)
            .is_some_and(|value| !is_portable_schema_number(value))
        {
            object.remove(*keyword);
        }
    }
}

fn is_array_schema(object: &Map<String, Value>) -> bool {
    match object.get("type") {
        Some(Value::String(kind)) => kind == "array",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("array")),
        _ => false,
    }
}

/// Subschema positions this module lowers. Positions only some wires reject
/// (for example `not`) are removed by the strict pass before it descends, so
/// they are deliberately absent here.
const MAP_POSITIONS: &[&str] = &["properties", "$defs", "definitions"];
const SINGLE_POSITIONS: &[&str] = &["items", "additionalProperties"];
const LIST_POSITIONS: &[&str] = &["anyOf", "oneOf", "allOf"];

fn for_each_subschema(object: &mut Map<String, Value>, mut visit: impl FnMut(&mut Value)) {
    for (key, child) in object.iter_mut() {
        let key = key.as_str();
        match child {
            Value::Object(entries) if MAP_POSITIONS.contains(&key) => {
                entries.values_mut().for_each(&mut visit);
            }
            Value::Object(_) if SINGLE_POSITIONS.contains(&key) => visit(child),
            Value::Array(variants) if LIST_POSITIONS.contains(&key) => {
                variants.iter_mut().for_each(&mut visit);
            }
            _ => {}
        }
    }
}

fn subschemas(object: &Map<String, Value>) -> impl Iterator<Item = &Value> {
    object.iter().flat_map(|(key, child)| {
        let key = key.as_str();
        let children: Box<dyn Iterator<Item = &Value>> = match child {
            Value::Object(entries) if MAP_POSITIONS.contains(&key) => Box::new(entries.values()),
            Value::Object(_) if SINGLE_POSITIONS.contains(&key) => Box::new(std::iter::once(child)),
            Value::Array(variants) if LIST_POSITIONS.contains(&key) => Box::new(variants.iter()),
            _ => Box::new(std::iter::empty()),
        };
        children
    })
}

fn needs_lenient_lowering(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    NUMERIC_KEYWORDS.iter().any(|keyword| {
        object
            .get(*keyword)
            .is_some_and(|value| !is_portable_schema_number(value))
    }) || (is_array_schema(object) && !object.contains_key("items"))
        || subschemas(object).any(needs_lenient_lowering)
}

fn lower_to_lenient(schema: &mut Value) {
    let Value::Object(object) = schema else {
        return;
    };
    remove_unportable_numbers(object);
    if is_array_schema(object) && !object.contains_key("items") {
        object.insert("items".to_string(), json!({}));
    }
    for_each_subschema(object, lower_to_lenient);
}

/// Rewrite one schema into OpenAI's strict subset. Only drops keywords the
/// subset rejects (relaxing the advertised contract) and gives untyped leaves
/// a concrete type; the canonical schema still governs argument validation.
fn lower_to_strict(schema: &mut Value) {
    let Value::Object(object) = schema else {
        return;
    };

    merge_all_of(object);
    // OpenAI strict forbids siblings next to `$ref`.
    if object.contains_key("$ref") {
        object.retain(|key, _| key == "$ref");
        return;
    }
    if let Some(one_of) = object.remove("oneOf") {
        match (object.get_mut("anyOf"), one_of) {
            (Some(Value::Array(any_of)), Value::Array(mut variants)) => {
                any_of.append(&mut variants)
            }
            (_, one_of) => {
                object.insert("anyOf".to_string(), one_of);
            }
        }
    }

    object.retain(|key, value| {
        if is_extension_keyword(key) {
            return true;
        }
        if !PORTABLE_KEYWORDS.contains(&key.as_str()) && key != "definitions" {
            return false;
        }
        match key.as_str() {
            "format" => value
                .as_str()
                .is_some_and(|format| STRICT_STRING_FORMATS.contains(&format)),
            "items" => value.is_object(),
            _ => true,
        }
    });
    remove_unportable_numbers(object);

    if !object.contains_key("type") && !object.contains_key("anyOf") {
        if let Some(constant) = object.get("const") {
            if let Some(kind) = json_type_name(constant) {
                object.insert("type".to_string(), Value::String(kind.to_string()));
            }
        } else if object.contains_key("properties") {
            object.insert("type".to_string(), json!("object"));
        } else if !object.contains_key("enum") {
            object.insert("type".to_string(), strict_any_type());
        }
    }
    if is_array_schema(object) && !object.contains_key("items") {
        object.insert("items".to_string(), json!({}));
    }

    for_each_subschema(object, lower_to_strict);
}

/// Fold `allOf` (rejected by strict mode) into its parent. A single branch
/// merges exactly; several object branches merge their properties and
/// `required` lists (the first branch wins a property collision, which only
/// relaxes the contract). Branches that are not plain object fragments are
/// dropped.
fn merge_all_of(object: &mut Map<String, Value>) {
    let Some(Value::Array(branches)) = object.remove("allOf") else {
        return;
    };
    let single = branches.len() == 1;
    for branch in branches {
        let Value::Object(branch) = branch else {
            continue;
        };
        if !single && !branch.contains_key("properties") {
            continue;
        }
        for (key, value) in branch {
            match (key.as_str(), object.get_mut(&key)) {
                ("properties", Some(Value::Object(existing))) => {
                    if let Value::Object(incoming) = value {
                        for (name, property) in incoming {
                            existing.entry(name).or_insert(property);
                        }
                    }
                }
                ("required", Some(Value::Array(existing))) => {
                    if let Value::Array(incoming) = value {
                        for name in incoming {
                            if !existing.contains(&name) {
                                existing.push(name);
                            }
                        }
                    }
                }
                (_, Some(_)) => {}
                (_, None) => {
                    object.insert(key, value);
                }
            }
        }
    }
}

fn json_type_name(value: &Value) -> Option<&'static str> {
    match value {
        Value::String(_) => Some("string"),
        Value::Bool(_) => Some("boolean"),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some("integer"),
        Value::Number(_) => Some("number"),
        Value::Null => Some("null"),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn close_object_schemas(schema: &mut Value) {
    let Value::Object(object) = schema else {
        return;
    };

    if is_object_schema(object) {
        object
            .entry("properties".to_string())
            .or_insert_with(|| json!({}));
        object.insert("additionalProperties".to_string(), Value::Bool(false));
    }

    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for property in properties.values_mut() {
            close_object_schemas(property);
        }
    }
    if let Some(items) = object.get_mut("items") {
        close_object_schemas(items);
    }
    if let Some(Value::Object(definitions)) = object.get_mut("$defs") {
        for definition in definitions.values_mut() {
            close_object_schemas(definition);
        }
    }
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(variants)) = object.get_mut(combinator) {
            for variant in variants {
                close_object_schemas(variant);
            }
        }
    }
}

fn is_object_schema(object: &serde_json::Map<String, Value>) -> bool {
    object.contains_key("properties")
        || matches!(object.get("type"), Some(Value::String(kind)) if kind == "object")
        || object
            .get("type")
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("object")))
}

fn make_optional_properties_nullable(schema: &mut Value) {
    let Value::Object(object) = schema else {
        return;
    };

    let required: HashSet<String> = object
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();

    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for (name, property) in properties {
            make_optional_properties_nullable(property);
            if !required.contains(name) {
                add_null_arm(property);
            }
        }
    }

    if let Some(items) = object.get_mut("items") {
        make_optional_properties_nullable(items);
    }
    if let Some(Value::Object(definitions)) = object.get_mut("$defs") {
        for definition in definitions.values_mut() {
            make_optional_properties_nullable(definition);
        }
    }
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(variants)) = object.get_mut(combinator) {
            for variant in variants {
                make_optional_properties_nullable(variant);
            }
        }
    }
}

fn add_null_arm(schema: &mut Value) {
    if explicitly_allows_null(schema) {
        return;
    }

    let Value::Object(object) = schema else {
        let original = std::mem::take(schema);
        *schema = json!({ "anyOf": [original, { "type": "null" }] });
        return;
    };

    if object.contains_key("$ref") || object.contains_key("enum") || object.contains_key("const") {
        let original = std::mem::take(schema);
        *schema = json!({ "anyOf": [original, { "type": "null" }] });
        return;
    }

    match object.get_mut("type") {
        Some(Value::String(kind)) => {
            let kind = std::mem::take(kind);
            object.insert(
                "type".to_string(),
                Value::Array(vec![Value::String(kind), Value::String("null".to_string())]),
            );
            return;
        }
        Some(Value::Array(kinds)) => {
            kinds.push(Value::String("null".to_string()));
            return;
        }
        _ => {}
    }

    if let Some(Value::Array(variants)) = object.get_mut("anyOf") {
        variants.push(json!({ "type": "null" }));
        return;
    }
    if let Some(Value::Array(variants)) = object.get_mut("oneOf") {
        variants.push(json!({ "type": "null" }));
        return;
    }

    // `$ref` cannot have siblings on OpenAI's strict subset, and enum-only or
    // otherwise untyped schemas have no safe type to infer. Wrap the complete
    // schema so rig can sanitize each arm without dropping the reference.
    let original = std::mem::take(schema);
    *schema = json!({ "anyOf": [original, { "type": "null" }] });
}

fn explicitly_allows_null(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    match object.get("type") {
        Some(Value::String(kind)) if kind == "null" => return true,
        Some(Value::Array(kinds))
            if kinds
                .iter()
                .any(|kind| kind.as_str().is_some_and(|kind| kind == "null")) =>
        {
            return true;
        }
        _ => {}
    }
    if object
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(Value::is_null))
        || object.get("const").is_some_and(Value::is_null)
    {
        return true;
    }
    ["anyOf", "oneOf"].into_iter().any(|key| {
        object
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|variants| variants.iter().any(explicitly_allows_null))
    })
}

/// Remove object-property nulls introduced only for strict-wire optionality.
///
/// Strict lowering adds the null arm to optional properties at every depth,
/// including objects inside schema-described arrays (`targets[]`,
/// `questions[]`), so normalization descends the same way. Array elements are
/// otherwise model data: a null element, and any element whose `items` schema
/// does not describe an object, are left untouched.
pub(crate) fn strip_wire_nulls(canonical_schema: &Value, mut args: Value) -> Value {
    strip_object_nulls(canonical_schema, canonical_schema, &mut args);
    args
}

fn strip_object_nulls(root: &Value, schema: &Value, value: &mut Value) {
    let Value::Object(arguments) = value else {
        return;
    };

    let keys: Vec<String> = arguments.keys().cloned().collect();
    for key in keys {
        let property_schema = find_property_schema(root, schema, &key, 0);
        let remove = arguments.get(&key).is_some_and(Value::is_null)
            && !property_schema.is_some_and(|property| schema_allows_null(root, property));
        if remove {
            arguments.remove(&key);
            continue;
        }

        let Some(property_schema) = property_schema else {
            continue;
        };
        if let Some(child) = arguments.get_mut(&key) {
            strip_nested_nulls(root, property_schema, child, 0);
        }
    }
}

fn strip_nested_nulls(root: &Value, schema: &Value, value: &mut Value, depth: usize) {
    if depth > 64 {
        return;
    }
    match value {
        Value::Object(_) => strip_object_nulls(root, schema, value),
        Value::Array(elements) => {
            let Some(items) = find_items_schema(root, schema, 0) else {
                return;
            };
            if !describes_object(root, items, 0) {
                return;
            }
            for element in elements {
                strip_nested_nulls(root, items, element, depth + 1);
            }
        }
        _ => {}
    }
}

/// The `items` schema governing an array position, through `$ref` and
/// combinators.
fn find_items_schema<'a>(root: &'a Value, schema: &'a Value, depth: usize) -> Option<&'a Value> {
    if depth > 64 {
        return None;
    }
    let object = schema.as_object()?;
    if let Some(items) = object.get("items").filter(|items| items.is_object()) {
        return Some(items);
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve_local_ref(root, reference)
    {
        return find_items_schema(root, target, depth + 1);
    }
    ["anyOf", "oneOf", "allOf"]
        .into_iter()
        .find_map(|combinator| {
            object
                .get(combinator)
                .and_then(Value::as_array)
                .and_then(|variants| {
                    variants
                        .iter()
                        .find_map(|variant| find_items_schema(root, variant, depth + 1))
                })
        })
}

/// Whether a schema (or one of its variants) describes object properties, so
/// the strict lowering would have made its optional fields nullable.
fn describes_object(root: &Value, schema: &Value, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    let Some(object) = schema.as_object() else {
        return false;
    };
    if object.contains_key("properties") {
        return true;
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve_local_ref(root, reference)
    {
        return describes_object(root, target, depth + 1);
    }
    ["anyOf", "oneOf", "allOf"].into_iter().any(|combinator| {
        object
            .get(combinator)
            .and_then(Value::as_array)
            .is_some_and(|variants| {
                variants
                    .iter()
                    .any(|variant| describes_object(root, variant, depth + 1))
            })
    })
}

fn find_property_schema<'a>(
    root: &'a Value,
    schema: &'a Value,
    property: &str,
    depth: usize,
) -> Option<&'a Value> {
    if depth > 64 {
        return None;
    }
    let object = schema.as_object()?;
    if let Some(found) = object
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(property))
    {
        return Some(found);
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve_local_ref(root, reference)
    {
        return find_property_schema(root, target, property, depth + 1);
    }
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(found) = object
            .get(combinator)
            .and_then(Value::as_array)
            .and_then(|variants| {
                variants
                    .iter()
                    .find_map(|variant| find_property_schema(root, variant, property, depth + 1))
            })
        {
            return Some(found);
        }
    }
    None
}

fn schema_allows_null(root: &Value, schema: &Value) -> bool {
    schema_allows_null_inner(root, schema, &mut HashSet::new())
}

fn schema_allows_null_inner(root: &Value, schema: &Value, seen_refs: &mut HashSet<String>) -> bool {
    if explicitly_allows_null(schema) {
        return true;
    }
    if let Some(object) = schema.as_object() {
        for combinator in ["anyOf", "oneOf"] {
            if object
                .get(combinator)
                .and_then(Value::as_array)
                .is_some_and(|variants| {
                    variants
                        .iter()
                        .any(|variant| schema_allows_null_inner(root, variant, seen_refs))
                })
            {
                return true;
            }
        }
        if let Some(variants) = object.get("allOf").and_then(Value::as_array)
            && !variants.is_empty()
            && variants
                .iter()
                .all(|variant| schema_allows_null_inner(root, variant, seen_refs))
        {
            return true;
        }
    }
    let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
        return false;
    };
    if !seen_refs.insert(reference.to_string()) {
        return false;
    }
    resolve_local_ref(root, reference)
        .is_some_and(|target| schema_allows_null_inner(root, target, seen_refs))
}

fn resolve_local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        Some(root)
    } else {
        root.pointer(pointer)
    }
}

/// Rig supplied this final step in Cockpit; our explicit wire owns it here.
fn require_all_properties(schema: &mut Value) {
    let Value::Object(object) = schema else {
        return;
    };
    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        let keys: Vec<_> = properties.keys().cloned().collect();
        for property in properties.values_mut() {
            require_all_properties(property);
        }
        object.insert("required".into(), json!(keys));
    }
    for_each_subschema(object, require_all_properties);
}
