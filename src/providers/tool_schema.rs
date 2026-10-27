//! Provider wire adaptations. The caller's schema stays intact for local
//! argument validation; ordinary schemas do not need a second owned copy.

use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::BTreeSet;

const COMBINATORS: [&str; 3] = ["allOf", "anyOf", "oneOf"];

/// Messages requires an object at the tool schema root and rejects root
/// combinators. Expose their fields as an object while keeping the exact
/// constraint in the description. This is a permissive wire projection, not
/// a replacement for the original tool's argument validation.
pub(super) fn anthropic_tool_schema(schema: &Value) -> Cow<'_, Value> {
    let Some(root) = schema.as_object() else {
        return Cow::Borrowed(schema);
    };
    if (root.contains_key("type") && !declares_type(root, "object"))
        || !COMBINATORS.iter().any(|key| {
            root.get(*key)
                .and_then(Value::as_array)
                .is_some_and(|variants| !variants.is_empty())
        })
    {
        return Cow::Borrowed(schema);
    }
    // A reference into a removed branch (or a named anchor it may define)
    // would dangle after folding. Keep that unsupported schema intact rather
    // than sending a different, broken contract to the provider.
    if any_schema_node(schema, &references_root_combinator) {
        return Cow::Borrowed(schema);
    }

    let mut wire = root.clone();
    let mut properties = match wire.remove("properties") {
        Some(Value::Object(properties)) => properties,
        None => Map::new(),
        // Do not turn a malformed schema into an apparently valid tool.
        Some(_) => return Cow::Borrowed(schema),
    };
    let mut required = string_entries(wire.get("required"));
    let mut closed = false;
    for key in COMBINATORS {
        if !wire
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|variants| !variants.is_empty())
        {
            continue;
        }
        let Some(constraint) = wire.remove(key) else {
            continue;
        };
        if let Some(folded) = constraint
            .as_array()
            .and_then(|variants| fold_object_variants(variants, key == "allOf"))
        {
            for (name, declaration) in folded.properties {
                // The root and every combinator are conjunctive. An existing
                // declaration is already a necessary condition for valid input.
                properties.entry(name).or_insert(declaration);
            }
            required.extend(folded.required);
            closed |= folded.closed;
        }
        append_constraint_hint(&mut wire, key, &constraint);
    }

    // A removed combinator may have evaluated properties through a reference.
    // Keeping this keyword could now reject fields valid in the original.
    if let Some(constraint) = wire.remove("unevaluatedProperties") {
        append_constraint_hint(&mut wire, "unevaluatedProperties", &constraint);
    }
    for name in &required {
        properties
            .entry(name.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    wire.insert("type".to_string(), Value::String("object".to_string()));
    wire.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        wire.insert(
            "required".to_string(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    if closed {
        wire.entry("additionalProperties")
            .or_insert(Value::Bool(false));
    }
    Cow::Owned(Value::Object(wire))
}

struct FoldedObject {
    properties: Map<String, Value>,
    required: BTreeSet<String>,
    closed: bool,
}

fn references_root_combinator(schema: &Map<String, Value>) -> bool {
    ["$ref", "$dynamicRef", "$recursiveRef"].iter().any(|key| {
        let Some(fragment) = schema
            .get(*key)
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
        else {
            return false;
        };
        if fragment.is_empty() {
            return false;
        }
        let Some(pointer) = fragment.strip_prefix('/') else {
            return true;
        };
        // URI-encoded fragments need full reference resolution; do not guess.
        pointer.contains('%')
            || COMBINATORS.contains(&pointer.split('/').next().unwrap_or_default())
    })
}

fn foldable_object_variant(variant: &Map<String, Value>) -> bool {
    (!variant.contains_key("type") || declares_type(variant, "object"))
        && variant.get("properties").is_none_or(Value::is_object)
        && ![
            "$ref",
            "$dynamicRef",
            "$recursiveRef",
            "not",
            "if",
            "then",
            "else",
            "allOf",
            "anyOf",
            "oneOf",
            "patternProperties",
            "dependentSchemas",
            "dependencies",
            "unevaluatedProperties",
        ]
        .iter()
        .any(|key| variant.contains_key(*key))
}

fn fold_object_variants(variants: &[Value], conjunctive: bool) -> Option<FoldedObject> {
    let variants = variants
        .iter()
        .map(|variant| {
            variant
                .as_object()
                .filter(|value| foldable_object_variant(value))
        })
        .collect::<Option<Vec<_>>>()?;
    let requirements = variants
        .iter()
        .map(|variant| string_entries(variant.get("required")))
        .collect::<Vec<_>>();
    let mut required = BTreeSet::new();
    if conjunctive {
        for names in &requirements {
            required.extend(names.iter().cloned());
        }
    } else if let Some(first) = requirements.first() {
        required.clone_from(first);
        for names in requirements.iter().skip(1) {
            required.retain(|name| names.contains(name));
        }
    }

    let mut names = required.clone();
    for variant in &variants {
        if let Some(properties) = variant.get("properties").and_then(Value::as_object) {
            names.extend(properties.keys().cloned());
        }
    }
    let mut properties = Map::new();
    let open = Value::Object(Map::new());
    for name in names {
        let mut declarations = Vec::new();
        for variant in &variants {
            let declaration = variant
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|properties| properties.get(&name));
            let declaration = declaration.or_else(|| {
                if conjunctive || variant.get("additionalProperties") == Some(&Value::Bool(false)) {
                    None
                } else {
                    // A union member that does not declare this field can
                    // still accept it. Omitting additionalProperties means
                    // open, just as in JSON Schema; do not narrow that member.
                    Some(variant.get("additionalProperties").unwrap_or(&open))
                }
            });
            if let Some(declaration) = declaration
                && !declarations.contains(declaration)
            {
                declarations.push(declaration.clone());
            }
        }
        if conjunctive {
            declarations.retain(|declaration| !unconstrained_schema(declaration));
        } else if declarations.iter().any(unconstrained_schema) {
            declarations.clear();
        }
        let declaration = match declarations.len() {
            0 => open.clone(),
            1 => declarations.remove(0),
            _ => {
                let mut combination = Map::new();
                combination.insert(
                    if conjunctive { "allOf" } else { "anyOf" }.to_string(),
                    Value::Array(declarations),
                );
                Value::Object(combination)
            }
        };
        properties.insert(name, declaration);
    }
    let is_closed = |variant: &&Map<String, Value>| {
        variant.get("additionalProperties") == Some(&Value::Bool(false))
    };
    let closed = if conjunctive {
        variants.iter().any(is_closed)
    } else {
        variants.iter().all(is_closed)
    };
    Some(FoldedObject {
        properties,
        required,
        closed,
    })
}

fn string_entries(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn unconstrained_schema(schema: &Value) -> bool {
    schema == &Value::Bool(true) || schema.as_object().is_some_and(Map::is_empty)
}

fn append_constraint_hint(schema: &mut Map<String, Value>, key: &str, constraint: &Value) {
    let hint = format!("JSON Schema constraint ({key}): {constraint}");
    if let Some(Value::String(description)) = schema.get_mut("description") {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(&hint);
    } else {
        schema.insert("description".to_string(), Value::String(hint));
    }
}

/// Codex rejects arrays without an explicit `items`. Make the implicit open
/// tail explicit, including nullable arrays and prefix-only tuples. Other
/// Responses backends may reject boolean schemas, so only Codex calls this.
///
/// An `items: true` also marks every item evaluated. If `unevaluatedItems` occurs
/// anywhere in the tool, references or composition can make that observable;
/// leave the whole schema intact instead of weakening those constraints.
pub(super) fn codex_tool_schema(schema: &Value) -> Cow<'_, Value> {
    if any_schema_node(schema, &|node| node.contains_key("unevaluatedItems"))
        || !any_schema_node(schema, &itemless_array)
    {
        return Cow::Borrowed(schema);
    }
    let mut wire = schema.clone();
    repair_array_items(&mut wire);
    Cow::Owned(wire)
}

fn declares_type(schema: &Map<String, Value>, expected: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(name)) => name == expected,
        Some(Value::Array(names)) => names.iter().any(|name| name.as_str() == Some(expected)),
        _ => false,
    }
}

