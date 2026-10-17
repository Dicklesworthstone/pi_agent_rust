//! Validation of caller rubrics and terminal provider answers.

use super::{Result, failure};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};

const MAX_BYTES: usize = 128 * 1024;
const MAX_QUESTIONS: usize = 16;
const MAX_OPTIONS: usize = 16;
const MAX_TEXT: usize = 4096;

/// A complete, preflighted state and rubric. The private fields prevent callers
/// from bypassing validation before a provider request.
#[derive(Clone, Debug)]
pub struct JudgmentRequest {
    state: Value,
    questions: BTreeMap<String, Question>,
}

#[derive(Clone, Debug)]
struct Question {
    kind: Kind,
    raw: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Bool,
    Choice(Vec<String>),
    Score(usize),
}

fn invalid() -> crate::error::Error {
    failure("PI_JUDGE_INPUT: expected bounded state and 1-16 bool/choice/score questions with valid criteria")
}

fn identifier(text: &str) -> bool {
    !text.is_empty() && text.len() <= 64 && !text.chars().any(char::is_control)
}

fn rubric(value: &Value) -> bool {
    value.as_str().is_some_and(|text| text.len() <= MAX_TEXT)
}

impl JudgmentRequest {
    /// `state` is nonempty text, an object or an array. Questions use the OMP
    /// bool/choice/score rubric shapes. Images and unknown fields are refused.
    pub fn parse(input: Value) -> Result<Self> {
        bounded_json(&input, MAX_BYTES)?;
        let object = input.as_object().filter(|o| o.len() == 2
            && o.contains_key("state") && o.contains_key("questions")).ok_or_else(invalid)?;
        let state = &object["state"];
        if !(state.is_object() || state.is_array()
            || state.as_str().is_some_and(|text| !text.trim().is_empty())) {
            return Err(invalid());
        }
        let raw = object["questions"].as_object().filter(|questions|
            (1..=MAX_QUESTIONS).contains(&questions.len())).ok_or_else(invalid)?;
        let mut questions = BTreeMap::new();
        for (id, value) in raw {
            if !identifier(id) { return Err(invalid()); }
            let fields = value.as_object().ok_or_else(invalid)?;
            if fields.keys().any(|key| !matches!(key.as_str(), "type" | "instructions" | "criteria"))
                || !fields.get("instructions").is_some_and(|value|
                    rubric(value) && value.as_str().is_some_and(|text| !text.trim().is_empty())) {
                return Err(invalid());
            }
            let kind = match fields.get("type").and_then(Value::as_str) {
                Some("bool") => {
                    if let Some(criteria) = fields.get("criteria") {
                        let criteria = criteria.as_object().ok_or_else(invalid)?;
                        if criteria.iter().any(|(key, value)|
                            !matches!(key.as_str(), "true" | "false") || !rubric(value)) {
                            return Err(invalid());
                        }
                    }
                    Kind::Bool
                }
                Some("choice") => {
                    let criteria = fields.get("criteria").and_then(Value::as_object)
                        .filter(|c| (2..=MAX_OPTIONS).contains(&c.len())).ok_or_else(invalid)?;
                    if criteria.iter().any(|(label, value)|
                        !identifier(label) || !(value.is_null() || rubric(value))) {
                        return Err(invalid());
                    }
                    let mut labels: Vec<String> = criteria.keys().cloned().collect();
                    labels.sort();
                    Kind::Choice(labels)
                }
                Some("score") => {
                    let criteria = fields.get("criteria").and_then(Value::as_array)
                        .filter(|c| (2..=MAX_OPTIONS).contains(&c.len())).ok_or_else(invalid)?;
                    if !criteria.iter().all(rubric) { return Err(invalid()); }
                    Kind::Score(criteria.len())
                }
                _ => return Err(invalid()),
            };
            questions.insert(id.clone(), Question { kind, raw: value.clone() });
        }
        Ok(Self { state: state.clone(), questions })
    }

    pub(crate) fn parse_encoded(text: &str) -> Result<Self> {
        if text.len() > MAX_BYTES { return Err(invalid()); }
        let value = strict_json(text).map_err(|_| invalid())?;
        Self::parse(value)
    }

    pub(crate) fn encoded(&self) -> Result<String> {
        let questions: BTreeMap<_, _> = self.questions.iter()
            .map(|(id, question)| (id, &question.raw)).collect();
        bounded_json(&json!({"state": self.state, "questions": questions}), MAX_BYTES)
    }

    pub(crate) fn same_answer_keys(&self, other: &Self) -> bool {
        self.questions.len() == other.questions.len()
            && self.questions.iter().all(|(id, question)|
                other.questions.get(id).is_some_and(|other| other.kind == question.kind))
    }

