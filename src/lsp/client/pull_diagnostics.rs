//! Negotiated textDocument/diagnostic support for pull-only language servers.
//!
//! Reports stay tied to the synchronized document incarnation and the newest
//! local pull. Result IDs are opaque; an unchanged report is meaningful only
//! with the exact prior report sent on that request. Related-document reports
//! are not advertised or imported. The retained report working set is bounded.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use super::request::RequestBudget;
use super::{LspClient, OpenDoc, WAIT_TICK, WARMUP_EMPTY_RESULT_WINDOW, file_uri};
use crate::error::{Error, Result};

const MAX_REPORT_BYTES: usize = 1024 * 1024;
const MAX_REPORT_ITEMS: usize = 4096;
const MAX_RESULT_ID_BYTES: usize = 4096;
const MAX_REPORTS: usize = 128;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Revision {
    version: u64,
    hash: u64,
}

impl From<&OpenDoc> for Revision {
    fn from(doc: &OpenDoc) -> Self {
        Self {
            version: doc.version,
            hash: doc.disk_hash,
        }
    }
}

#[derive(Clone)]
struct Report {
    revision: Revision,
    result_id: Option<String>,
    items: Arc<Vec<Value>>,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct ReportCache {
    reports: HashMap<String, Report>,
    order: VecDeque<String>,
    latest: HashMap<String, u64>,
    serial: u64,
    bytes: usize,
}

impl ReportCache {
    fn start(&mut self, uri: &str, docs: &HashMap<String, OpenDoc>) -> Result<u64> {
        // Failed/cancelled pulls must not leave an unbounded pending-key map
        // after documents are evicted or closed. The open set is itself bounded.
        self.latest.retain(|uri, _| docs.contains_key(uri));
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or_else(|| protocol_error("diagnostic request ID space exhausted"))?;
        self.latest.insert(uri.to_string(), self.serial);
        Ok(self.serial)
    }

    fn previous(&self, uri: &str, revision: Revision) -> Option<Report> {
        // Without a server-visible version, closing/reopening identical text
        // cannot distinguish incarnations by (version, hash). Request a full
        // report instead of reusing a possibly retired server result ID.
        self.reports
            .get(uri)
            .filter(|report| {
                revision.version != 0 && report.revision == revision && report.result_id.is_some()
            })
            .cloned()
    }

    fn insert(&mut self, uri: String, report: Report) -> Vec<String> {
        if let Some(old) = self.reports.remove(&uri) {
            self.bytes -= old.bytes;
        }
        self.order.retain(|key| key != &uri);
        let mut evicted = Vec::new();
        while self.reports.len() >= MAX_REPORTS
            || self.bytes.saturating_add(report.bytes) > MAX_CACHE_BYTES
        {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.reports.remove(&key) {
                self.bytes -= old.bytes;
                evicted.push(key);
            }
        }
        self.bytes += report.bytes;
        self.order.push_back(uri.clone());
        self.reports.insert(uri, report);
        evicted
    }
}

fn protocol_error(message: &str) -> Error {
    Error::tool("lsp", format!("[LSP_DIAGNOSTIC_REPORT] {message}"))
}

struct ByteBudget {
    remaining: usize,
}

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("diagnostic report exceeds its byte limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_size(value: &impl serde::Serialize, limit: usize) -> Result<usize> {
    let mut budget = ByteBudget { remaining: limit };
    serde_json::to_writer(&mut budget, value)
        .map_err(|_| protocol_error("diagnostic report exceeds its byte limit"))?;
    Ok(limit - budget.remaining)
}

fn result_id(raw: &Value) -> Result<Option<String>> {
    match raw.get("resultId") {
        None => Ok(None),
        Some(Value::String(id)) if id.len() <= MAX_RESULT_ID_BYTES => Ok(Some(id.clone())),
        Some(_) => Err(protocol_error("resultId must be a bounded string")),
    }
}

fn parse_report(raw: &Value, revision: Revision, previous: Option<&Report>) -> Result<Report> {
    encoded_size(raw, MAX_REPORT_BYTES)?;
    let result_id = result_id(raw)?;
    let items = match raw.get("kind").and_then(Value::as_str) {
        Some("full") => {
            let items = raw
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| protocol_error("full report must contain an items array"))?;
            if items.len() > MAX_REPORT_ITEMS {
                return Err(protocol_error("diagnostic report has too many items"));
            }
            for diagnostic in items {
                let range = diagnostic
                    .get("range")
                    .ok_or_else(|| protocol_error("diagnostic missing range"))?;
                let range: crate::lsp::text::Range = serde_json::from_value(range.clone())
                    .map_err(|_| protocol_error("invalid diagnostic range"))?;
                if range.end < range.start
                    || diagnostic.get("message").and_then(Value::as_str).is_none()
                {
                    return Err(protocol_error(
                        "diagnostic needs an ordered range and string message",
                    ));
                }
            }
            Arc::new(items.clone())
        }
        Some("unchanged") => {
            if result_id.is_none() {
                return Err(protocol_error("unchanged report must contain a resultId"));
            }
            let previous = previous
                .filter(|report| report.revision == revision && report.result_id.is_some())
                .ok_or_else(|| {
                    protocol_error("unchanged report has no matching previous result")
                })?;
            if raw.get("items").is_some() {
                return Err(protocol_error("unchanged report must not replace items"));
            }
            Arc::clone(&previous.items)
        }
        _ => return Err(protocol_error("unknown document diagnostic report kind")),
    };
    let bytes = encoded_size(items.as_ref(), MAX_REPORT_BYTES)?
        .saturating_add(result_id.as_ref().map_or(0, String::len));
    Ok(Report {
        revision,
        result_id,
        items,
        bytes,
    })
}

