//! Bounded request-hop summaries. Never fetch bodies or copy headers/cookies.
//! The parent admits the exact CDP session; this collector also checks the
//! initiating frame and document before retaining a request identity.

use super::{admitted_url, display_url, finite_number};
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};

const MAX_BYTES: usize = 64 * 1024;
const MAX_IDENTITIES: usize = 1024;

struct Hop {
    id: String,
    sequence: u64,
    start: f64,
    last: f64,
    active: bool,
    fields: Value,
}

pub(super) struct Trace {
    hops: VecDeque<Hop>,
    // Tombstones prevent an evicted or finished request from being mistaken
    // for a new request with the same wire identity. Stop admitting at capacity.
    seen: BTreeSet<String>,
    max_events: usize,
    sequence: u64,
    dropped: u64,
    filtered: u64,
    invalid: u64,
    fields_truncated: bool,
    admission_stopped: bool,
}

fn token(value: &Value, maximum: usize) -> Option<&str> {
    value.as_str().filter(|text| {
        !text.is_empty()
            && text.len() <= maximum
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:/".contains(&byte))
    })
}

fn response_fields(response: &Value, allowlist: Option<&[String]>) -> Option<Value> {
    let url = display_url(response["url"].as_str()?, allowlist)?;
    let status = finite_number(&response["status"])?;
    if status > 999.0 || status.fract() != 0.0 {
        return None;
    }
    Some(json!({
        "url": url, "status": status,
        "mime_type": token(&response["mimeType"], 128),
        "protocol": token(&response["protocol"], 32),
        "from_disk_cache": response["fromDiskCache"].as_bool(),
        "from_service_worker": response["fromServiceWorker"].as_bool(),
    }))
}

fn url_shortened(value: &Value) -> bool {
    let Some(raw) = value.as_str().filter(|raw| raw.len() <= 64 * 1024) else {
        return false;
    };
    let Ok(mut url) = url::Url::parse(raw) else {
        return false;
    };
    url.set_query(None);
    url.set_fragment(None);
    url.as_str().len() > super::MAX_FIELD_BYTES
}

impl Trace {
    pub(super) const fn new(max_events: usize) -> Self {
        Self {
            hops: VecDeque::new(),
            seen: BTreeSet::new(),
            max_events,
            sequence: 0,
            dropped: 0,
            filtered: 0,
            invalid: 0,
            fields_truncated: false,
            admission_stopped: false,
        }
    }

    fn active(&self, id: &str) -> Option<usize> {
        self.hops.iter().position(|hop| hop.id == id && hop.active)
    }