fn itemless_array(schema: &Map<String, Value>) -> bool {
    !schema.contains_key("items")
        && (declares_type(schema, "array")
            || (!schema.contains_key("type")
                && schema.get("prefixItems").is_some_and(Value::is_array)))
}

enum ChildKind {
    Map,
    Array,
    Schema,
    SchemaOrTuple,
    Literal,
}

fn child_kind(key: &str) -> ChildKind {
    match key {
        "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas"
        | "dependencies" => ChildKind::Map,
        "allOf" | "anyOf" | "oneOf" | "prefixItems" => ChildKind::Array,
        "items" => ChildKind::SchemaOrTuple,
        "additionalProperties"
        | "additionalItems"
        | "contains"
        | "propertyNames"
        | "unevaluatedProperties"
        | "unevaluatedItems"
        | "not"
        | "if"
        | "then"
        | "else"
        | "contentSchema" => ChildKind::Schema,
        _ => ChildKind::Literal,
    }
}

fn any_schema_node(schema: &Value, predicate: &impl Fn(&Map<String, Value>) -> bool) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    if predicate(schema) {
        return true;
    }
    schema.iter().any(|(key, child)| match child_kind(key) {
        ChildKind::Map => child
            .as_object()
            .is_some_and(|map| map.values().any(|value| any_schema_node(value, predicate))),
        ChildKind::Array => child
            .as_array()
            .is_some_and(|array| array.iter().any(|value| any_schema_node(value, predicate))),
        ChildKind::SchemaOrTuple if child.is_array() => child
            .as_array()
            .is_some_and(|array| array.iter().any(|value| any_schema_node(value, predicate))),
        ChildKind::Schema | ChildKind::SchemaOrTuple => any_schema_node(child, predicate),
        ChildKind::Literal => false,
    })
}

