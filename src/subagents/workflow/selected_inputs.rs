//! Explicit data handoff across mutually exclusive workflow branches.
//!
//! Selection is based on settled host records, not truthiness, arrival order or
//! a successful parse of child prose. Rejected/pending candidates never become
//! optional just because another candidate happened to finish successfully.

use super::{BTreeMap, Input, Record, Result, State, Value, identifier, invalid};
use std::collections::BTreeSet;

#[derive(Debug)]
pub(super) enum Source {
    Exact(usize),
    Selected(Vec<usize>),
}

impl Source {
    pub(super) fn parse(
        fields: &serde_json::Map<String, Value>,
        ids: &BTreeMap<String, usize>,
        needs: &[usize],
    ) -> Result<Self> {
        let resolve = |value: &Value| -> Result<usize> {
            ids.get(identifier(value)?).copied().filter(|index| needs.contains(index))
                .ok_or_else(|| invalid("inputs may reference only explicitly named direct dependencies"))
        };
        if let Some(value) = fields.get("fromSelected") {
            let candidates = value.as_array().filter(|values| (2..super::MAX_NODES).contains(&values.len()))
                .ok_or_else(|| invalid("fromSelected requires 2-31 distinct direct dependency IDs"))?;
            let mut seen = BTreeSet::new();
            let mut sources = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                let source = resolve(candidate)?;
                if !seen.insert(source) {
                    return Err(invalid("fromSelected must not repeat a dependency"));
                }
                sources.push(source);
            }
            Ok(Self::Selected(sources))
        } else {
            let value = fields.get("from").ok_or_else(|| invalid("input source is missing"))?;
            Ok(Self::Exact(resolve(value)?))
        }
    }

    /// All candidates participate in schema preflight and failure provenance.
    /// Selection never invents graph edges or bypasses the native wave barrier.
    pub(super) fn indices(&self) -> &[usize] {
        match self {
            Self::Exact(index) => std::slice::from_ref(index),
            Self::Selected(indices) => indices,
        }
    }

    pub(super) fn data<'a>(&self, records: &'a [Record]) -> std::result::Result<&'a Value, &'static str> {
        let index = match self {
            Self::Exact(index) => *index,
            Self::Selected(indices) => {
                let mut selected = None;
                let mut count = 0;
                for &index in indices {
                    match records.get(index).map(|record| record.state) {
                        Some(State::Accepted) => {
                            selected = Some(index);
                            count += 1;
                        }
                        Some(State::NotSelected) => {}
                        // Failure has precedence over ambiguity, independent
                        // of the candidate list's order. Never race or retry.
                        _ => return Err("selected_input_dependency_failed"),
                    }
                }
                if count > 1 { return Err("selected_input_ambiguous"); }
                selected.ok_or("selected_input_unavailable")?
            }
        };
        records.get(index).and_then(Record::validated_data)
            .ok_or("dependency_data_unavailable")
    }
}

pub(super) fn validate_join(entry: &Value, inputs: &[Input]) -> Result<()> {
    if inputs.iter().any(|input| matches!(&input.from, Source::Selected(_)))
        && entry.get("join").and_then(Value::as_str) != Some("selected")
    {
        return Err(invalid("fromSelected inputs require explicit join:selected on their workflow node"));
    }
    Ok(())
}

pub(super) fn extend_schema(schema: &mut Value) {
    let inputs = &mut schema["properties"]["workflow"]["items"]["properties"]["inputs"];
    inputs["description"] = serde_json::json!(
        "Named typed data inputs from direct needs predecessors. Each reference has pointer and exactly one source form: from names one required source; fromSelected names 2-31 distinct alternatives and requires join:selected. A selected input needs exactly one accepted alternative and every other alternative intentionally not_selected. Multiple accepted candidates are ambiguous; failed, skipped, pending or unavailable sources are errors, never fallbacks. Every possible source requires an effective output schema before any node launches. JSON pointers address validated native data or complete validated fan-out arrays; null remains a value and missing fields fail. Append literal data under the existing 64 KiB task budget; never change agents, tools, models, workspaces or deadlines from data."
    );
    inputs["additionalProperties"] = serde_json::json!({"oneOf":[
        {"type":"object","additionalProperties":false,"required":["from","pointer"],
            "properties":{
                "from":{"type":"string","minLength":1,"maxLength":64},
                "pointer":{"type":"string","maxLength":1024}
            }},
        {"type":"object","additionalProperties":false,"required":["fromSelected","pointer"],
            "properties":{
                "fromSelected":{"type":"array","minItems":2,"maxItems":31,"uniqueItems":true,
                    "items":{"type":"string","minLength":1,"maxLength":64}},
                "pointer":{"type":"string","maxLength":1024}
            }}
    ]});
}

#[cfg(test)]
mod tests;
