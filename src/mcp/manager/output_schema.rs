//! Compile advertised MCP output contracts before publishing a tool catalog.
//!
//! A schema is untrusted server data, not authority to fetch another URL or
//! open a local file. Compiled validators are shared with each in-flight call
//! so a catalog refresh cannot change the contract of an already-sent request.

use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;

use jsonschema::{PatternOptions, Retrieve, Uri, Validator};
use serde_json::Value;

use super::{Result, tool_err};

// Admission limits, not promises about provider context limits or a hard
// wall-clock bound on all possible JSON Schema evaluations.
const MAX_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_SCHEMA_NODES: usize = 16 * 1024;
const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_RESULT_NODES: usize = 64 * 1024;
const MAX_RESULT_DEPTH: usize = 64;
const MAX_REGEX_BYTES: usize = 1024 * 1024;
const MAX_REGEX_DFA_BYTES: usize = 256 * 1024;
const MAX_REGEX_BACKTRACKS: usize = 10_000;

struct NoExternalSchemas;

impl Retrieve for NoExternalSchemas {
    fn retrieve(
        &self,
        _uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        // Do not include the URI: it can contain credentials, a private path,
        // or terminal controls. This also overrides feature-unified resolvers.
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "external MCP schema retrieval is disabled",
        )
        .into())
    }
}

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "schema byte limit exceeded")
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Walk without building a second tree or a breadth-sized work queue. The
/// depth check runs before recursion, and each value consumes a shared node.
fn within_shape_budget(value: &Value, depth: usize, nodes: &mut usize) -> bool {
    if depth == 0 || *nodes == 0 {
        return false;
    }
    *nodes -= 1;
    match value {
        Value::Array(values) => values
            .iter()
            .all(|value| within_shape_budget(value, depth - 1, nodes)),
        Value::Object(values) => values
            .values()
            .all(|value| within_shape_budget(value, depth - 1, nodes)),
        _ => true,
    }
}

struct CompiledOutputSchema {
    schema: Value,
    validator: Validator,
}

/// An admitted, compiled MCP `outputSchema` and its exact advertised JSON.
/// Cloning retains the same validator; it does not recompile server data.
#[derive(Clone)]
pub struct McpOutputSchema(Arc<CompiledOutputSchema>);

impl fmt::Debug for McpOutputSchema {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Metadata debugging must not dump an untrusted schema or its examples.
        formatter
            .debug_struct("McpOutputSchema")
            .finish_non_exhaustive()
    }
}

impl McpOutputSchema {
    /// Compile a server's object-valued JSON Schema with local reference
    /// support, no external retrieval, and bounded regex backtracking/memory.
    ///
    /// # Errors
    /// Rejects malformed, oversized, excessively nested, or unresolved schemas.
    /// Error messages deliberately omit server-provided schema values and URIs.
    pub fn compile(schema: &Value) -> Result<Self> {
        if !schema.is_object() {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "tools/list outputSchema must be an object",
            ));
        }
        let mut nodes = MAX_SCHEMA_NODES;
        if !within_shape_budget(schema, MAX_SCHEMA_DEPTH, &mut nodes) {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "tools/list outputSchema exceeds the schema depth or node limit",
            ));
        }
        serde_json::to_writer(&mut ByteBudget(MAX_SCHEMA_BYTES), schema).map_err(|_| {
            tool_err(
                "MCP_PROTOCOL",
                "tools/list outputSchema exceeds the schema byte limit",
            )
        })?;
        let validator = jsonschema::options()
            .with_retriever(NoExternalSchemas)
            .with_pattern_options(
                PatternOptions::fancy_regex()
                    .backtrack_limit(MAX_REGEX_BACKTRACKS)
                    .size_limit(MAX_REGEX_BYTES)
                    .dfa_size_limit(MAX_REGEX_DFA_BYTES),
            )
            .build(schema)
            .map_err(|_| {
                tool_err(
                    "MCP_PROTOCOL",
                    "tools/list outputSchema is invalid, unsupported, or references an unavailable schema",
                )
            })?;
        Ok(Self(Arc::new(CompiledOutputSchema {
            schema: schema.clone(),
            validator,
        })))
    }

    /// The exact JSON advertised by the server, without a second copy per call.
    #[must_use]
    pub fn schema(&self) -> &Value {
        &self.0.schema
    }

    /// Check a completed tool response. An explicit tool execution error is
    /// not a successful structured result and need not match the success schema.
    /// Text that happens to contain JSON is never substituted for a missing
    /// `structuredContent` field.
    ///
    /// # Errors
    /// Returns `MCP_OUTPUT_INVALID` for a malformed response or a successful
    /// result that does not satisfy its advertised contract. This is a definite
    /// response, not an indeterminate delivery: callers must not replay it.
    pub fn validate_result(&self, result: &Value) -> Result<()> {
        let Some(result) = result.as_object() else {
            return Err(output_error("tool result must be an object"));
        };
        match result.get("isError") {
            Some(Value::Bool(true)) => return Ok(()),
            None | Some(Value::Bool(false)) => {}
            Some(_) => return Err(output_error("isError must be a boolean when present")),
        }
        let structured = result
            .get("structuredContent")
            .filter(|value| value.is_object())
            .ok_or_else(|| {
                output_error("successful tool result requires object structuredContent")
            })?;
        let mut nodes = MAX_RESULT_NODES;
        if !within_shape_budget(structured, MAX_RESULT_DEPTH, &mut nodes) {
            return Err(output_error(
                "structuredContent exceeds the depth or node limit",
            ));
        }
        if !self.0.validator.is_valid(structured) {
            return Err(output_error(
                "structuredContent does not match outputSchema",
            ));
        }
        Ok(())
    }
}

