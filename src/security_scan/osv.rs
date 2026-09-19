//! Native OSV lookups for an offline-derived public dependency inventory.
//! Query completion and advisory-detail completion are deliberately separate.

use super::dependencies::{Inventory, Package};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::http::client::Client as HttpClient;
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolOutput;
use futures::future::{Either, select};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};
use url::Url;

mod artifact;
#[cfg(test)]
mod tests;

const SCHEMA: &str = "pi.security-dependency-audit/v1";
const BATCH_SIZE: usize = 100;
const MAX_REQUESTS: usize = 128;
const MAX_PAGES: usize = 8;
const MAX_MATCHES: usize = 2000;
const MAX_DETAILS: usize = 128;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Input {
    #[serde(rename = "op")]
    _op: String,
    #[serde(default)]
    paths: Vec<String>,
    timeout_ms: Option<u64>,
    sarif_out: Option<String>,
}

pub(super) struct Client {
    http: HttpClient,
    base: Url,
}
impl Default for Client {
    fn default() -> Self {
        Self {
            http: HttpClient::default(),
            base: Url::parse("https://api.osv.dev/").expect("static OSV URL"),
        }
    }
}

impl Client {
    pub(super) fn with_base(mut self, raw: &str) -> Result<Self> {
        let mut base = Url::parse(raw).map_err(|_| tool_error("invalid OSV API base"))?;
        let loopback = match base.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !(base.scheme() == "https" || base.scheme() == "http" && loopback)
            || base.host().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(tool_error(
                "OSV base requires HTTPS (HTTP only for literal loopback), without credentials, query or fragment",
            ));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        self.base = base;
        Ok(self)
    }

    pub(super) fn with_http(mut self, http: HttpClient) -> Self {
        self.http = http;
        self
    }

