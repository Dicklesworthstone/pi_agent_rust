//! Explicit, version-bound triage in a new SARIF artifact. The original audit,
//! its findings and its coverage remain intact. No ambient suppression policy
//! is installed, and future audit queries never inherit a decision silently.

use super::{
    AgentCx, ContentBlock, MAX_REPORT_BYTES, Path, Result, Root, Snapshot, TextContent, ToolOutput,
    Value, checkpoint, error, json, snapshot, validate_name,
};
use serde::{Deserialize, Serialize};

pub(super) const SCHEMA: &str = "pi.security-dependency-disposition/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Decision {
    AcceptedRisk,
    FalsePositive,
    Reopen,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Record {
    schema: String,
    fingerprint: String,
    pub decision: Decision,
    reason: String,
    reviewed_at: String,
    source_sha256: String,
    advisory_modified: String,
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_reason(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 2048
        && !value
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
}

fn owned(suppression: &Value) -> bool {
    suppression
        .pointer("/properties/piDependencyReview")
        .and_then(Value::as_str)
        == Some(SCHEMA)
}

// Called during snapshot admission, including offline comparisons. Only our
// suppression marker is interpreted; unrelated reviewers' suppressions remain
// foreign metadata and are never silently removed by a reopen decision.
pub(super) fn validate_record(
    record: Option<&Record>,
    finding: &snapshot::Match,
    suppressions: &[Value],
    observed_at: &str,
) -> Result<()> {
    if suppressions.len() > 128 || suppressions.iter().any(|value| !value.is_object()) {
        return Err(error("invalid dependency suppression list"));
    }
    let own: Vec<_> = suppressions.iter().filter(|entry| owned(entry)).collect();
    let Some(record) = record else {
        return if own.is_empty() {
            Ok(())
        } else {
            Err(error(
                "dependency suppression has no matching review record",
            ))
        };
    };
    if record.reviewed_at.len() > 80 {
        return Err(error("invalid dependency review timestamp"));
    }
    let reviewed = chrono::DateTime::parse_from_rfc3339(&record.reviewed_at)
        .map_err(|_| error("invalid dependency review timestamp"))?;
    let observed = chrono::DateTime::parse_from_rfc3339(observed_at)
        .map_err(|_| error("invalid audit timestamp"))?;
    if record.schema != SCHEMA
        || record.fingerprint != finding.fingerprint
        || record.advisory_modified != finding.modified
        || !valid_reason(&record.reason)
        || !valid_hash(&record.source_sha256)
        || reviewed < observed
    {
        return Err(error(
            "dependency review is malformed or bound to another finding/revision",
        ));
    }
    if record.decision == Decision::Reopen {
        if !own.is_empty() {
            return Err(error("reopened dependency retains an owned suppression"));
        }
    } else if own.len() != 1
        || own[0]["kind"] != "external"
        || own[0]["status"] != "accepted"
        || own[0]["justification"].as_str() != Some(record.reason.as_str())
    {
        return Err(error(
            "dependency suppression disagrees with its review decision",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    #[serde(rename = "op")]
    _op: String,
    current: String,
    expected_sha256: String,
    fingerprint: String,
    decision: Decision,
    reason: String,
    sarif_out: String,
}

pub(in crate::security_scan) async fn execute(cwd: &Path, input: Value) -> Result<ToolOutput> {
    let input: Input = serde_json::from_value(input)
        .map_err(|_| error("invalid disposition_dependencies arguments"))?;
    validate_name(&input.current)?;
    validate_name(&input.sarif_out)?;
    if !valid_hash(&input.expected_sha256)
        || !valid_hash(&input.fingerprint)
        || !valid_reason(&input.reason)
        || input.current == input.sarif_out
    {
        return Err(error(
            "review requires exact SHA-256/fingerprint, a nonblank reason up to 2048 bytes, and a new output filename",
        ));
    }
    let owner = AgentCx::for_current_or_request();
    if !owner.capabilities().io {
        return Err(error("dependency review requires I/O capability"));
    }
    checkpoint(&owner)?;
    let workspace = cwd.to_path_buf();
    let worker = owner.clone();
    let output = asupersync::runtime::spawn_blocking(move || {
        checkpoint(&worker)?;
        let root = Root::open(&workspace)?;
        let bytes = root.read(&input.current)?;
        if snapshot::digest(&bytes) != input.expected_sha256 {
            return Err(error("dependency report changed since review; refresh its hash before deciding"));
        }
        let snapshot = Snapshot::parse(&bytes)?;
        checkpoint(&worker)?;
        let (document, foreign) = annotate(&bytes, &snapshot, &input)?;
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|_| error("cannot encode dependency review"))?;
        if bytes.len() > MAX_REPORT_BYTES {
            return Err(error("annotated dependency report exceeds 16 MiB"));
        }
        // Exercise the same admission as the next comparison/read before any
        // publication. A decision cannot repair or mask broken input coverage.
        Snapshot::parse(&bytes)?;
        checkpoint(&worker)?;
        root.publish(&input.sarif_out, &bytes, &worker)?;
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Dependency review saved to {}. All {} advisory matches and original query-completion flags are retained. The decision applies only to this exact finding/revision in this artifact, not future audits.",
                input.sarif_out, snapshot.findings.len()
            )))],
            details: Some(json!({"schema":SCHEMA,"sarif":input.sarif_out,"sha256":snapshot::digest(&bytes),
                "sourceSha256":snapshot.digest,"fingerprint":input.fingerprint,"decision":input.decision,
                "queryComplete":snapshot.query_complete,"advisoryMatches":snapshot.findings.len(),
                "otherSuppressionsRetained":foreign,"futureAuditsAffected":false})),
            is_error: !snapshot.query_complete,
        })
    }).await?;
    checkpoint(&owner)?;
    Ok(output)
}

