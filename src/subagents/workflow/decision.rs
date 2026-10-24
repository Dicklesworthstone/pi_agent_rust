//! Typed branch selection over settled native results. This does not approve
//! tools, interpret guest code, or turn invalid/missing evidence into a verdict.

#[cfg(test)]
use super::SubagentStatus;
use super::{
    BTreeMap, Record, Result, State, Value, encoded_size, identifier, invalid, validate_pointer,
};
use serde_json::json;

const MAX_CONDITION_BYTES: usize = 16 * 1024;

pub(super) fn is_field(key: &str) -> bool {
    matches!(key, "when" | "join")
}

pub(super) fn reject_misplaced(input: &Value) -> Result<()> {
    let has_policy = |value: &Value| {
        value
            .as_object()
            .is_some_and(|fields| fields.keys().any(|key| is_field(key)))
    };
    if has_policy(input)
        || ["tasks", "chain"].iter().any(|name| {
            input
                .get(*name)
                .and_then(Value::as_array)
                .is_some_and(|entries| entries.iter().any(has_policy))
        })
    {
        return Err(invalid(
            "decision policy belongs on workflow nodes, not ordinary delegations",
        ));
    }
    Ok(())
}

pub(super) struct Policy {
    when: Option<Condition>,
    join: Join,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Join {
    All,
    Selected,
}

struct Condition {
    from: usize,
    pointer: String,
    equals: Value,
}

pub(super) struct Admission {
    pub(super) reason: Option<&'static str>,
    pub(super) sources: Vec<usize>,
    pub(super) not_selected: bool,
    pub(super) condition: Option<&'static str>,
}

impl Admission {
    fn ready(condition: Option<&'static str>) -> Self {
        Self {
            reason: None,
            sources: Vec::new(),
            not_selected: false,
            condition,
        }
    }

    fn stopped(reason: &'static str, sources: Vec<usize>, not_selected: bool) -> Self {
        Self {
            reason: Some(reason),
            sources,
            not_selected,
            condition: None,
        }
    }
}

impl Policy {
    pub(super) fn parse(
        entry: &Value,
        ids: &BTreeMap<String, usize>,
        needs: &[usize],
    ) -> Result<Self> {
        let join = match entry.get("join") {
            None => Join::All,
            Some(value) => {
                if needs.is_empty() {
                    return Err(invalid("join requires at least one explicit dependency"));
                }
                match value.as_str() {
                    Some("all") => Join::All,
                    Some("selected") => Join::Selected,
                    _ => {
                        return Err(invalid(
                            "join must be all or selected; failed dependencies are never optional",
                        ));
                    }
                }
            }
        };
        let when = entry
            .get("when")
            .map(|value| {
                if encoded_size(value, MAX_CONDITION_BYTES).is_none() {
                    return Err(invalid("when exceeds its 16 KiB encoded limit"));
                }
                let fields = value
                    .as_object()
                    .filter(|fields| {
                        fields.len() == 3
                            && fields.contains_key("from")
                            && fields.contains_key("pointer")
                            && fields.contains_key("equals")
                    })
                    .ok_or_else(|| {
                        invalid("when requires exactly from, pointer and a literal equals value")
                    })?;
                let from = ids
                    .get(identifier(&fields["from"])?)
                    .copied()
                    .filter(|index| needs.contains(index))
                    .ok_or_else(|| {
                        invalid("when must select an explicitly named direct dependency")
                    })?;
                let pointer = fields["pointer"]
                    .as_str()
                    .ok_or_else(|| invalid("when pointer must be a JSON pointer string"))?;
                validate_pointer(pointer)?;
                Ok(Condition {
                    from,
                    pointer: pointer.to_owned(),
                    equals: fields["equals"].clone(),
                })
            })
            .transpose()?;
        Ok(Self { when, join })
    }

    /// The caller checks this source's effective schema during full-graph
    /// preflight, even when this branch will eventually not be selected.
    pub(super) fn source(&self) -> Option<usize> {
        self.when.as_ref().map(|condition| condition.from)
    }

    pub(super) fn evaluate(&self, needs: &[usize], records: &[Record]) -> Admission {
        // Failure wins over intentional exclusion. A mixed join cannot hide a
        // killed/schema-invalid predecessor behind another unselected branch.
        let failed: Vec<_> = needs
            .iter()
            .copied()
            .filter(|&index| {
                records.get(index).is_none_or(|record| {
                    !matches!(record.state, State::Accepted | State::NotSelected)
                })
            })
            .collect();
        if !failed.is_empty() {
            return Admission::stopped("dependency_failed", failed, false);
        }
        let excluded: Vec<_> = needs
            .iter()
            .copied()
            .filter(|&index| records[index].state == State::NotSelected)
            .collect();
        if !excluded.is_empty() && (self.join == Join::All || excluded.len() == needs.len()) {
            let reason = if self.join == Join::Selected {
                "no_selected_dependencies"
            } else {
                "dependency_not_selected"
            };
            return Admission::stopped(reason, excluded, true);
        }
        // Selected joins wait for the ordinary complete-wave barrier and need
        // at least one accepted predecessor. Only intentional nonselection is
        // optional; this is not an any-success race or failure-recovery path.
        let Some(condition) = &self.when else {
            return Admission::ready(None);
        };
        let data = records.get(condition.from).and_then(Record::validated_data);
        let failure = |reason| {
            let mut admission = Admission::stopped(reason, vec![condition.from], false);
            admission.condition = Some(reason);
            admission
        };
        let Some(data) = data else {
            return failure("condition_data_unavailable");
        };
        let Some(actual) = data.pointer(&condition.pointer) else {
            return failure("condition_value_missing");
        };
        if actual == &condition.equals {
            Admission::ready(Some("matched"))
        } else {
            let mut admission = Admission::stopped("condition_false", vec![condition.from], true);
            admission.condition = Some("not_matched");
            admission
        }
    }
}

pub(super) fn extend_schema(schema: &mut Value) {
    schema["properties"]["workflow"]["items"]["properties"]["when"] = json!({
        "type":"object", "additionalProperties":false, "required":["from","pointer","equals"],
        "description":"Run this node only when a direct needs predecessor's accepted schema-validated data matches a literal equals value at pointer. Source requires outputSchema or an agent-definition schema. JSON types and null are preserved; missing data is an error, not a false condition. A mismatch marks this branch not_selected without launching it or failing the workflow. All-required dependants are also not selected; explicit selected joins can converge branches. Dependency failures still block it. Conditions are checked before input expansion or workspace capture, under the original deadline. At most 16 KiB encoded; no script, coercion, polling, retries, or tool-approval authority. Compared values are not echoed in decision diagnostics.",
        "properties":{
            "from":{"type":"string","minLength":1,"maxLength":64},
            "pointer":{"type":"string","maxLength":1024},
            "equals":{}
        }
    });
    schema["properties"]["workflow"]["items"]["properties"]["join"] = json!({
        "type":"string", "enum":["all","selected"], "default":"all",
        "description":"Dependency admission, requiring nonempty needs when supplied. all requires every predecessor accepted. selected waits for all predecessors to settle, requires at least one accepted, and permits only intentional not_selected alternatives. Any rejected, error-skipped or unfinished predecessor still blocks the node; all-unselected prerequisites leave this node not_selected too. This is not any-success, early dispatch or recovery. Existing typed inputs and when still require their exact accepted source data; no absent-branch fallback. Native workspace acceptance, deadline and tool permissions remain unchanged."
    });
}

#[cfg(test)]
mod tests;
