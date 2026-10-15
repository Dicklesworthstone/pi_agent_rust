//! Bounded data-driven task expansion through the ordinary wave runner.
//!
//! Source data selects task items, never agents, tools or workspace policies.
//! All item prompts are admitted before this node contributes any native work.

use super::{
    BTreeMap, Input, Node, Record, Result, State, SubagentResult, SubagentTask, Value,
    encoded_size, identifier, invalid, native_state, render_results, validate_pointer,
};
use serde::Serialize;
use serde_json::json;

const MAX_ITEMS: usize = 32;
const MAX_CALLS: usize = 128;
const MAX_DATA_BYTES: usize = 256 * 1024;

pub(super) fn reject_misplaced(input: &Value) -> Result<()> {
    if input.get("forEach").is_some() || ["tasks", "chain"].iter().any(|name| {
        input.get(*name).and_then(Value::as_array)
            .is_some_and(|entries| entries.iter().any(|entry| entry.get("forEach").is_some()))
    }) {
        return Err(invalid("forEach belongs on workflow nodes, not ordinary delegations"));
    }
    Ok(())
}

pub(super) struct Expansion {
    pub(super) from: usize,
    pointer: String,
    name: String,
    max_items: usize,
}

impl Expansion {
    pub(super) fn parse(
        value: Option<&Value>,
        ids: &BTreeMap<String, usize>,
        needs: &[usize],
        inputs: &[Input],
    ) -> Result<Option<Self>> {
        let Some(value) = value else { return Ok(None); };
        let fields = value.as_object().filter(|fields| fields.len() == 4
            && ["from", "pointer", "as", "maxItems"].iter().all(|key| fields.contains_key(*key)))
            .ok_or_else(|| invalid("forEach requires exactly from, pointer, as and maxItems"))?;
        let from = ids.get(identifier(&fields["from"])?).copied()
            .filter(|index| needs.contains(index))
            .ok_or_else(|| invalid("forEach must reference an explicit direct dependency"))?;
        let pointer = fields["pointer"].as_str()
            .ok_or_else(|| invalid("forEach pointer must be a JSON pointer"))?;
        validate_pointer(pointer)?;
        let name = identifier(&fields["as"])?;
        if inputs.len() >= super::MAX_NODES || inputs.iter().any(|input| input.name == name) {
            return Err(invalid("forEach needs an unused input name and room within the 32-input limit"));
        }
        let max_items = fields["maxItems"].as_u64()
            .filter(|count| (1..=MAX_ITEMS as u64).contains(count))
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(|| invalid("forEach maxItems must be an explicit integer from 1 to 32"))?;
        Ok(Some(Self { from, pointer: pointer.to_owned(), name: name.to_owned(), max_items }))
    }

    pub(super) fn tasks(
        &self,
        node: &Node,
        records: &[Record],
    ) -> std::result::Result<Vec<SubagentTask>, &'static str> {
        let data = records.get(self.from).and_then(Record::validated_data)
            .ok_or("dependency_data_unavailable")?;
        let selected = data.pointer(&self.pointer).ok_or("fanout_input_missing")?;
        let items = selected.as_array().ok_or("fanout_input_not_array")?;
        if items.len() > self.max_items { return Err("fanout_item_limit"); }
        // Do not truncate a discovered work list, partially queue its prefix,
        // interpolate values as commands, or allocate a second copy of it.
        // A failure on the last item drops only these unlaunched task values.
        items.iter().map(|item| node.task_with_item(records, Some((&self.name, item)))).collect()
    }
}