    async fn exchange(
        &self,
        owner: &AgentCx,
        path: &str,
        payload: Option<&Value>,
        deadline: Instant,
    ) -> std::result::Result<Value, Failure> {
        owner
            .checkpoint()
            .map_err(|_| Failure::new("OSV_CANCELLED", "dependency audit cancelled"))?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                Failure::new(
                    "OSV_TIMEOUT",
                    "dependency audit deadline elapsed before dispatch",
                )
            })?;
        let url = self
            .base
            .join(path)
            .map_err(|_| Failure::new("OSV_ENDPOINT", "invalid API path"))?;
        if url.origin() != self.base.origin() || !url.path().starts_with(self.base.path()) {
            return Err(Failure::new(
                "OSV_ENDPOINT",
                "API path escaped configured base",
            ));
        }
        let client = owner.http().bind(&self.http);
        let request = match payload {
            Some(payload) => client
                .post(url.as_str())
                .json(payload)
                .map_err(|_| Failure::new("OSV_REQUEST", "could not encode query"))?,
            None => client.get(url.as_str()),
        }
        .timeout(remaining);
        let response = request.send().await.map_err(|_| {
            Failure::new(
                "OSV_UNAVAILABLE",
                "OSV request failed; no offline-clean fallback was used",
            )
        })?;
        if response.status() != 200 {
            return Err(Failure::new(
                "OSV_HTTP",
                &format!(
                    "OSV returned HTTP {}; query was not completed",
                    response.status()
                ),
            ));
        }
        if let Some((_, content_type)) = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            let content_type = content_type.split(';').next().unwrap_or("").trim();
            if !content_type.eq_ignore_ascii_case("application/json") {
                return Err(Failure::new("OSV_PROTOCOL", "OSV response was not JSON"));
            }
        }
        let limit = if payload.is_some() {
            MAX_RESPONSE
        } else {
            512 * 1024
        };
        let bytes = response.bytes_limited(limit).await.map_err(|_| {
            Failure::new(
                "OSV_RESPONSE",
                "OSV response exceeded its bound or ended incompletely",
            )
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|_| Failure::new("OSV_PROTOCOL", "invalid OSV JSON response"))
    }

    /// The outer deadline covers inventory and all query pages/metadata. Local
    /// filesystem calls and synchronous report serialization are not preemptible.
    pub(super) async fn execute(&self, cwd: &Path, input: Value) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)
            .map_err(|_| tool_error("invalid audit_dependencies arguments"))?;
        let milliseconds = input.timeout_ms.unwrap_or(60_000);
        if !(1..=120_000).contains(&milliseconds) {
            return Err(tool_error("timeoutMs must be in 1..=120000"));
        }
        if let Some(path) = &input.sarif_out {
            artifact::validate_name(path)?;
        }
        let owner = AgentCx::for_current_or_request();
        if !owner.capabilities().io || !owner.capabilities().time {
            return Err(tool_error(
                "dependency auditing requires I/O and timer capabilities",
            ));
        }
        owner
            .checkpoint()
            .map_err(|_| tool_error("dependency audit cancelled"))?;
        let duration = Duration::from_millis(milliseconds);
        let deadline = Instant::now() + duration;
        let operation = async {
            let workspace = cwd.to_path_buf();
            let paths = input.paths;
            let inventory = asupersync::runtime::spawn_blocking(move || {
                super::dependencies::inventory(&workspace, &paths)
            })
            .await?;
            owner
                .checkpoint()
                .map_err(|_| tool_error("dependency audit cancelled"))?;
            let mut report = self.lookup(&owner, inventory, deadline).await;
            owner
                .checkpoint()
                .map_err(|_| tool_error("dependency audit cancelled"))?;
            if Instant::now() >= deadline {
                return Err(tool_error(
                    "dependency audit deadline elapsed; no complete result is claimed",
                ));
            }
            if let Some(path) = &input.sarif_out {
                // Export is synchronous and bounded. It creates a new file only;
                // no report or source is overwritten, including on a name race.
                artifact::publish(cwd, path, &report.sarif(), &owner, deadline)?;
                report.sarif_path = Some(path.clone());
            }
            report.output()
        };
        let cancelled = async {
            let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
            let _ = receiver.recv(owner.cx()).await;
            drop(sender);
        };
        let watchdog = async {
            match select(Box::pin(owner.time().sleep(duration)), Box::pin(cancelled)).await {
                Either::Left(_) => "dependency audit timed out; no complete result is claimed",
                Either::Right(_) => "dependency audit cancelled; no complete result is claimed",
            }
        };
        match select(Box::pin(operation), Box::pin(watchdog)).await {
            Either::Left((result, _)) => result,
            Either::Right((message, pending)) => {
                drop(pending);
                Err(tool_error(message))
            }
        }
    }

    async fn lookup(&self, owner: &AgentCx, inventory: Inventory, deadline: Instant) -> Report {
        let mut report = Report::new(inventory);
        if report.inventory.packages.is_empty() {
            report.failures.push(Failure::new(
                "OSV_NOT_CHECKED",
                "no eligible public-registry packages; no OSV request was sent",
            ));
            return report;
        }
        let mut pending: VecDeque<Work> = (0..report.queries.len())
            .map(|index| Work { index, token: None })
            .collect();
        let mut requests = 0;
        while !pending.is_empty() {
            if requests == MAX_REQUESTS {
                report
                    .failures
                    .push(Failure::new("OSV_LIMIT", "query request budget exhausted"));
                break;
            }
            let batch: Vec<_> = (0..BATCH_SIZE)
                .filter_map(|_| pending.pop_front())
                .collect();
            let queries: Vec<_> = batch
                .iter()
                .map(|work| work.query(&report.inventory.packages[work.index]))
                .collect();
            requests += 1;
            let response = self
                .exchange(
                    owner,
                    "v1/querybatch",
                    Some(&json!({"queries":queries})),
                    deadline,
                )
                .await;
            let applied = response.and_then(|response| report.apply_pages(&batch, &response));
            match applied {
                Ok(next) => pending.extend(next),
                Err(failure) => {
                    report.failures.push(failure);
                    break;
                }
            }
        }
        report.query_complete = report.queries.iter().all(|query| query.complete);
        let ids: BTreeSet<_> = report
            .queries
            .iter()
            .flat_map(|query| query.advisories.keys().cloned())
            .collect();
        report.metadata_complete = ids.len() <= MAX_DETAILS;
        if ids.len() > MAX_DETAILS {
            report.failures.push(Failure::new(
                "OSV_DETAIL_LIMIT",
                "advisory detail budget exceeded; all collected IDs remain visible",
            ));
        }
        // Do not spend a second request budget on metadata after lookup failure.
        if !report.query_complete {
            report.metadata_complete = false;
            return report;
        }
        for id in ids.into_iter().take(MAX_DETAILS) {
            let response = self
                .exchange(owner, &format!("v1/vulns/{id}"), None, deadline)
                .await;
            match response.and_then(|value| Detail::parse(&id, &value)) {
                Ok(detail) => {
                    report.details.insert(id, detail);
                }
                Err(failure) => {
                    report.metadata_complete = false;
                    report.failures.push(failure);
                    // No retry storm against an unavailable service. The rest
                    // stay matched IDs with unavailable metadata, not clean.
                    break;
                }
            }
        }
        report
    }
}