#[allow(clippy::significant_drop_tightening)]
impl LspClient {
    pub(super) fn has_pull_diagnostics(&self) -> bool {
        Self::lock(&self.capabilities)
            .raw
            .get("diagnosticProvider")
            .is_some_and(Value::is_object)
    }

    /// Obtain a diagnostic report for the currently synchronized document.
    ///
    /// Unlike the boolean waiting API, this method preserves pull failures and
    /// distinguishes an explicit empty report from the absence of a report.
    /// A zero wait inspects the cache without sending a pull request. Returned
    /// data is checked against the local synchronized revision; unversioned
    /// server pushes still cannot provide a server-side freshness proof.
    ///
    /// # Errors
    /// Returns `LSP_DIAGNOSTICS_PENDING` when no report arrived, and propagates
    /// protocol, cancellation, transport and resynchronization failures.
    pub async fn document_diagnostics(&self, uri: &str, wait: Duration) -> Result<Vec<Value>> {
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        owner
            .checkpoint()
            .map_err(|_| Error::from(super::LspCallError::Cancelled))?;
        let uri =
            file_uri::normalize_uri(uri).ok_or_else(|| protocol_error("invalid document URI"))?;
        let (revision, source) = {
            let docs = Self::lock(&self.open_docs);
            let doc = docs.get(&uri).ok_or_else(|| {
                protocol_error("synchronize the document before reading diagnostics")
            })?;
            (Revision::from(doc), Arc::clone(&doc.text))
        };
        let received = if self.has_pull_diagnostics() && !wait.is_zero() {
            let budget = RequestBudget::new(wait);
            loop {
                // Recomputed per iteration so the wait shrinks across retries
                // and a cancelled or exhausted budget ends the loop, matching
                // `refresh_document_diagnostics` itself.
                let remaining = budget.remaining().map_err(Error::from)?;
                match self.refresh_document_diagnostics(&uri, remaining).await {
                    Ok(()) => {}
                    Err(err)
                        if err.to_string().contains("-32802")
                            || err.to_string().contains("-32801")
                            || err.to_string().contains("server cancelled") => {}
                    Err(err) => return Err(err),
                }
                self.poll_notifications();
                let diags = Self::lock(&self.diagnostics)
                    .get(&uri)
                    .cloned()
                    .unwrap_or_default();
                let settled = !diags.is_empty()
                    || self.quiescent.load(Ordering::SeqCst)
                    || self.connected_at.elapsed() >= WARMUP_EMPTY_RESULT_WINDOW;
                if settled {
                    break;
                }
                if budget.remaining().is_err() || budget.remaining().unwrap() < WAIT_TICK * 2 {
                    break;
                }
                budget.pause(WAIT_TICK).await.map_err(Error::from)?;
            }
            true
        } else {
            self.wait_for_diagnostics(&uri, wait).await
        };
        owner
            .checkpoint()
            .map_err(|_| Error::from(super::LspCallError::Cancelled))?;
        if !self.is_alive() {
            return Err(Error::tool(
                "lsp",
                "[LSP_TRANSPORT_CLOSED] diagnostic connection closed",
            ));
        }
        if !received {
            return Err(Error::tool(
                "lsp",
                "[LSP_DIAGNOSTICS_PENDING] no diagnostic report arrived; this is not a clean result",
            ));
        }
        // Holding the document lock through the snapshot read prevents a
        // resync from clearing the cache between freshness check and clone.
        let docs = Self::lock(&self.open_docs);
        if !docs
            .get(&uri)
            .is_some_and(|doc| Revision::from(doc) == revision && Arc::ptr_eq(&doc.text, &source))
        {
            return Err(protocol_error(
                "document changed or closed while waiting for diagnostics",
            ));
        }
        Self::lock(&self.diagnostics)
            .get(&uri)
            .cloned()
            .ok_or_else(|| protocol_error("diagnostic report was invalidated before delivery"))
    }

