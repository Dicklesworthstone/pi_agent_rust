//! Drive held native workspace requests over the real framed-stdio transport.
//! Peer commands only control wire messages; collection, routing, retry,
//! cancellation and final report validation are production implementations.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::agent_cx::AgentCx;
use crate::lsp::client::test_server::Fixture;

fn peer(root: &std::path::Path) -> Option<Fixture> {
    Fixture::connect(
        root,
        json!({"diagnosticProvider":{
            "interFileDependencies":true,"workspaceDiagnostics":true
        }}),
    )
}

fn report(uri: &str) -> Value {
    json!({"uri":uri,"version":null,"kind":"full","items":[{
        "message":"reported problem","severity":1,
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}
    }]})
}

async fn peer_call(peer: &Fixture, method: &str, params: Value) -> Value {
    // Bypass only the application lane, which belongs to the request under
    // test. The actual transport still sends and correlates every frame.
    let (id, response) = peer.client.rpc.request(method, params).unwrap();
    crate::lsp::jsonrpc::await_completion(response, Duration::from_secs(5), || {
        peer.client.rpc.cancel_request(id);
    })
    .await
    .expect("peer acknowledgement arrived")
    .expect("peer operation succeeded")
}

async fn workspace_requests(peer: &Fixture) -> Vec<Value> {
    peer_call(peer, "test/frames", Value::Null)
        .await
        .as_array()
        .unwrap()
        .iter()
        .filter(|frame| frame["method"] == "workspace/diagnostic")
        .cloned()
        .collect()
}

async fn notifications(peer: &Fixture, frames: Vec<Value>) {
    peer_call(peer, "test/notifications", json!({"notifications":frames})).await;
}

fn progress(request: &Value, value: &Value) -> Value {
    json!({"jsonrpc":"2.0","method":"$/progress","params":{
        "token":request["params"]["partialResultToken"],"value":value
    }})
}

fn completion(request: &Value, result: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":request["id"],"result":result})
}

fn assert_retired(peer: &Fixture) {
    assert!(
        LspClient::lock(&peer.client.partial_results)
            .active
            .is_none()
    );
    assert!(
        Arc::clone(&peer.client.request_lane)
            .try_lock_owned()
            .is_ok()
    );
    assert!(peer.client.is_alive());
}

#[test]
fn snapshot_polling_preserves_fragments_until_the_real_terminal_response() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    peer.configure(json!({"workspace/diagnostic":[{"hold":true}]}));
    peer.runtime.block_on(async {
        let mut pending = Box::pin(peer.client.workspace_diagnostics(Duration::from_secs(5)));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let requests = workspace_requests(&peer).await;
        assert_eq!(requests.len(), 1, "the peer observed actual dispatch");
        let request = &requests[0];
        let expected = report("untitled:from-progress");
        notifications(&peer, vec![
            progress(request, &json!({"items":[expected]})),
            json!({"jsonrpc":"2.0","method":"experimental/serverStatus","params":{"quiescent":true}}),
        ]).await;
        assert!(peer.client.diagnostics_snapshot().is_empty());
        assert!(peer.client.quiescent.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            LspClient::lock(&peer.client.partial_results).active.as_ref().unwrap().values.len(),
            1,
            "a snapshot reader routes rather than steals the fragment"
        );
        assert!(futures::poll!(pending.as_mut()).is_pending(), "progress is not completion");
        notifications(&peer, vec![completion(request, &Value::Null)]).await;
        assert_eq!(pending.await.unwrap(), vec![expected]);
        assert!(peer.client.diagnostics_snapshot().is_empty());
        assert_retired(&peer);
    });
}

#[test]
fn actual_queue_loss_refuses_an_already_buffered_success_and_allows_a_fresh_call() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    peer.configure(json!({"workspace/diagnostic":[{"hold":true}]}));
    peer.runtime.block_on(async {
        let mut pending = Box::pin(peer.client.workspace_diagnostics(Duration::from_secs(5)));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let requests = workspace_requests(&peer).await;
        let request = &requests[0];
        // No application poll runs while this entire batch is acknowledged.
        // Fill the actual production queue, lose one diagnostic fragment,
        // then buffer a success response that must NOT authorize empty output.
        let mut flood = vec![
            json!({"jsonrpc":"2.0","method":"window/logMessage",
            "params":{"type":3,"message":"analyzing"}});
            1024
        ];
        flood.push(progress(
            request,
            &json!({"items":[report("untitled:lost-error")]}),
        ));
        flood.push(completion(request, &json!({"items":[]})));
        notifications(&peer, flood).await;
        assert!(peer.client.dropped_notifications() > 0);
        let error = pending.await.unwrap_err();
        assert!(
            error.to_string().contains("notifications were lost"),
            "{error}"
        );
        assert_retired(&peer);
    });
    peer.configure(json!({"workspace/diagnostic":[{"result":{"items":[]}}]}));
    assert!(
        peer.runtime
            .block_on(peer.client.workspace_diagnostics(Duration::from_secs(5)))
            .unwrap()
            .is_empty()
    );
    assert_retired(&peer);
}

