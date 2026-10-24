//! Bounded provisional rich results, shared by Python and QuickJS.
//!
//! The existing final MIME formatter also admits each live frame. No preview
//! parser, PNG bypass, or caller hook runs before that admission. The final
//! collection must match all streamed snapshots, not silently rewrite them.

use crate::error::{Error, Result};
use crate::model::ContentBlock;
use serde_json::{Value, json};

use super::super::display::{MAX_DISPLAYS, MAX_TOTAL_BYTES, python_response};

// Replacement updates may be frequent during noisy computations. Bound their
// MIME payload independently of the complete final collection. Never cut a
// JSON value or PNG in half; omitted items remain available at completion.
const PREVIEW_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(super) struct LiveDisplays {
    frames: Vec<Value>,
    content: Vec<ContentBlock>,
    descriptors: Vec<Value>,
    json_outputs: Vec<Value>,
    bytes: usize,
    preview_bytes: usize,
    omitted: usize,
}

impl LiveDisplays {
    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.frames.len()
    }

    pub(super) fn accept(&mut self, stream: &Value) -> Result<()> {
        let expected = u64::try_from(self.frames.len() + 1)
            .map_err(|_| super::invalid("display sequence exhausted"))?;
        if stream.get("sequence").and_then(Value::as_u64) != Some(expected) {
            return Err(super::invalid("out-of-order or duplicated rich display"));
        }
        if self.frames.len() >= MAX_DISPLAYS {
            return Err(Error::tool(
                "eval",
                "EVAL_DISPLAY_LIMIT: at most 32 live displays per cell",
            ));
        }
        let frame = stream
            .get("display")
            .ok_or_else(|| super::invalid("rich stream must contain a display"))?;
        // This is the same native formatter used at cell completion, with one
        // frame and no stdout/result/startup banner. Per-item MIME, PNG,
        // structure and encoded-byte checks cannot drift between both paths.
        let single = json!({"ok": true, "displays": [frame]});
        let validated = python_response(&serde_json::to_string(&single)?, false, 0)?;
        let details = validated
            .details
            .as_ref()
            .ok_or_else(|| super::invalid("missing validated display metadata"))?;
        let descriptor = details
            .get("displays")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .ok_or_else(|| super::invalid("missing validated display descriptor"))?;
        let item_bytes = descriptor
            .get("bytes")
            .and_then(Value::as_u64)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| super::invalid("missing validated display byte count"))?;
        let bytes = self
            .bytes
            .checked_add(item_bytes)
            .filter(|bytes| *bytes <= MAX_TOTAL_BYTES)
            .ok_or_else(|| Error::tool("eval", "EVAL_DISPLAY_LIMIT: live displays exceed 4 MiB"))?;
        let json_outputs = details
            .get("jsonOutputs")
            .and_then(Value::as_array)
            .ok_or_else(|| super::invalid("missing validated JSON outputs"))?;
        // All refusal paths precede mutation. One admission does not reparse
        // the previous collection; storage is bounded by the same 32/4 MiB
        // budget as final displays, independently from console capture.
        // JSON appears both as text and as structured data. Charge both
        // copies conservatively; descriptors and omission notes are bounded
        // separately by the existing display count limit.
        let cost = item_bytes.saturating_mul(if json_outputs.is_empty() { 1 } else { 2 });
        let mut descriptor = descriptor.clone();
        if cost <= PREVIEW_BYTES.saturating_sub(self.preview_bytes) {
            self.content.extend(validated.content);
            self.json_outputs.extend(json_outputs.iter().cloned());
            self.preview_bytes += cost;
        } else {
            self.omitted += 1;
            descriptor["previewOmitted"] = Value::Bool(true);
            self.content.push(ContentBlock::Text(crate::model::TextContent::new(format!(
                "[EVAL_DISPLAY_PREVIEW: display {expected} omitted from this bounded live preview; full value is available only if the cell completes]"
            ))));
        }
        self.frames.push(frame.clone());
        self.descriptors.push(descriptor);
        self.bytes = bytes;
        Ok(())
    }

    pub(super) fn verify(&self, final_frames: Option<&Vec<Value>>) -> Result<()> {
        // Keep final-only compatibility for native kernel tests and hosts
        // without a live producer. Once streaming starts it is all-or-nothing:
        // missing, reordered, appended or rewritten final frames are invalid.
        if !self.frames.is_empty() && final_frames != Some(&self.frames) {
            return Err(super::invalid(
                "final rich displays differ from streamed snapshots",
            ));
        }
        Ok(())
    }

    pub(super) fn annotate(&self, details: &mut Value, complete: bool) {
        if self.is_empty() || !details.is_object() {
            return;
        }
        details["displayStream"] = json!({
            "schema": "pi.eval.display_stream.v1",
            "sequence": self.frames.len(),
            "bytes": self.bytes,
            "complete": complete,
            "preview": !complete,
        });
        if !complete {
            details["displayStream"]["previewBytes"] = json!(self.preview_bytes);
            details["displayStream"]["previewLimit"] = json!(PREVIEW_BYTES);
            details["displayStream"]["omittedDisplays"] = json!(self.omitted);
        }
    }

    pub(super) fn append(
        &self,
        content: &mut Vec<ContentBlock>,
        details: &mut Value,
        complete: bool,
    ) {
        if self.is_empty() {
            return;
        }
        content.extend(self.content.iter().cloned());
        details["displays"] = Value::Array(self.descriptors.clone());
        details["jsonOutputs"] = Value::Array(self.json_outputs.clone());
        self.annotate(details, complete);
    }
}

#[cfg(test)]
mod tests;