    /// Refuse absent/extra questions, duplicate JSON keys, coercion, unknown
    /// labels and incomplete distributions. Derive answers deterministically.
    pub(crate) fn answers(&self, text: &str) -> Result<Value> {
        let bad = || failure("PI_JUDGE_RESPONSE: response does not completely satisfy the typed rubric; no verdict");
        if text.len() > super::MAX_ANSWER_BYTES { return Err(bad()); }
        let value = strict_json(text).map_err(|_| bad())?;
        let values = value.as_object().filter(|v| v.len() == self.questions.len()).ok_or_else(bad)?;
        let mut answers = Map::new();
        for (id, question) in &self.questions {
            let value = values.get(id).ok_or_else(bad)?;
            let answer = match &question.kind {
                Kind::Bool => json!({"type":"bool", "bool": probability(value).ok_or_else(bad)?}),
                Kind::Choice(labels) => {
                    let fields = value.as_object().filter(|v| v.len() == labels.len()).ok_or_else(bad)?;
                    let mut probabilities: Vec<f64> = labels.iter().map(|label|
                        fields.get(label).and_then(probability).ok_or_else(bad)).collect::<Result<_>>()?;
                    distribution(&mut probabilities).ok_or_else(bad)?;
                    // Stable lexical tie break; never trust a separate model-
                    // supplied winner that conflicts with the distribution.
                    let mut winner = 0;
                    for index in 1..probabilities.len() {
                        if probabilities[index] > probabilities[winner] { winner = index; }
                    }
                    let by_label: BTreeMap<_, _> = labels.iter().zip(&probabilities).collect();
                    json!({"type":"choice", "choice":labels[winner],
                        "probabilities":by_label, "confidence": concentration(&probabilities)})
                }
                Kind::Score(count) => {
                    let fields = value.as_array().filter(|v| v.len() == *count).ok_or_else(bad)?;
                    let mut probabilities: Vec<f64> = fields.iter().map(|v|
                        probability(v).ok_or_else(bad)).collect::<Result<_>>()?;
                    distribution(&mut probabilities).ok_or_else(bad)?;
                    let mut score = 0.0;
                    let mut by_index = Map::new();
                    for (index, probability) in probabilities.iter().enumerate() {
                        score += f64::from(u32::try_from(index).map_err(|_| bad())?) * probability;
                        by_index.insert(index.to_string(), json!(probability));
                    }
                    json!({"type":"score", "score":score,
                        "probabilities":by_index, "confidence":concentration(&probabilities)})
                }
            };
            answers.insert(id.clone(), answer);
        }
        Ok(Value::Object(answers))
    }
}

fn probability(value: &Value) -> Option<f64> {
    value.as_f64().filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
}

fn distribution(values: &mut [f64]) -> Option<()> {
    let sum = values.iter().sum::<f64>();
    if (sum - 1.0).abs() > 0.000_001 { return None; }
    // Normalize only accepted floating-point rounding, not arbitrary scores.
    // This keeps weighted scores within the rubric's level range.
    for value in values { *value /= sum; }
    Some(())
}

fn concentration(values: &[f64]) -> f64 {
    let entropy: f64 = values.iter().filter(|value| **value > 0.0).map(|value| -value * value.ln()).sum();
    let count = u32::try_from(values.len()).expect("validated distribution has at most 16 entries");
    (1.0 - entropy / f64::from(count).ln()).clamp(0.0, 1.0)
}

fn bounded_json(value: &Value, limit: usize) -> Result<String> {
    fn shape(value: &Value, depth: usize, left: &mut usize) -> Result<()> {
        if depth > 32 || *left == 0 { return Err(invalid()); }
        *left -= 1;
        match value {
            Value::Array(values) => for value in values { shape(value, depth + 1, left)?; },
            Value::Object(values) => for value in values.values() { shape(value, depth + 1, left)?; },
            _ => {}
        }
        Ok(())
    }
    shape(value, 0, &mut 65_536)?;
    struct Bounded { data: Vec<u8>, limit: usize }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.data.len()) {
                return Err(io::Error::other("judgment input limit"));
            }
            self.data.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut writer = Bounded { data: Vec::new(), limit };
    serde_json::to_writer(&mut writer, value).map_err(|_| invalid())?;
    String::from_utf8(writer.data).map_err(|_| invalid())
}

/// Serde's Value normally accepts duplicate object keys by overwriting them.
/// A verdict must not depend on which consumer happens to choose the last key.
struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Json;
        impl<'de> Visitor<'de> for Json {
            type Value = Strict;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON without duplicate keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Strict, E> { Ok(Strict(json!(value))) }
            fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Strict, E> { Ok(Strict(json!(value))) }
            fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Strict, E> { Ok(Strict(json!(value))) }
            fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Strict, E> {
                serde_json::Number::from_f64(value).map(Value::Number).map(Strict)
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Strict, E> { Ok(Strict(json!(value))) }
            fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<Strict, E> { Ok(Strict(Value::String(value))) }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Strict, E> { Ok(Strict(Value::Null)) }
            fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> std::result::Result<Strict, A::Error> {
                let mut result = Vec::new();
                while let Some(Strict(value)) = values.next_element()? { result.push(value); }
                Ok(Strict(Value::Array(result)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut values: A) -> std::result::Result<Strict, A::Error> {
                let mut result = Map::new();
                while let Some((key, Strict(value))) = values.next_entry::<String, Strict>()? {
                    if result.insert(key, value).is_some() { return Err(de::Error::custom("duplicate key")); }
                }
                Ok(Strict(Value::Object(result)))
            }
        }
        deserializer.deserialize_any(Json)
    }
}
fn strict_json(text: &str) -> serde_json::Result<Value> {
    serde_json::from_str::<Strict>(text).map(|Strict(value)| value)
}