fn tool_error(message: impl Into<String>) -> Error {
    Error::tool(
        "security_scan",
        format!("[DEPENDENCY_AUDIT] {}", message.into()),
    )
}

#[derive(Debug, Clone, Serialize)]
struct Failure {
    code: String,
    message: String,
}
impl Failure {
    fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone)]
struct Work {
    index: usize,
    token: Option<String>,
}
impl Work {
    fn query(&self, package: &Package) -> Value {
        let mut query = json!({"package":{"ecosystem":package.ecosystem,"name":package.name},"version":package.version});
        if let Some(token) = &self.token {
            query["page_token"] = json!(token);
        }
        query
    }
}

#[derive(Default)]
struct Query {
    advisories: BTreeMap<String, String>,
    complete: bool,
    pages: usize,
    tokens: BTreeSet<String>,
}

struct Page {
    advisories: BTreeMap<String, String>,
    token: Option<String>,
}
fn parse_pages(value: &Value, expected: usize) -> std::result::Result<Vec<Page>, Failure> {
    let invalid = || {
        Failure::new(
            "OSV_PROTOCOL",
            "malformed or uncorrelated OSV batch response",
        )
    };
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if results.len() != expected {
        return Err(invalid());
    }
    results
        .iter()
        .map(|result| {
            if !result.is_object() || result.get("error").is_some() {
                return Err(invalid());
            }
            let mut advisories = BTreeMap::new();
            if let Some(vulns) = result.get("vulns") {
                let vulns = vulns.as_array().ok_or_else(invalid)?;
                if vulns.len() > MAX_MATCHES {
                    return Err(Failure::new("OSV_LIMIT", "too many advisory matches"));
                }
                for vuln in vulns {
                    let id = vuln
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| valid_id(id))
                        .ok_or_else(invalid)?;
                    let modified = vuln
                        .get("modified")
                        .and_then(Value::as_str)
                        .filter(|value| {
                            value.len() <= 80 && chrono::DateTime::parse_from_rfc3339(value).is_ok()
                        })
                        .ok_or_else(invalid)?;
                    advisories.insert(id.to_string(), modified.to_string());
                }
            }
            let token = match result.get("next_page_token") {
                None => None,
                Some(Value::String(token)) if token.is_empty() => None,
                Some(Value::String(token))
                    if token.len() <= 4096 && !token.chars().any(char::is_control) =>
                {
                    Some(token.clone())
                }
                _ => return Err(invalid()),
            };
            Ok(Page { advisories, token })
        })
        .collect()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        && !matches!(id, "." | "..")
}