#[test]
fn dropping_a_written_partial_request_cancels_once_and_isolates_late_fragments() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    peer.configure(json!({"workspace/diagnostic":[{"hold":true},{"hold":true}]}));
    peer.runtime.block_on(async {
        let mut first = Box::pin(peer.client.workspace_diagnostics(Duration::from_secs(5)));
        assert!(futures::poll!(first.as_mut()).is_pending());
        let requests = workspace_requests(&peer).await;
        let old = requests[0].clone();
        notifications(
            &peer,
            vec![progress(&old, &json!({"items":[report("untitled:old")]}))],
        )
        .await;
        peer.client.poll_notifications();
        drop(first);
        assert_retired(&peer);

        let mut next = Box::pin(peer.client.workspace_diagnostics(Duration::from_secs(5)));
        assert!(futures::poll!(next.as_mut()).is_pending());
        let requests = workspace_requests(&peer).await;
        assert_eq!(requests.len(), 2);
        let new = &requests[1];
        assert_ne!(
            old["params"]["partialResultToken"],
            new["params"]["partialResultToken"]
        );
        let expected = report("untitled:new");
        notifications(
            &peer,
            vec![
                progress(&old, &json!({"items":[report("untitled:late-old")]})),
                completion(&old, &json!({"items":[]})),
                progress(new, &json!({"items":[expected]})),
                completion(new, &Value::Null),
            ],
        )
        .await;
        assert_eq!(next.await.unwrap(), vec![expected]);
        assert_retired(&peer);
    });
    let frames = peer.frames();
    let first = frames
        .iter()
        .position(|frame| frame["method"] == "workspace/diagnostic")
        .unwrap();
    let next = frames
        .iter()
        .rposition(|frame| frame["method"] == "workspace/diagnostic")
        .unwrap();
    let cancellations: Vec<_> = frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame["method"] == "$/cancelRequest")
        .collect();
    assert_eq!(cancellations.len(), 1);
    let (cancelled, frame) = cancellations[0];
    assert!(first < cancelled && cancelled < next);
    assert_eq!(frame["params"]["id"], frames[first]["id"]);
}

#[test]
fn server_retrigger_discards_the_previous_attempt_and_uses_a_new_token() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    peer.configure(json!({"workspace/diagnostic":[{"hold":true},{"hold":true}]}));
    peer.runtime.block_on(async {
        let mut pending = Box::pin(peer.client.workspace_diagnostics(Duration::from_secs(10)));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let requests = workspace_requests(&peer).await;
        let old = requests[0].clone();
        notifications(
            &peer,
            vec![
                progress(&old, &json!({"items":[report("untitled:failed-attempt")]})),
                json!({"jsonrpc":"2.0","id":old["id"],"error":{
                    "code":-32802,"message":"retry diagnostics","data":{"retriggerRequest":true}
                }}),
            ],
        )
        .await;
        let expected = report("untitled:retried-result");
        let responder = async {
            let started = Instant::now();
            let new = loop {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "retry was not dispatched"
                );
                let requests = workspace_requests(&peer).await;
                if let Some(new) = requests.get(1) {
                    break new.clone();
                }
                AgentCx::for_current_or_request()
                    .time()
                    .sleep(Duration::from_millis(10))
                    .await;
            };
            assert_ne!(
                old["params"]["partialResultToken"],
                new["params"]["partialResultToken"]
            );
            notifications(
                &peer,
                vec![
                    progress(
                        &old,
                        &json!({"items":[report("untitled:late-failed-attempt")]}),
                    ),
                    progress(&new, &json!({"items":[expected]})),
                    completion(&new, &json!({"items":[]})),
                ],
            )
            .await;
        };
        let (result, ()) = futures::join!(pending, responder);
        assert_eq!(result.unwrap(), vec![expected]);
        assert_eq!(workspace_requests(&peer).await.len(), 2);
        assert_retired(&peer);
    });
}

#[test]
fn owner_cancellation_after_progress_retires_the_collection_without_delivery() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    peer.configure(json!({"workspace/diagnostic":[{"hold":true}]}));
    peer.runtime.block_on(async {
        let owner = AgentCx::for_request();
        let mut pending =
            Box::pin(owner.with_current(peer.client.workspace_diagnostics(Duration::from_secs(5))));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let requests = workspace_requests(&peer).await;
        notifications(
            &peer,
            vec![progress(
                &requests[0],
                &json!({"items":[report("untitled:cancelled")]}),
            )],
        )
        .await;
        peer.client.poll_notifications();
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel partial pull"),
        );
        let error = pending.await.unwrap_err();
        assert!(error.to_string().contains("LSP_CANCELLED"), "{error}");
        assert_retired(&peer);
        assert_eq!(
            peer.client
                .call("test/success", Value::Null, Duration::from_secs(5))
                .await
                .unwrap(),
            json!({"ok":true})
        );
        assert!(peer.client.diagnostics_snapshot().is_empty());
    });
}

#[test]
fn terminal_response_cannot_exceed_the_fragments_remaining_byte_budget() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    peer.configure(json!({"workspace/diagnostic":[{"hold":true}]}));
    peer.runtime.block_on(async {
        let fragment = json!({"items":[]});
        let bytes = serde_json::to_vec(&fragment).unwrap().len();
        let budget = RequestBudget::new(Duration::from_secs(5));
        let mut pending = Box::pin(peer.client.call_with_partial_results(
            "workspace/diagnostic",
            json!({"previousResultIds":[]}),
            &budget,
            bytes,
        ));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let requests = workspace_requests(&peer).await;
        notifications(
            &peer,
            vec![
                progress(&requests[0], &fragment),
                completion(&requests[0], &Value::Null),
            ],
        )
        .await;
        let error = pending.await.unwrap_err();
        assert!(
            error.to_string().contains("cumulative byte limit"),
            "{error}"
        );
        assert_retired(&peer);
    });
}