fn output_error(reason: &str) -> super::Error {
    tool_err(
        "MCP_OUTPUT_INVALID",
        format!(
            "{reason}; the call was not replayed and remote side effects may already have occurred"
        ),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn contract() -> McpOutputSchema {
        McpOutputSchema::compile(&json!({
            "type": "object",
            "properties": {"count": {"type": "integer", "minimum": 0}},
            "required": ["count"],
            "additionalProperties": false
        }))
        .expect("valid schema")
    }

    #[test]
    fn discovery_retains_and_compiles_output_schema() {
        let schema = json!({"type":"object", "required":["count"]});
        let metas = super::super::parse_tool_list(&json!({"tools":[{
            "name":"count", "inputSchema":{"type":"object"}, "outputSchema":schema
        }]}))
        .expect("catalog");
        let compiled = metas[0]
            .output_schema
            .as_ref()
            .expect("advertised output schema");
        assert_eq!(compiled.schema(), &schema);
        assert!(
            compiled
                .validate_result(&json!({"structuredContent":{"count":1}}))
                .is_ok()
        );
        assert!(
            compiled
                .validate_result(&json!({"structuredContent":{}}))
                .is_err()
        );
    }

    #[test]
    fn discovery_keeps_unstructured_tools_without_an_output_contract() {
        let metas = super::super::parse_tool_list(&json!({"tools":[{
            "name":"plain", "inputSchema":{"type":"object"}
        }]}))
        .expect("catalog");
        assert!(metas[0].output_schema.is_none());
    }

    #[test]
    fn discovery_rejects_malformed_output_schema_instead_of_dropping_it() {
        for schema in [
            Value::Null,
            json!(false),
            json!([]),
            json!("object"),
            json!({"type":7}),
        ] {
            let error = super::super::parse_tool_list(&json!({"tools":[{
                "name":"bad", "inputSchema":{}, "outputSchema":schema
            }]}))
            .expect_err("invalid contract must reject the catalog");
            assert!(error.to_string().contains("MCP_PROTOCOL"));
        }
    }

    #[test]
    fn output_schema_clones_share_the_compiled_validator() {
        let original = contract();
        let cloned = original.clone();
        assert!(Arc::ptr_eq(&original.0, &cloned.0));
    }

    #[test]
    fn local_schema_references_are_supported() {
        let schema = McpOutputSchema::compile(&json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "$defs":{"count":{"type":"integer", "minimum":0}},
            "type":"object",
            "properties":{"count":{"$ref":"#/$defs/count"}},
            "required":["count"]
        }))
        .expect("local refs require no I/O");
        assert!(
            schema
                .validate_result(&json!({"structuredContent":{"count":3}}))
                .is_ok()
        );
        assert!(
            schema
                .validate_result(&json!({"structuredContent":{"count":-1}}))
                .is_err()
        );
    }

    #[test]
    fn external_schema_references_and_unknown_meta_schemas_fail_closed() {
        for uri in [
            "https://example.invalid/credential-secret/schema",
            "http://127.0.0.1/credential-secret/schema",
            "file:///credential-secret/schema.json",
        ] {
            for keyword in ["$ref", "$schema"] {
                let mut schema = json!({"type":"object"});
                schema[keyword] = json!(uri);
                let error = McpOutputSchema::compile(&schema).expect_err("no external retrieval");
                let message = error.to_string();
                assert!(message.contains("MCP_PROTOCOL"));
                assert!(!message.contains("credential-secret"));
                assert!(!message.contains(uri));
            }
        }
    }

    #[test]
    fn unresolved_local_reference_is_not_silently_ignored() {
        assert!(McpOutputSchema::compile(&json!({"$ref":"#/$defs/missing"})).is_err());
    }

    #[test]
    fn schema_byte_budget_is_checked_before_compilation() {
        let schema = json!({"type":"object", "description":"x".repeat(MAX_SCHEMA_BYTES)});
        let error = McpOutputSchema::compile(&schema).expect_err("oversized schema");
        assert!(error.to_string().contains("byte limit"));
    }

    #[test]
    fn schema_node_and_depth_budgets_are_checked_before_compilation() {
        let wide = json!({"examples":vec![Value::Null; MAX_SCHEMA_NODES]});
        assert!(
            McpOutputSchema::compile(&wide)
                .expect_err("wide schema")
                .to_string()
                .contains("node limit")
        );
        let mut deep = json!({});
        for _ in 0..MAX_SCHEMA_DEPTH {
            deep = json!({"properties":{"nested":deep}});
        }
        assert!(
            McpOutputSchema::compile(&deep)
                .expect_err("deep schema")
                .to_string()
                .contains("depth")
        );
    }

    #[test]
    fn valid_structured_result_is_not_changed_or_coerced() {
        let result = json!({
            "content":[{"type":"text", "text":"1 item"}],
            "structuredContent":{"count":1},
            "_meta":{"private":"not-model-content"}
        });
        let before = result.clone();
        contract().validate_result(&result).expect("valid response");
        assert_eq!(result, before);
    }

    #[test]
    fn success_requires_real_object_structured_content() {
        for result in [
            json!({"content":[{"type":"text", "text":"{\"count\":1}"}]}),
            json!({"structuredContent":null}),
            json!({"structuredContent":[]}),
            json!({"structuredContent":"{\"count\":1}"}),
            json!({"structuredContent":{"count":"1"}}),
            json!({"structuredContent":{"count":-1}}),
            json!({"structuredContent":{"count":1,"extra":true}}),
            json!({"structuredContent":{}}),
        ] {
            let error = contract()
                .validate_result(&result)
                .expect_err("invalid output");
            assert!(error.to_string().contains("MCP_OUTPUT_INVALID"));
            assert!(error.to_string().contains("not replayed"));
        }
    }

    #[test]
    fn execution_errors_do_not_need_success_shaped_content() {
        for result in [
            json!({"isError":true, "content":[{"type":"text","text":"rate limited"}]}),
            json!({"isError":true, "structuredContent":{"error":"unavailable"}}),
        ] {
            contract()
                .validate_result(&result)
                .expect("preserve server execution error");
        }
    }

    #[test]
    fn malformed_error_flags_cannot_bypass_validation() {
        for flag in [json!("true"), json!(1), Value::Null, json!({}), json!([])] {
            let error = contract()
                .validate_result(&json!({"isError":flag}))
                .expect_err("isError must really be a boolean");
            assert!(error.to_string().contains("MCP_OUTPUT_INVALID"));
        }
    }

    #[test]
    fn structured_result_admission_is_bounded() {
        let schema = McpOutputSchema::compile(&json!({})).expect("permissive schema");
        let wide = json!({"structuredContent":{"items":vec![Value::Null; MAX_RESULT_NODES]}});
        assert!(
            schema
                .validate_result(&wide)
                .expect_err("too many result nodes")
                .to_string()
                .contains("node limit")
        );
        let mut deep = json!({});
        for _ in 0..MAX_RESULT_DEPTH {
            deep = json!({"nested":deep});
        }
        assert!(
            schema
                .validate_result(&json!({"structuredContent":deep}))
                .expect_err("too deep")
                .to_string()
                .contains("depth")
        );
    }

    #[test]
    fn validation_errors_do_not_echo_private_output_or_schema_values() {
        let error = contract()
            .validate_result(&json!({"structuredContent":{"count":"private-secret-value"}}))
            .expect_err("wrong type");
        assert!(!error.to_string().contains("private-secret-value"));
        let schema = McpOutputSchema::compile(&json!({
            "type":"object", "examples":[{"secret":"private-schema-example"}]
        }))
        .expect("schema");
        assert!(!format!("{schema:?}").contains("private-schema-example"));
    }

    #[test]
    fn shape_budget_accepts_exact_node_limit_and_rejects_one_more() {
        let value = json!({"a":[1,2]});
        assert!(within_shape_budget(&value, 3, &mut 4));
        assert!(!within_shape_budget(&value, 3, &mut 3));
        assert!(!within_shape_budget(&value, 2, &mut 4));
    }
}
