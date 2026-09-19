//! Negotiated textDocument/diagnostic support for pull-only language servers.
//!
//! Reports stay tied to the synchronized document incarnation and the newest
//! local pull. Result IDs are opaque; an unchanged report is meaningful only
//! with the exact prior report sent on that request. Related-document reports
//! are not advertised or imported. The retained report working set is bounded.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::request::RequestBudget;
use super::{LspClient, OpenDoc, file_uri};
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
        Self { version: doc.version, hash: doc.disk_hash }
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
        self.serial = self.serial.checked_add(1)
            .ok_or_else(|| protocol_error("diagnostic request ID space exhausted"))?;
        self.latest.insert(uri.to_string(), self.serial);
        Ok(self.serial)
    }

    fn previous(&self, uri: &str, revision: Revision) -> Option<Report> {
        self.reports.get(uri)
            .filter(|report| report.revision == revision && report.result_id.is_some())
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
            let Some(key) = self.order.pop_front() else { break };
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
        self.remaining = self.remaining.checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("diagnostic report exceeds its byte limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
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
            let items = raw.get("items").and_then(Value::as_array)
                .ok_or_else(|| protocol_error("full report must contain an items array"))?;
            if items.len() > MAX_REPORT_ITEMS {
                return Err(protocol_error("diagnostic report has too many items"));
            }
            for diagnostic in items {
                let range = diagnostic.get("range").ok_or_else(|| protocol_error("diagnostic missing range"))?;
                let range: crate::lsp::text::Range = serde_json::from_value(range.clone())
                    .map_err(|_| protocol_error("invalid diagnostic range"))?;
                if range.end < range.start || diagnostic.get("message").and_then(Value::as_str).is_none() {
                    return Err(protocol_error("diagnostic needs an ordered range and string message"));
                }
            }
            Arc::new(items.clone())
        }
        Some("unchanged") => {
            if result_id.is_none() {
                return Err(protocol_error("unchanged report must contain a resultId"));
            }
            let previous = previous.filter(|report| report.revision == revision && report.result_id.is_some())
                .ok_or_else(|| protocol_error("unchanged report has no matching previous result"))?;
            if raw.get("items").is_some() {
                return Err(protocol_error("unchanged report must not replace items"));
            }
            Arc::clone(&previous.items)
        }
        _ => return Err(protocol_error("unknown document diagnostic report kind")),
    };
    let bytes = encoded_size(items.as_ref(), MAX_REPORT_BYTES)?
        .saturating_add(result_id.as_ref().map_or(0, String::len));
    Ok(Report { revision, result_id, items, bytes })
}

impl LspClient {
    pub(super) fn has_pull_diagnostics(&self) -> bool {
        Self::lock(&self.capabilities).raw.get("diagnosticProvider").is_some_and(Value::is_object)
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
            let options = caps.raw.get("diagnosticProvider").filter(|value| value.is_object())
                .ok_or_else(|| protocol_error("server did not advertise pull diagnostics"))?;
            match options.get("identifier") {
                None => None,
                Some(Value::String(id)) if id.len() <= MAX_RESULT_ID_BYTES => Some(id.clone()),
                Some(_) => return Err(protocol_error("diagnostic provider identifier must be a bounded string")),
            }
        };
        let (revision, ticket, previous) = {
            let docs = Self::lock(&self.open_docs);
            let revision = docs.get(&uri).map(Revision::from)
                .ok_or_else(|| protocol_error("synchronize the document before pulling diagnostics"))?;
            let mut cache = Self::lock(&self.pull_reports);
            let ticket = cache.start(&uri, &docs)?;
            let previous = cache.previous(&uri, revision);
            (revision, ticket, previous)
        };
        let mut params = json!({"textDocument":{"uri":uri}});
        if let Some(identifier) = identifier { params["identifier"] = json!(identifier); }
        if let Some(id) = previous.as_ref().and_then(|report| report.result_id.as_ref()) {
            params["previousResultId"] = json!(id);
        }
        let raw = self.call_with_budget("textDocument/diagnostic", params, &budget).await.map_err(Error::from)?;
        let mut report = parse_report(&raw, revision, previous.as_ref())?;
        report.bytes = report.bytes.saturating_add(uri.len());
        budget.remaining().map_err(Error::from)?;
        // Same lock order as push acceptance and document synchronization.
        // A result cannot pass freshness checks, wait for a resync, then win.
        let docs = Self::lock(&self.open_docs);
        if docs.get(&uri).map(Revision::from) != Some(revision) {
            return Err(protocol_error("document changed or closed during diagnostic request"));
        }
        let mut cache = Self::lock(&self.pull_reports);
        if cache.latest.get(&uri) != Some(&ticket) {
            return Err(protocol_error("a newer diagnostic request superseded this result"));
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
