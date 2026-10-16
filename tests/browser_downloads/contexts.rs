//! Public browser-context capture and recovery using the existing download peer.
//! Real loopback transport and files, not Chromium and not a production mock.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Named,
    ReportedDefault,
    ProbedDefault,
    Unknown,
    Null,
    Empty,
    InventoryFailure,
    DuplicateInventory,
    ChangedBeforeInput,
    ChangedBeforePublication,
    ProbeCreateFailure,
    ProbeInspectFailure,
    ProbeCloseFailure,
    NamedDenyFailure,
    WithheldReset,
}

impl Case {
    const fn named(self) -> bool {
        matches!(self, Self::Named | Self::InventoryFailure | Self::DuplicateInventory
            | Self::ChangedBeforeInput | Self::ChangedBeforePublication
            | Self::NamedDenyFailure | Self::WithheldReset)
    }

    fn identity(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Empty => json!(""),
            Self::Unknown => json!("unmanaged-profile"),
            _ if self.named() => json!("private"),
            _ => json!("ordinary"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery { None, Restored, DenyFailure, InventoryFailure, Disposed }

#[derive(Debug)]
pub(super) struct Peer {
    case: Case,
    recovery: Recovery,
    methods: Vec<String>,
    policies: Vec<(Option<String>, String)>,
    cancellations: Vec<(Option<String>, String)>,
    created: usize,
    closed: usize,
    inspected: usize,
    attached: usize,
    withheld: bool,
}

impl Peer {
    fn new(case: Case, recovery: Recovery) -> Self {
        Self {
            case, recovery, methods: Vec::new(), policies: Vec::new(),
            cancellations: Vec::new(), created: 0, closed: 0, inspected: 0,
            attached: 0, withheld: false,
        }
    }

    pub(super) const fn withholding(&self) -> bool { self.withheld }

    /// Return a contextual root reply, or let the existing renderer/download
    /// fixture handle the request. No duplicate DOM or input implementation.
    pub(super) fn reply(
        &mut self,
        request: &Value,
        observed: &Observed,
        socket: &mut TcpStream,
    ) -> Option<(Value, bool)> {
        let method = request["method"].as_str().unwrap();
        let params = &request["params"];
        self.methods.push(method.to_owned());
        if method.starts_with("Browser.") || method.starts_with("Target.") {
            if method != "Target.setAutoAttach" {
                assert!(request.get("sessionId").is_none(), "context commands are browser-scoped");
            }
        }
        match method {
            "Browser.setDownloadBehavior" => {
                let context = params.get("browserContextId").map(|value| value.as_str().unwrap().to_owned());
                let behavior = params["behavior"].as_str().unwrap();
                self.policies.push((context.clone(), behavior.to_owned()));
                if self.recovery != Recovery::None {
                    assert_eq!(behavior, "deny", "recovery cannot reopen capture");
                    assert!(self.attached == 0, "recover before selecting any page");
                    if let Some(context) = &context {
                        assert_eq!(context, "private", "recover the original context, not the next tab");
                        assert_ne!(self.recovery, Recovery::Disposed);
                        if self.recovery == Recovery::DenyFailure { return Some((json!({}), true)); }
                    }
                } else if self.policies.len() == 1 {
                    assert_eq!(context, None);
                    assert_eq!(behavior, "deny");
                } else {
                    let expected = self.case.named().then(|| "private".to_owned());
                    assert_eq!(context, expected, "never route named capture to the default or sibling");
                    if self.case == Case::NamedDenyFailure && observed.directory.is_none() {
                        assert_eq!(behavior, "deny");
                        return Some((json!({}), true));
                    }
                    if self.case == Case::WithheldReset && observed.directory.is_some() && behavior == "deny" {
                        // The public outer deadline must close this connection.
                        // No acknowledgement is returned and no cleanup replay
                        // is granted a fresh timeout on this socket.
                        self.withheld = true;
                    }
                }
                None
            }
            "Browser.cancelDownload" => {
                let context = params.get("browserContextId").map(|value| value.as_str().unwrap().to_owned());
                assert_eq!(context, self.case.named().then(|| "private".to_owned()));
                self.cancellations.push((context, params["guid"].as_str().unwrap().to_owned()));
                None
            }
            "Target.getTargets" => {
                if self.recovery != Recovery::None {
                    assert!(matches!(self.recovery, Recovery::Restored | Recovery::Disposed));
                    let expected = if self.recovery == Recovery::Disposed {
                        vec![(None, "deny".to_owned())]
                    } else {
                        vec![(Some("private".to_owned()), "deny".to_owned()), (None, "deny".to_owned())]
                    };
                    assert_eq!(self.policies, expected, "old policy must settle before listing another tab");
                }
                write_frame(socket, &start("existing", "unrelated-frame"));
                Some((json!({"targetInfos":[
                    {"type":"page","targetId":"page-1","url":"https://example.com/root","browserContextId":self.case.identity()},
                    {"type":"page","targetId":"other-page","url":"https://example.com/root","browserContextId":"sibling"}
                ]}), false))
            }
            "Target.getBrowserContexts" => {
                assert_eq!(params, &json!({}));
                if self.case == Case::InventoryFailure || self.recovery == Recovery::InventoryFailure {
                    return Some((json!({}), true));
                }
                let mut result = if self.recovery == Recovery::Disposed {
                    json!({"browserContextIds":["sibling"]})
                } else if self.case == Case::DuplicateInventory {
                    json!({"browserContextIds":["private","private"]})
                } else {
                    json!({"browserContextIds":["private","sibling"]})
                };
                if self.case == Case::ReportedDefault {
                    result["defaultBrowserContextId"] = json!("ordinary");
                }
                Some((result, false))
            }
            "Target.createTarget" => {
                assert_eq!(params, &json!({"url":"about:blank","hidden":true,"background":true}));
                self.created += 1;
                assert_eq!(self.created, 1, "only one bounded default identity probe");
                Some((json!({"targetId":"identity-probe"}), self.case == Case::ProbeCreateFailure))
            }
            "Target.getTargetInfo" => {
                self.inspected += 1;
                let target = params["targetId"].as_str().unwrap();
                if target == "identity-probe" {
                    assert_eq!(self.created, 1);
                    return Some((json!({"targetInfo":{
                        "targetId":target,"type":"other","url":"about:blank","browserContextId":"ordinary"
                    }}), self.case == Case::ProbeInspectFailure));
                }
                assert_eq!(target, "page-1");
                let context = if self.case == Case::ChangedBeforeInput
                    || (self.case == Case::ChangedBeforePublication && observed.reset)
                { json!("PRIVATE-CANARY") } else { self.case.identity() };
                Some((json!({"targetInfo":{
                    "targetId":target,"type":"page","url":"https://example.com/root","browserContextId":context
                }}), false))
            }
            "Target.closeTarget" => {
                assert_eq!(params, &json!({"targetId":"identity-probe"}), "never close a user tab");
                self.closed += 1;
                assert_eq!((self.created, self.closed), (1, 1));
                Some((json!({"success":self.case != Case::ProbeCloseFailure}), false))
            }
            "Target.attachToTarget" => {
                assert_eq!(self.recovery, Recovery::None, "list-only recovery must not attach");
                self.attached += 1;
                if self.case.named() {
                    assert!(self.policies.contains(&(Some("private".into()), "deny".into())));
                }
                None
            }
            _ => None,
        }
    }
}

struct Outcome {
    results: Vec<Result<ToolOutput>>,
    rounds: Vec<(Observed, Peer)>,
    bytes: Option<Vec<u8>>,
    endpoint_error: Option<String>,
}

fn exercise(
    scene: Scene,
    fault: Fault,
    case: Case,
    recoveries: &[Recovery],
    switch_endpoint: bool,
) -> Outcome {
    eprintln!("{}", json!({"suite":"browser_download_contexts","case":format!("{case:?}"),
        "scene":format!("{scene:?}"),"fault":format!("{fault:?}"),"phase":"start"}));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("result.bin");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let plan: Vec<_> = std::iter::once(Recovery::None).chain(recoveries.iter().copied()).collect();
    let peer_output = output.clone();
    let handle = thread::spawn(move || {
        let mut rounds = Vec::new();
        for recovery in plan {
            let mut socket = open_connection(&listener, address);
            let mut context = Peer::new(case, recovery);
            let fault = if recovery == Recovery::None { fault } else { Fault::None };
            let observed = serve_with_context(&mut socket, scene, fault, &peer_output, Some(&mut context));
            rounds.push((observed, context));
        }
        rounds
    });
    let mut tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_cdp_endpoint(format!("http://{address}"))
        .with_domain_allowlist(Some(vec!["example.com".to_owned()]));
    let timeout = if case == Case::WithheldReset { 1000 } else { 10000 };
    let mut args = json!({"action":"download","tab":"page-1","selector":"#export",
        "output_path":"result.bin","timeout_ms":timeout});
    if !matches!(scene, Scene::Main) { args["frame"] = json!(scene.frame()); }
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let mut results = vec![runtime.block_on(tool.execute("context-download", args, None))];
    for _ in recoveries {
        results.push(runtime.block_on(tool.execute("context-recovery", json!({
            "action":"list_tabs","tab":"other-page","timeout_ms":10000,
        }), None)));
    }
    let endpoint_error = if switch_endpoint {
        tool = tool.with_cdp_endpoint("http://127.0.0.1:9");
        Some(runtime.block_on(tool.execute("wrong-recovery-endpoint", json!({
            "action":"list_tabs","timeout_ms":1000,
        }), None)).unwrap_err().to_string())
    } else { None };
    drop(tool);
    let rounds = handle.join().unwrap();
    let bytes = match std::fs::read(output) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("context output read failed: {error}"),
    };
    for (observed, _) in &rounds {
        if let Some(path) = &observed.directory { assert!(!path.exists(), "private staging leaked"); }
    }
    eprintln!("{}", json!({"suite":"browser_download_contexts","case":format!("{case:?}"),
        "phase":"done","rounds":rounds.len(),"published":bytes.is_some()}));
    Outcome { results, rounds, bytes, endpoint_error }
}

#[test]
fn named_context_downloads_keep_bytes_and_nested_frame_input_in_the_selected_profile() {
    for scene in [Scene::Main, Scene::Local, Scene::Remote, Scene::Inside, Scene::Nested] {
        for fault in [Fault::None, Fault::Data] {
            let outcome = exercise(scene, fault, Case::Named, &[], false);
            let result = outcome.results[0].as_ref().unwrap();
            assert!(!result.is_error);
            let details = result.details.as_ref().unwrap();
            assert_eq!(details["browser_context"], json!({"kind":"named","id":"private"}));
            assert_eq!(details["frame_id"], scene.frame());
            assert_eq!(outcome.bytes.as_deref(), Some(PAYLOAD));
            let (observed, peer) = &outcome.rounds[0];
            assert_eq!((observed.presses, observed.releases, observed.queries), (1, 1, 1));
            assert_eq!(peer.policies, vec![(None,"deny".into()),
                (Some("private".into()),"deny".into()),
                (Some("private".into()),"allowAndName".into()),
                (Some("private".into()),"deny".into())]);
            assert!(peer.cancellations.is_empty());
            assert_eq!((peer.created, peer.closed), (0, 0));
        }
    }
}

#[test]
fn reported_and_probed_default_profile_ids_are_never_passed_as_named_contexts() {
    for case in [Case::ReportedDefault, Case::ProbedDefault] {
        for scene in [Scene::Main, Scene::Local, Scene::Remote, Scene::Inside, Scene::Nested] {
            let outcome = exercise(scene, Fault::None, case, &[], false);
            let result = outcome.results[0].as_ref().unwrap();
            assert_eq!(result.details.as_ref().unwrap()["browser_context"], json!({"kind":"default"}));
            assert_eq!(outcome.bytes.as_deref(), Some(PAYLOAD));
            let (observed, peer) = &outcome.rounds[0];
            assert_eq!(observed.policies, ["deny", "allowAndName", "deny"]);
            assert!(peer.policies.iter().all(|(context, _)| context.is_none()));
            let probes = usize::from(case == Case::ProbedDefault);
            assert_eq!((peer.created, peer.closed), (probes, probes));
        }
    }
}

#[test]
fn malformed_unknown_or_unverified_contexts_fail_before_attachment_and_input() {
    for case in [Case::Unknown, Case::Null, Case::Empty, Case::InventoryFailure,
        Case::DuplicateInventory, Case::ChangedBeforeInput, Case::ProbeCreateFailure,
        Case::ProbeInspectFailure, Case::ProbeCloseFailure]
    {
        let outcome = exercise(Scene::Main, Fault::None, case, &[], false);
        let failure = outcome.results[0].as_ref().unwrap_err().to_string();
        assert!(failure.contains("BROWSER_CONTEXT"), "{case:?}: {failure}");
        assert!(!failure.contains("PRIVATE-CANARY"));
        assert!(outcome.bytes.is_none());
        let (observed, peer) = &outcome.rounds[0];
        assert_eq!((observed.presses, observed.releases, peer.attached), (0, 0, 0));
        assert_eq!(observed.policies, ["deny"]);
        if matches!(case, Case::ProbeInspectFailure | Case::ProbeCloseFailure | Case::Unknown) {
            assert_eq!((peer.created, peer.closed), (1, 1));
        }
    }
}

#[test]
fn context_changes_before_publication_cancel_only_in_the_original_profile() {
    let outcome = exercise(Scene::Nested, Fault::None, Case::ChangedBeforePublication, &[], false);
    let failure = outcome.results[0].as_ref().unwrap_err().to_string();
    assert!(failure.contains("BROWSER_CONTEXT_CHANGED"));
    assert!(!failure.contains("PRIVATE-CANARY"));
    assert!(outcome.bytes.is_none());
    let (observed, peer) = &outcome.rounds[0];
    assert_eq!((observed.presses, observed.releases), (1, 1));
    assert_eq!(peer.cancellations, vec![(Some("private".into()), "job".into())]);
}

#[test]
fn named_policy_errors_do_not_fall_back_to_default_capture_or_cancel() {
    let denied = exercise(Scene::Main, Fault::None, Case::NamedDenyFailure, &[], false);
    assert!(denied.results[0].is_err());
    assert_eq!(denied.rounds[0].1.attached, 0);
    assert_eq!(denied.rounds[0].0.presses, 0);
    assert!(denied.bytes.is_none());
    for fault in [Fault::EnableFailure, Fault::ResetFailure, Fault::ForeignFrame] {
        let outcome = exercise(Scene::Main, fault, Case::Named, &[], false);
        assert!(outcome.results[0].is_err());
        assert!(outcome.bytes.is_none());
        let (_, peer) = &outcome.rounds[0];
        assert!(peer.policies.iter().skip(1).all(|(context, _)| context.as_deref() == Some("private")));
        assert!(peer.cancellations.iter().all(|(context, _)| context.as_deref() == Some("private")));
    }
}

#[test]
fn failed_capture_recovers_its_original_context_before_another_tab_can_run() {
    let outcome = exercise(Scene::Main, Fault::ResetFailure, Case::Named, &[Recovery::Restored], false);
    assert!(outcome.results[0].is_err());
    assert!(!outcome.results[1].as_ref().unwrap().is_error);
    assert!(outcome.bytes.is_none());
    assert_eq!((outcome.rounds[0].0.presses, outcome.rounds[0].0.releases), (1, 1));
    let (observed, peer) = &outcome.rounds[1];
    assert_eq!(peer.methods.first().map(String::as_str), Some("Target.getBrowserContexts"));
    assert_eq!(observed.presses, 0);
    assert_eq!(peer.attached, 0);
    assert_eq!(peer.policies, vec![(Some("private".into()), "deny".into()), (None, "deny".into())]);
}

#[test]
fn failed_recovery_retains_the_pending_context_and_blocks_all_page_work() {
    for failure in [Recovery::DenyFailure, Recovery::InventoryFailure] {
        let outcome = exercise(Scene::Main, Fault::ResetFailure, Case::Named,
            &[failure, Recovery::Restored], false);
        assert!(outcome.results[0].is_err());
        assert!(outcome.results[1].is_err());
        assert!(!outcome.results[2].as_ref().unwrap().is_error);
        assert!(outcome.bytes.is_none());
        let (_, failed) = &outcome.rounds[1];
        assert!(!failed.methods.iter().any(|method| method == "Target.getTargets"));
        assert!(!failed.policies.iter().any(|(context, _)| context.is_none()));
        assert_eq!(outcome.rounds[2].1.policies[0], (Some("private".into()), "deny".into()));
    }
}

#[test]
fn disposed_pending_contexts_do_not_redirect_their_recovery_to_another_named_context() {
    let outcome = exercise(Scene::Main, Fault::ResetFailure, Case::Named, &[Recovery::Disposed], false);
    assert!(outcome.results[0].is_err());
    assert!(!outcome.results[1].as_ref().unwrap().is_error);
    assert!(outcome.bytes.is_none());
    assert_eq!(outcome.rounds[1].1.policies, vec![(None, "deny".into())]);
    assert!(outcome.rounds[1].1.cancellations.is_empty());
}

#[test]
fn an_outer_deadline_during_reset_preserves_context_for_the_next_connection() {
    let outcome = exercise(Scene::Main, Fault::None, Case::WithheldReset, &[Recovery::Restored], false);
    let failure = outcome.results[0].as_ref().unwrap_err().to_string();
    assert!(failure.contains("timed out"), "{failure}");
    assert!(!outcome.results[1].as_ref().unwrap().is_error);
    assert!(outcome.bytes.is_none());
    assert!(outcome.rounds[0].1.withheld);
    assert_eq!((outcome.rounds[0].0.presses, outcome.rounds[0].0.releases), (1, 1));
    assert_eq!(outcome.rounds[1].0.presses, 0);
    assert_eq!(outcome.rounds[1].1.policies[0], (Some("private".into()), "deny".into()));
}

#[test]
fn pending_context_recovery_refuses_endpoint_switching_before_a_new_connection() {
    let outcome = exercise(Scene::Main, Fault::ResetFailure, Case::Named, &[], true);
    assert!(outcome.results[0].is_err());
    assert!(outcome.endpoint_error.unwrap().contains("BROWSER_CONTEXT_RECOVERY_ENDPOINT"));
    assert!(outcome.bytes.is_none());
    assert_eq!(outcome.rounds.len(), 1);
}