fn annotate(bytes: &[u8], snapshot: &Snapshot, input: &Input) -> Result<(Value, usize)> {
    let finding = snapshot
        .findings
        .get(&input.fingerprint)
        .ok_or_else(|| error("fingerprint is absent from this dependency report"))?;
    let mut document: Value =
        serde_json::from_slice(bytes).map_err(|_| error("malformed dependency report"))?;
    let results = document
        .pointer_mut("/runs/0/results")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| error("missing dependency results"))?;
    // SARIF 2.1.0 section 3.27.23 requires suppression information to be
    // consistently null or non-null across a run. Once a decision is present,
    // explicitly retain empty arrays for the remaining unsuppressed results.
    // The typed snapshot already rejected any non-array/non-null payload.
    for result in results.iter_mut() {
        if result.get("suppressions").is_none_or(Value::is_null) {
            result["suppressions"] = json!([]);
        }
    }
    let result = results
        .iter_mut()
        .find(|entry| {
            entry
                .pointer("/fingerprints/pi~1dependency~1v1")
                .and_then(Value::as_str)
                == Some(input.fingerprint.as_str())
        })
        .ok_or_else(|| error("missing dependency finding"))?;
    let record = Record {
        schema: SCHEMA.into(),
        fingerprint: input.fingerprint.clone(),
        decision: input.decision,
        reason: input.reason.clone(),
        reviewed_at: chrono::Utc::now().to_rfc3339(),
        source_sha256: snapshot.digest.clone(),
        advisory_modified: finding.modified.clone(),
    };
    let mut suppressions: Vec<Value> = result
        .get("suppressions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| !owned(entry))
        .cloned()
        .collect();
    let foreign = suppressions.len();
    if input.decision != Decision::Reopen {
        suppressions.push(
            json!({"kind":"external","status":"accepted","justification":record.reason,
            "properties":{"piDependencyReview":SCHEMA}}),
        );
    }
    validate_record(Some(&record), finding, &suppressions, &snapshot.observed_at)?;
    result["suppressions"] = json!(suppressions);
    result["properties"]["piDependencyReview"] =
        serde_json::to_value(record).map_err(|_| error("cannot encode dependency decision"))?;
    Ok((document, foreign))
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
struct Stage<'a> {
    root: &'a rustix::fd::OwnedFd,
    name: String,
}
#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(self.root, self.name.as_str(), rustix::fs::AtFlags::empty());
    }
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
impl Root {
    fn publish(&self, name: &str, bytes: &[u8], owner: &AgentCx) -> Result<()> {
        use rustix::fs::{AtFlags, Mode, OFlags};
        use std::io::Write as _;
        validate_name(name)?;
        if bytes.len() > MAX_REPORT_BYTES || !owner.capabilities().io {
            return Err(error(
                "review publication exceeds its size or I/O capability",
            ));
        }
        checkpoint(owner)?;
        let name_stage = format!(".pi-dependency-review-{}.tmp", uuid::Uuid::new_v4());
        let fd = rustix::fs::openat(
            &self.0,
            name_stage.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|_| error("cannot create private dependency review staging file"))?;
        let _stage = Stage {
            root: &self.0,
            name: name_stage.clone(),
        };
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| error("dependency review write/sync failed; destination unchanged"))?;
        checkpoint(owner)?;
        rustix::fs::linkat(
            &self.0,
            name_stage.as_str(),
            &self.0,
            name,
            AtFlags::empty(),
        )
        .map_err(|_| {
            error("dependency review output exists or cannot be published; no overwrite performed")
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
impl Root {
    #[allow(clippy::unused_self)]
    fn publish(&self, _name: &str, _bytes: &[u8], _owner: &AgentCx) -> Result<()> {
        Err(error(
            "confined review publication requires supported Unix descriptor APIs",
        ))
    }
}