fn repair_array_items(schema: &mut Value) {
    let Some(schema) = schema.as_object_mut() else {
        return;
    };
    if itemless_array(schema) {
        schema.insert("items".to_string(), Value::Bool(true));
    }
    for (key, child) in schema {
        match child_kind(key) {
            ChildKind::Map => {
                if let Some(map) = child.as_object_mut() {
                    for value in map.values_mut() {
                        repair_array_items(value);
                    }
                }
            }
            ChildKind::Array => {
                if let Some(array) = child.as_array_mut() {
                    for value in array {
                        repair_array_items(value);
                    }
                }
            }
            ChildKind::SchemaOrTuple if child.is_array() => {
                if let Some(array) = child.as_array_mut() {
                    for value in array {
                        repair_array_items(value);
                    }
                }
            }
            ChildKind::Schema | ChildKind::SchemaOrTuple => repair_array_items(child),
            ChildKind::Literal => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_accepts(schema: &Value, instance: &Value) {
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(schema)
            .expect("valid test schema");
        assert!(validator.is_valid(instance), "{schema} rejected {instance}");
    }

    #[test]
    fn ordinary_anthropic_schemas_remain_borrowed() {
        let schema = json!({
            "type":"object", "properties":{"value":{"anyOf":[{"type":"string"},{"type":"number"}]}},
            "required":["value"], "additionalProperties":false
        });
        assert!(matches!(anthropic_tool_schema(&schema), Cow::Borrowed(_)));
    }

    #[test]
    fn anthropic_root_unions_admit_every_closed_variant() {
        for key in ["anyOf", "oneOf"] {
            let mut schema = json!({"description":"Report the result."});
            schema[key] = json!([
                {"type":"object", "properties":{"kind":{"const":"answer"},"text":{"type":"string"}}, "required":["kind","text"], "additionalProperties":false},
                {"type":"object", "properties":{"kind":{"const":"failure"},"retry":{"type":"boolean"}}, "required":["kind","retry"], "additionalProperties":false}
            ]);
            let original = schema.clone();
            let wire = anthropic_tool_schema(&schema);
            assert_eq!(wire["type"], "object");
            assert!(wire.get(key).is_none());
            assert_eq!(wire["required"], json!(["kind"]));
            assert_eq!(wire["additionalProperties"], false);
            assert!(
                wire["description"]
                    .as_str()
                    .unwrap()
                    .contains("Report the result.")
            );
            for input in [
                json!({"kind":"answer","text":"done"}),
                json!({"kind":"failure","retry":true}),
            ] {
                assert_accepts(&schema, &input);
                assert_accepts(&wire, &input);
            }
            assert_eq!(schema, original);
            assert!(matches!(anthropic_tool_schema(&wire), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn anthropic_intersections_keep_required_fields_and_shared_constraints() {
        let schema = json!({
            "type":"object", "properties":{"id":{"type":"string"}}, "required":["id"],
            "allOf":[
                {"type":"object", "properties":{"count":{"type":"integer","minimum":1}}, "required":["count"]},
                {"type":"object", "properties":{"count":{"type":"integer","maximum":3},"label":{"type":"string"}}, "required":["label"]}
            ]
        });
        let wire = anthropic_tool_schema(&schema);
        assert_eq!(wire["required"], json!(["count", "id", "label"]));
        assert_eq!(wire["properties"]["id"], schema["properties"]["id"]);
        assert_eq!(
            wire["properties"]["count"]["allOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let input = json!({"id":"job","count":2,"label":"done","extra":true});
        assert_accepts(&schema, &input);
        assert_accepts(&wire, &input);
    }

    #[test]
    fn anthropic_open_union_members_do_not_narrow_sibling_fields() {
        for additional in [None, Some(json!(true)), Some(json!({"type":"number"}))] {
            let mut alternative = json!({"type":"object", "properties":{"kind":{"const":"other"}}, "required":["kind"]});
            if let Some(additional) = additional {
                alternative["additionalProperties"] = additional;
            }
            let schema = json!({"anyOf":[
                {"type":"object", "properties":{"kind":{"const":"text"},"value":{"type":"string"}}, "required":["kind","value"], "additionalProperties":false},
                alternative
            ]});
            let wire = anthropic_tool_schema(&schema);
            for input in [
                json!({"kind":"text","value":"done"}),
                json!({"kind":"other","value":42,"extra":7}),
            ] {
                assert_accepts(&schema, &input);
                assert_accepts(&wire, &input);
            }
        }
    }

    #[test]
    fn anthropic_referenced_variants_keep_an_open_root_and_the_exact_hint() {
        let schema = json!({
            "description":"Choose one result.",
            "$defs":{"Result":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}},
            "oneOf":[{"$ref":"#/$defs/Result"},{"type":"object","properties":{"failed":{"const":true}},"required":["failed"]}],
            "unevaluatedProperties":false
        });
        let wire = anthropic_tool_schema(&schema);
        let input = json!({"value":"done"});
        assert_accepts(&schema, &input);
        assert_accepts(&wire, &input);
        assert_eq!(wire["properties"], json!({}));
        assert!(wire.get("unevaluatedProperties").is_none());
        let description = wire["description"].as_str().unwrap();
        assert!(description.contains("#/$defs/Result"));
        assert!(description.contains("unevaluatedProperties"));
    }

    #[test]
    fn anthropic_does_not_create_dangling_references_into_removed_branches() {
        let schema = json!({
            "type":"object",
            "properties":{"alias":{"$ref":"#/anyOf/0/properties/value"}},
            "anyOf":[{"type":"object","properties":{"value":{"type":"string"}}}]
        });
        assert_accepts(&schema, &json!({"alias":"valid","value":"valid"}));
        assert!(matches!(anthropic_tool_schema(&schema), Cow::Borrowed(_)));

        let anchored = json!({
            "$defs":{"Value":{"$anchor":"Value","type":"string"}},
            "anyOf":[{"type":"object","properties":{"value":{"$ref":"#Value"}}}]
        });
        assert_accepts(&anchored, &json!({"value":"valid"}));
        assert!(matches!(anthropic_tool_schema(&anchored), Cow::Borrowed(_)));
    }

    #[test]
    fn codex_repairs_open_arrays_and_tuple_tails_without_changing_the_schema() {
        for schema in [
            json!({"type":"array"}),
            json!({"type":["array","null"]}),
            json!({"type":"array","prefixItems":[{"type":"string"}]}),
            json!({"prefixItems":[]}),
        ] {
            let original = schema.clone();
            let wire = codex_tool_schema(&schema);
            assert_eq!(wire["items"], true);
            for input in [json!([]), json!(["first",42,null,{"last":true}])] {
                assert_accepts(&schema, &input);
                assert_accepts(&wire, &input);
            }
            assert_eq!(schema, original);
            assert!(matches!(codex_tool_schema(&wire), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn codex_visits_schema_positions_but_never_literal_payloads_or_names() {
        let literal = json!({"type":"array","unevaluatedItems":false});
        let schema = json!({
            "type":"object",
            "$defs":{"unevaluatedItems":{"type":"array"}},
            "properties":{
                "unevaluatedItems":{"type":"array"},
                "nested":{"type":"array","items":{"type":"array"}},
                "record":{"additionalProperties":{"type":"array"}},
                "choice":{"anyOf":[{"type":"array"},{"type":"null"}]},
                "literal":{"const":literal,"default":literal,"enum":[literal],"examples":[literal]}
            }
        });
        let original = schema.clone();
        let wire = codex_tool_schema(&schema);
        assert_eq!(wire["$defs"]["unevaluatedItems"]["items"], true);
        assert_eq!(wire["properties"]["unevaluatedItems"]["items"], true);
        assert_eq!(wire["properties"]["nested"]["items"]["items"], true);
        assert_eq!(
            wire["properties"]["record"]["additionalProperties"]["items"],
            true
        );
        assert_eq!(wire["properties"]["choice"]["anyOf"][0]["items"], true);
        assert_eq!(
            wire["properties"]["literal"],
            schema["properties"]["literal"]
        );
        assert_eq!(schema, original);
    }

    #[test]
    fn codex_preserves_explicit_item_constraints() {
        for items in [
            json!(true),
            json!(false),
            json!({}),
            json!({"type":"number"}),
            json!([{"type":"string"}]),
        ] {
            let schema = json!({"type":"array","items":items});
            assert!(matches!(codex_tool_schema(&schema), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn codex_does_not_change_evaluated_item_semantics_across_references() {
        let schema = json!({
            "type":"object",
            "$defs":{"Tuple":{"type":"array","prefixItems":[{"type":"string"}]}},
            "properties":{
                "row":{"allOf":[{"$ref":"#/$defs/Tuple"}],"unevaluatedItems":false},
                "other":{"type":"array"}
            }
        });
        assert!(matches!(codex_tool_schema(&schema), Cow::Borrowed(_)));
        assert_accepts(&schema, &json!({"row":["one"],"other":[1,2]}));
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        assert!(!validator.is_valid(&json!({"row":["one","two"]})));
    }
}