#[derive(Default)]
struct Detail {
    summary: String,
    aliases: Vec<String>,
    withdrawn: bool,
    fixed: BTreeMap<(String, String), BTreeSet<String>>,
}
impl Detail {
    fn parse(id: &str, value: &Value) -> std::result::Result<Self, Failure> {
        let invalid = || {
            Failure::new(
                "OSV_METADATA",
                "advisory details were malformed or belonged to another ID",
            )
        };
        if value.get("id").and_then(Value::as_str) != Some(id) {
            return Err(invalid());
        }
        let summary = match value.get("summary") {
            None => String::new(),
            Some(Value::String(summary)) => summary
                .chars()
                .filter(|ch| !ch.is_control())
                .take(512)
                .collect(),
            _ => return Err(invalid()),
        };
        let mut detail = Self {
            summary,
            ..Self::default()
        };
        if let Some(withdrawn) = value.get("withdrawn") {
            let date = withdrawn
                .as_str()
                .filter(|date| {
                    date.len() <= 80 && chrono::DateTime::parse_from_rfc3339(date).is_ok()
                })
                .ok_or_else(invalid)?;
            detail.withdrawn = !date.is_empty();
        }
        if let Some(aliases) = value.get("aliases") {
            let aliases = aliases.as_array().ok_or_else(invalid)?;
            if aliases.len() > 128 {
                return Err(invalid());
            }
            for alias in aliases {
                detail.aliases.push(
                    alias
                        .as_str()
                        .filter(|id| valid_id(id))
                        .ok_or_else(invalid)?
                        .to_string(),
                );
            }
        }
        if let Some(affected) = value.get("affected") {
            let affected = affected.as_array().ok_or_else(invalid)?;
            if affected.len() > 1024 {
                return Err(invalid());
            }
            for affected in affected {
                if !affected.is_object() {
                    return Err(invalid());
                }
                let package = &affected["package"];
                let (Some(ecosystem), Some(name)) =
                    (package["ecosystem"].as_str(), package["name"].as_str())
                else {
                    continue;
                };
                if !matches!(ecosystem, "crates.io" | "npm")
                    || !super::dependencies::valid_name(ecosystem, name)
                {
                    continue;
                }
                parse_affected_ranges(&mut detail, ecosystem, name, affected, &invalid)?;
            }
        }
        Ok(detail)
    }
}

fn parse_affected_ranges(
    detail: &mut Detail,
    ecosystem: &str,
    name: &str,
    affected: &Value,
    invalid: &impl Fn() -> Failure,
) -> std::result::Result<(), Failure> {
    if let Some(ranges) = affected.get("ranges") {
        let ranges = ranges.as_array().ok_or_else(invalid)?;
        if ranges.len() > 128 {
            return Err(invalid());
        }
        for range in ranges {
            // Git fix hashes and unfamiliar range types are not
            // package upgrade versions, even when they look numeric.
            if !matches!(
                range.get("type").and_then(Value::as_str),
                Some("SEMVER" | "ECOSYSTEM")
            ) {
                continue;
            }
            let events = range
                .get("events")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            if events.len() > 1024 {
                return Err(invalid());
            }
            for event in events {
                if let Some(version) = event.get("fixed") {
                    let version = version
                        .as_str()
                        .filter(|version| version.len() <= 256)
                        .ok_or_else(invalid)?;
                    if semver::Version::parse(version).is_ok() {
                        let versions = detail
                            .fixed
                            .entry((ecosystem.to_string(), name.to_string()))
                            .or_default();
                        if versions.len() == 128 && !versions.contains(version) {
                            return Err(invalid());
                        }
                        versions.insert(version.to_string());
                    }
                }
            }
        }
    }
    Ok(())
}

struct Report {
    observed_at: String,
    inventory: Inventory,
    queries: Vec<Query>,
    details: BTreeMap<String, Detail>,
    query_complete: bool,
    metadata_complete: bool,
    failures: Vec<Failure>,
    sarif_path: Option<String>,
}
impl Report {
    fn new(inventory: Inventory) -> Self {
        Self {
            observed_at: chrono::Utc::now().to_rfc3339(),
            queries: (0..inventory.packages.len())
                .map(|_| Query::default())
                .collect(),
            inventory,
            details: BTreeMap::new(),
            query_complete: false,
            metadata_complete: false,
            failures: Vec::new(),
            sarif_path: None,
        }
    }

