//! Whole-workspace pull diagnostics, including documents Pi has never opened.
//!
//! Each invocation requests a full report. No previous result IDs or partial
//! result tokens are sent: an `unchanged` report has no usable baseline, and
//! a failed request must never be filled in from the document diagnostic cache.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use super::super::{LspClient, OpenDoc, file_uri};
use super::RequestBudget;
use crate::error::{Error, Result};

const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_DOCUMENTS: usize = 2048;
const MAX_DIAGNOSTICS: usize = 16_384;
const MAX_ID_BYTES: usize = 4096;

fn report_error(message: &str) -> Error {
    Error::tool(
        "lsp",
        format!("[LSP_WORKSPACE_DIAGNOSTIC_REPORT] {message}"),
    )
}

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("workspace diagnostic byte limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn normalize_report_uri(raw: &str) -> Result<String> {
    if raw.is_empty() || raw.len() > MAX_ID_BYTES || raw.chars().any(char::is_control) {
        return Err(report_error("report URI must be a bounded absolute URI"));
    }
    let parsed =
        url::Url::parse(raw).map_err(|_| report_error("report URI must be an absolute URI"))?;
    if parsed.scheme() == "file" {
        return file_uri::normalize_uri(raw)
            .ok_or_else(|| report_error("invalid local file URI in diagnostic report"));
    }
    // Virtual documents and dependency URIs are metadata, not paths to open.
    // No filesystem access or network request is made for a returned URI.
    Ok(parsed.to_string())
}

fn validate_reports(raw: Value, open_docs: &HashMap<String, OpenDoc>) -> Result<Vec<Value>> {
    serde_json::to_writer(&mut ByteBudget(MAX_REPORT_BYTES), &raw)
        .map_err(|_| report_error("workspace report exceeds 8 MiB"))?;
    let Value::Object(mut object) = raw else {
        return Err(report_error("workspace result must contain an items array"));
    };
    let Some(Value::Array(mut reports)) = object.remove("items") else {
        return Err(report_error("workspace result must contain an items array"));
    };
    if reports.len() > MAX_DOCUMENTS {
        return Err(report_error("workspace report has too many documents"));
    }
    let mut seen = HashSet::with_capacity(reports.len());
    let mut diagnostic_count = 0_usize;
    for report in &mut reports {
        if report.get("kind").and_then(Value::as_str) != Some("full") {
            return Err(report_error(
                "a full request cannot accept an unchanged or unknown report",
            ));
        }
        let uri = report
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| report_error("document report is missing its URI"))?;
        let uri = normalize_report_uri(uri)?;
        if !seen.insert(uri.clone()) {
            return Err(report_error(
                "workspace report contains duplicate document URIs",
            ));
        }
        let version = match report.get("version") {
            Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .filter(|value| i32::try_from(*value).is_ok())
                    .ok_or_else(|| report_error("document version must be an integer or null"))?,
            ),
            None => return Err(report_error("document report is missing its version")),
        };
        if let Some(doc) = open_docs.get(&uri)
            && doc.opened
            && version != i64::try_from(doc.version).ok()
        {
            return Err(report_error(
                "report version does not match the synchronized document",
            ));
        }
        if let Some(id) = report.get("resultId")
            && !id.as_str().is_some_and(|id| id.len() <= MAX_ID_BYTES)
        {
            return Err(report_error("resultId must be a bounded string"));
        }
        let items = report
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| report_error("full document report must contain diagnostic items"))?;
        diagnostic_count = diagnostic_count.saturating_add(items.len());
        if diagnostic_count > MAX_DIAGNOSTICS {
            return Err(report_error("workspace report has too many diagnostics"));
        }
        for diagnostic in items {
            let range = diagnostic
                .get("range")
                .ok_or_else(|| report_error("diagnostic is missing its range"))?;
            let range: crate::lsp::text::Range = serde_json::from_value(range.clone())
                .map_err(|_| report_error("diagnostic has an invalid range"))?;
            if range.end < range.start
                || diagnostic.get("message").and_then(Value::as_str).is_none()
            {
                return Err(report_error(
                    "diagnostic needs an ordered range and string message",
                ));
            }
        }
        report["uri"] = Value::String(uri);
    }
    reports.sort_by(|left, right| left["uri"].as_str().cmp(&right["uri"].as_str()));
    Ok(reports)
}