    /// Refresh diagnostics using the server's negotiated pull protocol.
    ///
    /// The document must already be synchronized. Full reports replace prior
    /// results (including an empty array); unchanged reports reuse only the
    /// baseline sent on this request. Failures never manufacture a clean report.
    ///
    /// # Errors
    /// Returns protocol, timeout, cancellation or stale-document errors. A
    /// dropped request uses the client request layer's cancellation discipline.
    pub async fn refresh_document_diagnostics(&self, uri: &str, timeout: Duration) -> Result<()> {
        let budget = RequestBudget::new(timeout);
        budget.remaining().map_err(Error::from)?;
        let uri = file_uri::normalize_uri(uri)
            .filter(|uri| uri.len() <= MAX_RESULT_ID_BYTES)
            .ok_or_else(|| protocol_error("invalid or oversized document URI"))?;
        let identifier = {
            let caps = Self::lock(&self.capabilities);
            let options = caps
                .raw
                .get("diagnosticProvider")
                .filter(|value| value.is_object())
                .ok_or_else(|| protocol_error("server did not advertise pull diagnostics"))?;
            match options.get("identifier") {
                None => None,
                Some(Value::String(id)) if id.len() <= MAX_RESULT_ID_BYTES => Some(id.clone()),
                Some(_) => {
                    return Err(protocol_error(
                        "diagnostic provider identifier must be a bounded string",
                    ));
                }
            }
        };
        let (revision, source, ticket, previous) = {
            let docs = Self::lock(&self.open_docs);
            let doc = docs.get(&uri).ok_or_else(|| {
                protocol_error("synchronize the document before pulling diagnostics")
            })?;
            let revision = Revision::from(doc);
            // Retain the source allocation only for the in-flight request.
            // Pointer identity also detects close/reopen when version == 0.
            let source = Arc::clone(&doc.text);
            let mut cache = Self::lock(&self.pull_reports);
            let ticket = cache.start(&uri, &docs)?;
            let previous = cache.previous(&uri, revision);
            (revision, source, ticket, previous)
        };
        let mut params = json!({"textDocument":{"uri":uri}});
        if let Some(identifier) = identifier {
            params["identifier"] = json!(identifier);
        }
        if let Some(id) = previous
            .as_ref()
            .and_then(|report| report.result_id.as_ref())
        {
            params["previousResultId"] = json!(id);
        }
        let raw = self
            .call_with_budget("textDocument/diagnostic", params, &budget)
            .await
            .map_err(Error::from)?;
        let mut report = parse_report(&raw, revision, previous.as_ref())?;
        report.bytes = report.bytes.saturating_add(uri.len());
        budget.remaining().map_err(Error::from)?;
        // Same lock order as push acceptance and document synchronization.
        // A result cannot pass freshness checks, wait for a resync, then win.
        let docs = Self::lock(&self.open_docs);
        if !docs
            .get(&uri)
            .is_some_and(|doc| Revision::from(doc) == revision && Arc::ptr_eq(&doc.text, &source))
        {
            return Err(protocol_error(
                "document changed or closed during diagnostic request",
            ));
        }
        let mut cache = Self::lock(&self.pull_reports);
        if cache.latest.get(&uri) != Some(&ticket) {
            return Err(protocol_error(
                "a newer diagnostic request superseded this result",
            ));
        }
        let mut diagnostics = Self::lock(&self.diagnostics);
        diagnostics.insert(uri.clone(), report.items.as_ref().clone());
        for evicted in cache.insert(uri, report) {
            diagnostics.remove(&evicted);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