pub(super) fn validate_budget(nodes: &[Node]) -> Result<()> {
    let mut total = 0_usize;
    for node in nodes {
        let count = node.fanout.as_ref().map_or(1, |expansion| expansion.max_items);
        total = total.checked_add(count).filter(|total| *total <= MAX_CALLS)
            .ok_or_else(|| invalid("declared workflow expansion exceeds 128 native task instances"))?;
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    index: usize,
    state: State,
    hub_id: Option<String>,
    native_is_error: bool,
    result: SubagentResult,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Run {
    item_count: usize,
    items: Vec<Item>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    data_unavailable: Option<&'static str>,
}

impl Run {
    pub(super) fn new(item_count: usize) -> Self {
        Self {
            item_count, items: Vec::with_capacity(item_count),
            // A known empty array contains no unvalidated item. Consumers
            // still require this node's effective item schema at preflight.
            data: (item_count == 0).then(|| Value::Array(Vec::new())),
            data_unavailable: (item_count != 0).then_some("not_settled"),
        }
    }

    pub(super) fn settle(&mut self, result: SubagentResult, index: usize) -> Result<State> {
        if index != self.items.len() || index >= self.item_count {
            return Err(invalid("native fan-out returned an invalid item order; effects may have occurred"));
        }
        let state = native_state(&result);
        self.items.push(Item {
            index, state, hub_id: result.hub_id.clone(), native_is_error: result.is_error, result,
        });
        if self.items.len() != self.item_count { return Ok(State::Pending); }
        self.collect_data();
        Ok(if self.items.iter().all(|item| item.state == State::Accepted) {
            State::Accepted
        } else {
            State::Rejected
        })
    }

    /// Construct one bounded typed list only after every native result has
    /// settled. Keep individual outcomes on failure; never invent a partial
    /// successful list or infer values from result text. This snapshot lives
    /// only in the same workflow invocation, not a process-global cache.
    fn collect_data(&mut self) {
        if self.items.iter().any(|item| item.state != State::Accepted) {
            self.data_unavailable = Some("rejected_item");
            return;
        }
        let values: Option<Vec<&Value>> = self.items.iter().map(|item| {
            (item.result.schema_valid == Some(true)).then_some(item.result.data.as_ref()).flatten()
        }).collect();
        let Some(values) = values else {
            self.data_unavailable = Some("unvalidated_item");
            return;
        };
        if encoded_size(&values, MAX_DATA_BYTES).is_none() {
            self.data_unavailable = Some("result_limit");
            return;
        }
        self.data = Some(Value::Array(values.into_iter().cloned().collect()));
        self.data_unavailable = None;
    }

    pub(super) fn validated_data(&self) -> Option<&Value> { self.data.as_ref() }

    pub(super) fn render(&self, content: &mut String) {
        content.push_str(&format!("Fan-out: {} item(s); {} native results.\n", self.item_count, self.items.len()));
        content.push_str(&format!("Typed aggregate: {}.\n", self.data_unavailable.unwrap_or("available")));
        for item in &self.items {
            content.push_str(&format!("\n## Item {}; state: {}; hub: {}\n",
                item.index, item.state.label(),
                item.hub_id.as_ref().map_or_else(|| "none".into(), |id| json!(id).to_string())));
            content.push_str(&render_results(std::slice::from_ref(&item.result)));
        }
    }
}

pub(super) fn extend_schema(schema: &mut Value) {
    schema["properties"]["workflow"]["items"]["properties"]["forEach"] = json!({
        "type":"object", "additionalProperties":false, "required":["from","pointer","as","maxItems"],
        "description":"Run this node's unchanged native call once per item of an accepted direct needs predecessor's schema-validated JSON array. Bind each literal item under as in the existing workflow input block, alongside shared inputs; never choose agent/tools/cwd/isolation from data. maxItems is explicit (1-32); no truncation. The sum of declared maxima and ordinary calls must fit 128 task instances. All item prompts validate before any item launch, under the original 64 KiB task limit. All expanded tasks share the wave's global concurrency, captured baselines and ordered acceptance. Empty arrays complete with zero children. Any rejected item rejects the node; completed effects are retained, never rolled back or replayed. Results appear in node.fanout.items, in input order, with native outcomes and hub IDs.",
        "properties":{
            "from":{"type":"string","minLength":1,"maxLength":64},
            "pointer":{"type":"string","maxLength":1024},
            "as":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},
            "maxItems":{"type":"integer","minimum":1,"maximum":MAX_ITEMS}
        }
    });
    if let Some(description) = schema["properties"]["workflow"]["items"]["properties"]["forEach"]["description"]
        .as_str().map(str::to_owned)
    {
        schema["properties"]["workflow"]["items"]["properties"]["forEach"]["description"] = json!(format!(
            "{description} When all item results are schema-valid, fanout.data is their typed array in input order (256 KiB combined limit). Downstream inputs, when and forEach pointers address that array directly, not the per-item native envelopes. Source call/definition schemas validate each item, not the aggregate. Unavailable aggregate data never falls back to prose; individual native results remain inspectable."
        ));
    }
}

#[cfg(test)]
mod tests;