    // Validate a whole response before changing query-completion state. A short
    // array, malformed entry or cyclic token cannot partially certify a batch.
    fn apply_pages(
        &mut self,
        work: &[Work],
        value: &Value,
    ) -> std::result::Result<Vec<Work>, Failure> {
        let pages = parse_pages(value, work.len())?;
        let mut count: usize = self
            .queries
            .iter()
            .map(|query| query.advisories.len())
            .sum();
        for (work, page) in work.iter().zip(&pages) {
            let query = &self.queries[work.index];
            if query.complete
                || query.pages >= MAX_PAGES
                || page
                    .token
                    .as_ref()
                    .is_some_and(|token| query.tokens.contains(token))
            {
                return Err(Failure::new(
                    "OSV_PAGINATION",
                    "repeated page token or per-package page limit",
                ));
            }
            count += page
                .advisories
                .keys()
                .filter(|id| !query.advisories.contains_key(*id))
                .count();
            if count > MAX_MATCHES {
                return Err(Failure::new("OSV_LIMIT", "advisory match budget exceeded"));
            }
        }
        let mut next = Vec::new();
        for (work, page) in work.iter().zip(pages) {
            let query = &mut self.queries[work.index];
            query.advisories.extend(page.advisories);
            query.pages += 1;
            if let Some(token) = page.token {
                query.tokens.insert(token.clone());
                next.push(Work {
                    index: work.index,
                    token: Some(token),
                });
            } else {
                query.complete = true;
            }
        }
        Ok(next)
    }