    fn bound(&mut self) {
        let mut bytes: usize = self
            .hops
            .iter()
            .map(|hop| hop.fields.to_string().len())
            .sum();
        while self.hops.len() > self.max_events || bytes > MAX_BYTES {
            let Some(hop) = self.hops.pop_front() else {
                break;
            };
            bytes -= hop.fields.to_string().len();
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    fn timestamp(&mut self, index: usize, value: &Value) -> Option<f64> {
        let time = finite_number(value)?;
        let hop = &mut self.hops[index];
        if time < hop.last {
            return None;
        }
        let elapsed = (time - hop.start) * 1000.0;
        if !elapsed.is_finite() {
            return None;
        }
        hop.last = time;
        hop.fields["elapsed_ms"] = json!(elapsed);
        Some(time)
    }

    #[allow(clippy::too_many_lines)]
    fn start(
        &mut self,
        params: &Value,
        main_frame: &str,
        allowlist: Option<&[String]>,
        phase: &str,
    ) {
        let Some(id) = token(&params["requestId"], 256) else {
            self.invalid = self.invalid.saturating_add(1);
            return;
        };
        if params["frameId"].as_str() != Some(main_frame) {
            self.filtered = self.filtered.saturating_add(1);
            return;
        }
        let mut redirect_from = None;
        if let Some(index) = self.active(id) {
            // A repeated start without redirect evidence cannot rebind a live
            // identity. Keep the preceding facts but retire its correlation.
            let redirect = response_fields(&params["redirectResponse"], allowlist);
            if let Some(redirect) = redirect
                && redirect["url"] == self.hops[index].fields["url"]
                && self.timestamp(index, &params["timestamp"]).is_some()
            {
                let hop = &mut self.hops[index];
                self.fields_truncated |= url_shortened(&params["redirectResponse"]["url"]);
                hop.fields["response"] = redirect;
                hop.fields["state"] = json!("redirected");
                redirect_from = Some(hop.sequence);
                hop.active = false;
            } else {
                self.hops[index].fields["state"] = json!("correlation_lost");
                self.hops[index].active = false;
                self.invalid = self.invalid.saturating_add(1);
                return;
            }
        } else if self.seen.contains(id) {
            self.filtered = self.filtered.saturating_add(1);
            return;
        }
        // Validate the successor independently: an allowed request may redirect
        // to a forbidden URL. Its old hop can finish, but the new one is omitted.
        let request = &params["request"];
        let admitted = params["frameId"].as_str() == Some(main_frame)
            && params["documentURL"]
                .as_str()
                .is_some_and(|url| admitted_url(url, allowlist));
        let fields = admitted
            .then(|| {
                Some((
                    display_url(request["url"].as_str()?, allowlist)?,
                    token(&request["method"], 32)?.to_string(),
                    finite_number(&params["timestamp"])?,
                ))
            })
            .flatten();
        // Remember rejected identities too so later unscoped events cannot
        // resurrect them. This set is independent of the latest-result tail.
        if !self.seen.contains(id) {
            if self.seen.len() >= MAX_IDENTITIES {
                self.admission_stopped = true;
                self.filtered = self.filtered.saturating_add(1);
                return;
            }
            self.seen.insert(id.to_string());
        }
        let Some((url, method, start)) = fields else {
            self.filtered = self.filtered.saturating_add(1);
            return;
        };
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.admission_stopped = true;
            return;
        };
        self.sequence = sequence;
        let url_truncated = url_shortened(&request["url"]);
        self.fields_truncated |= url_truncated;
        let fields = json!({
            "sequence": sequence, "redirect_from": redirect_from,
            "phase": phase, "url": url, "method": method,
            "url_may_be_shortened": url_truncated,
            "resource_type": token(&params["type"], 32),
            "state": "pending", "started_at": start,
            "start_seen": true,
            "redirect_start_missing": redirect_from.is_none() && params.get("redirectResponse").is_some(),
        });
        self.hops.push_back(Hop {
            id: id.to_string(),
            sequence,
            start,
            last: start,
            active: true,
            fields,
        });
    }

    pub(super) fn observe(
        &mut self,
        event: &Value,
        main_frame: &str,
        allowlist: Option<&[String]>,
        phase: &str,
    ) {
        let method = event["method"].as_str().unwrap_or_default();
        if !matches!(
            method,
            "Network.requestWillBeSent"
                | "Network.responseReceived"
                | "Network.loadingFinished"
                | "Network.loadingFailed"
                | "Network.requestServedFromCache"
        ) {
            return;
        }
        let params = &event["params"];
        if method == "Network.requestWillBeSent" {
            self.start(params, main_frame, allowlist, phase);
            self.bound();
            return;
        }
        let Some(id) = token(&params["requestId"], 256) else {
            self.invalid = self.invalid.saturating_add(1);
            return;
        };
        let Some(index) = self.active(id) else {
            self.filtered = self.filtered.saturating_add(1);
            return;
        };
        if method == "Network.requestServedFromCache" {
            self.hops[index].fields["served_from_cache"] = json!(true);
        } else {
            let response = if method == "Network.responseReceived" {
                if self.hops[index].fields.get("response").is_some() {
                    self.invalid = self.invalid.saturating_add(1);
                    return;
                }
                if params.get("frameId").is_some() && params["frameId"].as_str() != Some(main_frame)
                {
                    self.invalid = self.invalid.saturating_add(1);
                    return;
                }
                let Some(response) = response_fields(&params["response"], allowlist) else {
                    self.invalid = self.invalid.saturating_add(1);
                    return;
                };
                if response["url"] != self.hops[index].fields["url"] {
                    self.invalid = self.invalid.saturating_add(1);
                    return;
                }
                self.fields_truncated |= url_shortened(&params["response"]["url"]);
                Some(response)
            } else {
                None
            };
            if self.timestamp(index, &params["timestamp"]).is_none() {
                self.invalid = self.invalid.saturating_add(1);
                return;
            }
            let hop = &mut self.hops[index];
            if let Some(response) = response {
                hop.fields["response"] = response;
                hop.fields["state"] = json!("responded");
            } else {
                hop.active = false;
                if method == "Network.loadingFinished" {
                    hop.fields["state"] = json!("completed");
                    hop.fields["encoded_bytes"] =
                        json!(finite_number(&params["encodedDataLength"]));
                } else {
                    hop.fields["state"] = json!("failed");
                    // Chromium normally supplies net::ERR_* here. Do not copy
                    // arbitrary diagnostics that might contain a URL or cookie.
                    hop.fields["error_code"] =
                        json!(token(&params["errorText"], 128).filter(|text| {
                            text.strip_prefix("net::ERR_").is_some_and(|code| {
                                !code.is_empty()
                                    && code.bytes().all(|byte| {
                                        byte.is_ascii_uppercase()
                                            || byte.is_ascii_digit()
                                            || byte == b'_'
                                    })
                            })
                        }));
                    hop.fields["blocked_reason"] = json!(token(&params["blockedReason"], 64));
                    hop.fields["cors_error"] =
                        json!(token(&params["corsErrorStatus"]["corsError"], 128));
                    hop.fields["canceled"] = json!(params["canceled"].as_bool());
                }
            }
        }
        self.bound();
    }

    pub(super) fn report(self) -> Value {
        let pending = self.hops.iter().filter(|hop| hop.active).count();
        json!({
            "scope": "admitted_main_frame_requests", "pending_at_end": pending,
            "truncated": self.dropped > 0 || self.admission_stopped || self.fields_truncated,
            "fields_truncated": self.fields_truncated,
            "dropped_requests": self.dropped, "filtered_events": self.filtered,
            "invalid_events": self.invalid, "admission_stopped": self.admission_stopped,
            "max_request_identities": MAX_IDENTITIES,
            "headers_and_bodies": "omitted", "complete_history": false,
            "requests": self.hops.into_iter().map(|hop| hop.fields).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(id: &str, url: &str, time: f64) -> Value {
        json!({"method":"Network.requestWillBeSent","params":{
            "requestId":id,"frameId":"main","documentURL":"https://example.com/app",
            "timestamp":time,"type":"Fetch","request":{
                "url":url,"method":"GET","headers":{"Cookie":"PRIVATE-COOKIE"},
                "postData":"PRIVATE-BODY"
            }
        }})
    }
    fn observe(trace: &mut Trace, event: &Value) {
        trace.observe(event, "main", Some(&["example.com".into()]), "action");
    }
    fn response(id: &str, status: u64, time: f64) -> Value {
        json!({"method":"Network.responseReceived","params":{
            "requestId":id,"frameId":"main","timestamp":time,"response":{
                "url":"https://example.com/api?PRIVATE#PRIVATE","status":status,
                "mimeType":"application/json","protocol":"h2","fromDiskCache":true,
                "headers":{"Set-Cookie":"PRIVATE-COOKIE"}
            }
        }})
    }

    #[test]
    fn real_request_lifecycle_keeps_http_failure_separate_from_transport_failure() {
        let mut trace = Trace::new(100);
        observe(
            &mut trace,
            &start("1", "https://example.com/api?PRIVATE#PRIVATE", 10.0),
        );
        observe(&mut trace, &response("1", 503, 10.25));
        observe(
            &mut trace,
            &json!({"method":"Network.loadingFinished","params":{
                "requestId":"1","timestamp":10.5,"encodedDataLength":80
            }}),
        );
        observe(&mut trace, &start("2", "https://example.com/offline", 11.0));
        observe(
            &mut trace,
            &json!({"method":"Network.loadingFailed","params":{
                "requestId":"2","timestamp":11.125,"errorText":"net::ERR_CONNECTION_REFUSED","canceled":false
            }}),
        );
        let report = trace.report();
        assert_eq!(report["requests"][0]["state"], "completed");
        assert_eq!(
            report["requests"][0]["response"]["status"].as_f64(),
            Some(503.0)
        );
        assert_eq!(report["requests"][0]["elapsed_ms"].as_f64(), Some(500.0));
        assert_eq!(report["requests"][1]["state"], "failed");
        assert_eq!(
            report["requests"][1]["error_code"],
            "net::ERR_CONNECTION_REFUSED"
        );
        assert!(!report.to_string().contains("PRIVATE"));
        assert_eq!(report["pending_at_end"], 0);
    }

    #[test]
    fn redirects_are_distinct_hops_and_forbidden_successors_are_not_adopted() {
        let mut trace = Trace::new(100);
        observe(&mut trace, &start("1", "https://example.com/first", 1.0));
        let mut redirected = start("1", "https://example.com/second", 2.0);
        redirected["params"]["redirectResponse"] =
            json!({"url":"https://example.com/first","status":302});
        observe(&mut trace, &redirected);
        let mut forbidden = start("1", "https://forbidden.test/PRIVATE", 3.0);
        forbidden["params"]["redirectResponse"] =
            json!({"url":"https://example.com/second","status":307});
        observe(&mut trace, &forbidden);
        observe(&mut trace, &response("1", 200, 4.0));
        let report = trace.report();
        assert_eq!(report["requests"].as_array().unwrap().len(), 2);
        assert_eq!(report["requests"][1]["redirect_from"], 1);
        assert_eq!(report["requests"][0]["state"], "redirected");
        assert_eq!(report["requests"][1]["state"], "redirected");
        assert!(!report.to_string().contains("PRIVATE"));
        assert_eq!(report["pending_at_end"], 0);
    }

    #[test]
    fn missing_starts_child_frames_and_old_identities_cannot_forge_new_requests() {
        let mut trace = Trace::new(1);
        observe(&mut trace, &response("unknown", 200, 1.0));
        let mut child = start("child", "https://example.com/PRIVATE", 1.0);
        child["params"]["frameId"] = json!("child");
        observe(&mut trace, &child);
        observe(&mut trace, &start("1", "https://example.com/old", 2.0));
        observe(&mut trace, &start("2", "https://example.com/new", 3.0));
        observe(&mut trace, &response("1", 500, 4.0));
        observe(
            &mut trace,
            &start("1", "https://example.com/PRIVATE-reused", 4.0),
        );
        let report = trace.report();
        assert_eq!(report["requests"].as_array().unwrap().len(), 1);
        assert_eq!(report["requests"][0]["url"], "https://example.com/new");
        assert_eq!(report["dropped_requests"], 1);
        assert_eq!(report["pending_at_end"], 1);
        assert!(!report.to_string().contains("PRIVATE"));
    }

    #[test]
    fn backwards_time_and_duplicate_starts_are_not_successful_completions() {
        let mut trace = Trace::new(100);
        observe(&mut trace, &start("1", "https://example.com/api", 10.0));
        observe(&mut trace, &response("1", 200, 9.0));
        observe(&mut trace, &start("1", "https://example.com/rebound", 11.0));
        observe(&mut trace, &response("1", 200, 12.0));
        let report = trace.report();
        assert_eq!(report["requests"].as_array().unwrap().len(), 1);
        assert_eq!(report["requests"][0]["state"], "correlation_lost");
        assert_eq!(report["invalid_events"], 2);
    }

    #[test]
    fn retained_bytes_and_lifetime_identities_have_independent_bounds() {
        let mut trace = Trace::new(200);
        let long_url = format!("https://example.com/{}", "a".repeat(3000));
        for id in 0..MAX_IDENTITIES + 3 {
            observe(&mut trace, &start(&id.to_string(), &long_url, 1.0));
        }
        assert_eq!(trace.seen.len(), MAX_IDENTITIES);
        assert!(trace.hops.len() < 200);
        let report = trace.report();
        assert_eq!(report["admission_stopped"], true);
        assert_eq!(report["truncated"], true);
        assert!(report.to_string().len() < MAX_BYTES + 4096);
    }

    #[test]
    fn arbitrary_failure_payloads_and_response_headers_are_not_report_fields() {
        let mut trace = Trace::new(100);
        observe(&mut trace, &start("1", "https://example.com/api", 1.0));
        observe(
            &mut trace,
            &json!({"method":"Network.loadingFailed","params":{
                "requestId":"1","timestamp":2.0,"errorText":"failed https://example.com?PRIVATE",
                "corsErrorStatus":{"corsError":"InvalidResponse","failedParameter":"PRIVATE"}
            }}),
        );
        let report = trace.report();
        assert_eq!(report["requests"][0]["error_code"], Value::Null);
        assert_eq!(report["requests"][0]["cors_error"], "InvalidResponse");
        assert!(!report.to_string().contains("PRIVATE"));
    }

    #[test]
    fn unrelated_frame_redirect_cannot_retire_an_admitted_request() {
        let mut trace = Trace::new(100);
        observe(&mut trace, &start("1", "https://example.com/api", 1.0));
        let mut child = start("1", "https://example.com/child", 2.0);
        child["params"]["frameId"] = json!("child");
        child["params"]["redirectResponse"] = json!({"url":"https://example.com/api","status":302});
        observe(&mut trace, &child);
        observe(&mut trace, &response("1", 200, 3.0));
        let report = trace.report();
        assert_eq!(report["requests"].as_array().unwrap().len(), 1);
        assert_eq!(report["requests"][0]["state"], "responded");
    }

    #[test]
    fn response_url_mismatch_preserves_prior_request_facts() {
        let mut trace = Trace::new(100);
        observe(&mut trace, &start("1", "https://example.com/api", 1.0));
        let mut unrelated = response("1", 200, 2.0);
        unrelated["params"]["response"]["url"] = json!("https://example.com/other");
        observe(&mut trace, &unrelated);
        let report = trace.report();
        assert_eq!(report["invalid_events"], 1);
        assert_eq!(report["requests"][0]["state"], "pending");
        assert!(report["requests"][0].get("response").is_none());
    }
}