impl LspClient {
    /// Pull a full diagnostic report from a workspace-capable language server.
    ///
    /// Unlike a file walk, the server can include unopened source files and
    /// virtual documents. Returned URIs are metadata and are never opened here.
    /// Reports are validated and sorted by URI. They do not modify the cached
    /// push/document diagnostics or reuse result IDs from another request.
    ///
    /// The timeout covers lane admission, retries, response validation and
    /// delivery. Any local document synchronization during the call invalidates
    /// the result. A successful server report is not an atomic disk snapshot,
    /// and an omitted document must not be inferred to have been checked.
    ///
    /// # Errors
    /// Refuses unsupported servers, malformed/oversized/incremental responses,
    /// version mismatches, local document changes, cancellation and timeout.
    pub async fn workspace_diagnostics(&self, timeout: Duration) -> Result<Vec<Value>> {
        let budget = RequestBudget::new(timeout);
        budget.remaining().map_err(Error::from)?;
        let mut params = json!({"previousResultIds": []});
        {
            let capabilities = Self::lock(&self.capabilities);
            let options = capabilities
                .raw
                .get("diagnosticProvider")
                .filter(|options| options.get("workspaceDiagnostics") == Some(&Value::Bool(true)))
                .ok_or_else(|| {
                    Error::tool(
                        "lsp",
                        "[LSP_UNSUPPORTED] server does not support workspace diagnostics",
                    )
                })?;
            if let Some(identifier) = options.get("identifier") {
                let identifier = identifier
                    .as_str()
                    .filter(|identifier| identifier.len() <= MAX_ID_BYTES)
                    .ok_or_else(|| {
                        report_error("diagnostic identifier must be a bounded string")
                    })?;
                params["identifier"] = json!(identifier);
            }
        }
        let (before, epoch) = {
            let docs = Self::lock(&self.open_docs);
            (
                docs.clone(),
                self.next_document_version.load(Ordering::SeqCst),
            )
        };
        let raw = self
            .call_with_budget("workspace/diagnostic", params, &budget)
            .await
            .map_err(Error::from)?;
        let reports = validate_reports(raw, &before)?;
        // Hold the same lock as document synchronization through final
        // validation. No await or callback can interleave with this check.
        let docs = Self::lock(&self.open_docs);
        let unchanged = self.next_document_version.load(Ordering::SeqCst) == epoch
            && docs.len() == before.len()
            && before.iter().all(|(uri, prior)| {
                docs.get(uri).is_some_and(|current| {
                    current.version == prior.version
                        && current.opened == prior.opened
                        && Arc::ptr_eq(&current.text, &prior.text)
                })
            });
        if !unchanged {
            return Err(report_error(
                "documents changed or closed during workspace diagnostics",
            ));
        }
        if !self.is_alive() {
            return Err(Error::tool(
                "lsp",
                "[LSP_TRANSPORT_CLOSED] workspace diagnostic connection closed",
            ));
        }
        budget.remaining().map_err(Error::from)?;
        drop(docs);
        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::client::test_server::Fixture;

    fn capabilities() -> Value {
        json!({"textDocumentSync": 1, "diagnosticProvider": {
            "identifier": "workspace-fixture", "interFileDependencies": true,
            "workspaceDiagnostics": true
        }})
    }

    fn report(uri: &str, message: &str) -> Value {
        json!({"uri": uri, "version": null, "kind": "full", "items": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "message": message, "severity": 1
        }]})
    }

    #[test]
    fn pulls_unopened_files_without_manufacturing_document_notifications() {
        let temp = tempfile::tempdir().unwrap();
        let Some(peer) = Fixture::connect(temp.path(), capabilities()) else {
            return;
        };
        let path = temp.path().canonicalize().unwrap().join("unopened.rs");
        let uri = crate::lsp::client::try_path_to_uri(&path).unwrap();
        let first = report(&uri, "first");
        let second = report("untitled:virtual-source", "virtual");
        peer.configure(json!({"workspace/diagnostic": [
            {"result": {"items": [second, first]}}
        ]}));
        let reports = peer
            .runtime
            .block_on(peer.client.workspace_diagnostics(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(reports, vec![first, second]);
        assert!(!path.exists(), "returned paths must never be materialized");
        assert!(peer.client.diagnostics_snapshot().is_empty());
        let frames = peer.frames();
        let calls: Vec<_> = frames
            .iter()
            .filter(|frame| frame["method"] == "workspace/diagnostic")
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]["params"],
            json!({
                "identifier": "workspace-fixture", "previousResultIds": []
            })
        );
        assert!(!frames.iter().any(|frame| matches!(
            frame["method"].as_str(),
            Some("textDocument/didOpen" | "textDocument/didChange" | "textDocument/diagnostic")
        )));
    }

    #[test]
    fn malformed_incomplete_and_duplicate_reports_are_not_clean_results() {
        let valid = report("untitled:source", "broken");
        for raw in [
            Value::Null,
            json!({}),
            json!({"items": null}),
            json!({"items": [{"uri":"untitled:source","version":null,"kind":"unchanged","resultId":"old"}]}),
            json!({"items": [{"uri":"relative.rs","version":null,"kind":"full","items":[]}]}),
            json!({"items": [{"uri":"untitled:source","kind":"full","items":[]}]}),
            json!({"items": [{"uri":"untitled:source","version":1.5,"kind":"full","items":[]}]}),
            json!({"items": [{"uri":"untitled:source","version":null,"kind":"full","items":null}]}),
            json!({"items": [{"uri":"untitled:source","version":null,"kind":"full","items":[{"message":"no range"}]}]}),
            json!({"items": [valid.clone(), valid]}),
        ] {
            assert!(validate_reports(raw, &HashMap::new()).is_err());
        }
        assert!(
            validate_reports(json!({"items": []}), &HashMap::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn workspace_reports_enforce_document_diagnostic_and_byte_bounds() {
        let empty = json!({"uri":"untitled:source","version":null,"kind":"full","items":[]});
        let error = validate_reports(
            json!({"items":vec![empty; MAX_DOCUMENTS + 1]}),
            &HashMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("too many documents"));
        let mut crowded = report("untitled:source", "x");
        crowded["items"] = json!(vec![crowded["items"][0].clone(); MAX_DIAGNOSTICS + 1]);
        let error = validate_reports(json!({"items":[crowded]}), &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("too many diagnostics"));
        let large = report("untitled:source", &"x".repeat(MAX_REPORT_BYTES));
        let error = validate_reports(json!({"items":[large]}), &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("8 MiB"));
    }

    #[test]
    fn unsupported_and_zero_budget_requests_never_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let Some(peer) = Fixture::connect(temp.path(), json!({"textDocumentSync":1})) else {
            return;
        };
        let error = peer
            .runtime
            .block_on(peer.client.workspace_diagnostics(Duration::from_secs(5)))
            .unwrap_err();
        assert!(error.to_string().contains("LSP_UNSUPPORTED"));
        let error = peer
            .runtime
            .block_on(peer.client.workspace_diagnostics(Duration::ZERO))
            .unwrap_err();
        assert!(error.to_string().contains("LSP_TIMEOUT"));
        assert!(
            !peer
                .frames()
                .iter()
                .any(|frame| frame["method"] == "workspace/diagnostic")
        );
    }

    #[test]
    fn changes_during_a_pull_and_wrong_document_versions_are_rejected() {
        for close in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let Some(peer) = Fixture::connect(temp.path(), capabilities()) else {
                return;
            };
            let path = temp.path().join("source.rs");
            std::fs::write(&path, "before").unwrap();
            let uri = peer.client.ensure_synced(&path, "rust").unwrap();
            let mut wrong = report(&uri, "wrong version");
            wrong["version"] = json!(i32::MAX);
            peer.configure(json!({"workspace/diagnostic":[
                {"result":{"items":[wrong]}}, {"hold":true}
            ]}));
            let error = peer
                .runtime
                .block_on(peer.client.workspace_diagnostics(Duration::from_secs(5)))
                .unwrap_err();
            assert!(error.to_string().contains("version"));
            peer.runtime.block_on(async {
                let mut pending =
                    Box::pin(peer.client.workspace_diagnostics(Duration::from_secs(5)));
                assert!(futures::poll!(pending.as_mut()).is_pending());
                if close {
                    peer.client.invalidate(&uri);
                } else {
                    std::fs::write(&path, "after").unwrap();
                    peer.client.ensure_synced(&path, "rust").unwrap();
                }
                peer.client
                    .call_no_wait_notify("test/release", json!({"result":{"items":[]}}))
                    .unwrap();
                assert!(
                    pending
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("changed or closed")
                );
            });
        }
    }

    #[test]
    fn workspace_cancellation_obeys_retrigger_and_never_uses_cached_success() {
        for retrigger in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let Some(peer) = Fixture::connect(temp.path(), capabilities()) else {
                return;
            };
            peer.configure(json!({"workspace/diagnostic":[
                {"error":{"code":-32802,"message":"fixture cancellation","data":{"retriggerRequest":retrigger}}},
                {"result":{"items":[]}}
            ]}));
            let result = peer
                .runtime
                .block_on(peer.client.workspace_diagnostics(Duration::from_secs(5)));
            if retrigger {
                assert!(result.unwrap().is_empty());
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("fixture cancellation")
                );
            }
            assert_eq!(
                peer.frames()
                    .iter()
                    .filter(|frame| frame["method"] == "workspace/diagnostic")
                    .count(),
                if retrigger { 2 } else { 1 }
            );
        }
    }
}