    const fn status(&self) -> &'static str {
        if self.inventory.packages.is_empty() {
            "not_checked"
        } else if !self.query_complete {
            "incomplete"
        } else if !self.metadata_complete {
            "complete_with_metadata_gaps"
        } else {
            "complete"
        }
    }

    fn matches(&self) -> impl Iterator<Item = (usize, &str, &str)> {
        self.queries.iter().enumerate().flat_map(|(index, query)| {
            query
                .advisories
                .iter()
                .map(move |(id, modified)| (index, id.as_str(), modified.as_str()))
        })
    }

    fn finding(&self, index: usize, id: &str, modified: &str) -> Value {
        let package = &self.inventory.packages[index];
        let detail = self.details.get(id);
        let fixed = detail.and_then(|detail| {
            detail
                .fixed
                .get(&(package.ecosystem.clone(), package.name.clone()))
        });
        json!({"packageIndex":index,"ecosystem":package.ecosystem,"name":package.name,"version":package.version,
            "advisoryId":id,"modified":modified,"fingerprint":finding_id(package,id),
            "url":format!("https://osv.dev/vulnerability/{id}"),
            "summary":detail.map(|detail| &detail.summary),
            "aliases":detail.map(|detail| detail.aliases.iter().take(16).collect::<Vec<_>>()),
            "aliasesTruncated":detail.is_some_and(|detail|detail.aliases.len()>16),
            "withdrawn":detail.map(|detail| detail.withdrawn),
            "fixedVersionBoundaries":fixed.map(|versions|versions.iter().take(16).collect::<Vec<_>>()),
            "fixedVersionsTruncated":fixed.is_some_and(|versions|versions.len()>16),
            "metadataAvailable":detail.is_some()})
    }

    fn output(&self) -> Result<ToolOutput> {
        let count = self.matches().count();
        let withdrawn = self
            .matches()
            .filter(|(_, id, _)| self.details.get(*id).is_some_and(|detail| detail.withdrawn))
            .count();
        let checked = self.queries.iter().filter(|query| query.complete).count();
        let mut text = format!(
            "Dependency audit {}: {checked}/{} package/version queries completed; {count} advisory matches ({withdrawn} withdrawn), {} excluded lockfile entries. This is a known-advisory lookup, not proof of exploitability or absence of vulnerabilities.",
            self.status(),
            self.queries.len(),
            self.inventory.excluded.len()
        );
        for (index, id, _) in self.matches().take(20) {
            let package = &self.inventory.packages[index];
            let _ = write!(
                text,
                "\n{} {}@{}: {}",
                package.ecosystem, package.name, package.version, id
            );
        }
        if let Some(path) = &self.sarif_path {
            let _ = write!(text, "\nSARIF: {path}");
        }
        if count > 50 && self.sarif_path.is_none() {
            text.push_str("\nOnly the first 50 matches are included here; supply sarifOut for a complete bounded report.");
        }
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(
                json!({"schema":SCHEMA,"observedAt":self.observed_at,"scope":self.inventory.scope,
                "status":self.status(),"queryComplete":self.query_complete,
                "lockfiles":self.inventory.lockfiles,
                "metadataComplete":self.metadata_complete,"eligiblePackages":self.queries.len(),"completedQueries":checked,
                "advisoryMatches":count,"withdrawnMatches":withdrawn,"excludedEntries":self.inventory.excluded.len(),
                "excludedPreview":self.inventory.excluded.iter().take(20).collect::<Vec<_>>(),
                "findings":self.matches().take(50).map(|(index,id,modified)|self.finding(index,id,modified)).collect::<Vec<_>>(),
                "findingsTruncated":count > 50,"failures":self.failures,"sarif":self.sarif_path}),
            ),
            is_error: !self.query_complete,
        };
        if serde_json::to_vec(&output)?.len() > crate::tools::DEFAULT_MAX_BYTES {
            return Err(tool_error(
                "dependency audit preview exceeds the tool-output limit; narrow the lockfile scope",
            ));
        }
        Ok(output)
    }

    fn sarif(&self) -> Value {
        let results: Vec<_> = self.matches().map(|(index,id,modified)| {
            let package = &self.inventory.packages[index];
            let finding = self.finding(index,id,modified);
            let withdrawn = self.details.get(id).is_some_and(|detail|detail.withdrawn);
            json!({"ruleId":format!("osv/{id}"),"level":if withdrawn {"note"} else {"warning"},
                "message":{"text":format!("{} {}@{} matched {} in OSV; review advisory details and applicability",package.ecosystem,package.name,package.version,id)},
                "fingerprints":{"pi/dependency/v1":finding_id(package,id)},
                "locations":package.locations.iter().take(4).map(|location|json!({"physicalLocation":{"artifactLocation":{"uri":relative_uri(&location.lockfile)}}})).collect::<Vec<_>>(),
                "properties":{"dependency":finding,"locationsTruncated":package.locations.len()>4}})
        }).collect();
        json!({"version":super::SARIF_VERSION,"$schema":super::SARIF_SCHEMA_URI,"runs":[{
            "tool":{"driver":{"name":"pi dependency audit","version":env!("CARGO_PKG_VERSION")}},
            "invocations":[{"executionSuccessful":self.query_complete,"toolExecutionNotifications":self.failures.iter().map(|failure|json!({"level":"warning","message":{"text":failure.message},"descriptor":{"id":failure.code}})).collect::<Vec<_>>() }],
            "results":results,
            "properties":{"schema":SCHEMA,"observedAt":self.observed_at,"status":self.status(),"queryComplete":self.query_complete,"metadataComplete":self.metadata_complete,
                "completedQueries":self.queries.iter().filter(|query|query.complete).count(),"inventory":self.inventory,
                "queryCompletion":self.queries.iter().map(|query|query.complete).collect::<Vec<_>>()}
        }]})
    }
}

fn finding_id(package: &Package, id: &str) -> String {
    use std::fmt::Write as _;
    let mut hash = Sha256::new();
    for part in [
        "pi/dependency/v1",
        &package.ecosystem,
        &package.name,
        &package.version,
        id,
    ] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hash.finalize()
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

fn relative_uri(path: &str) -> String {
    use std::fmt::Write as _;
    path.bytes().fold(String::new(), |mut uri, byte| {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
        uri
    })
}
