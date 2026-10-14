use super::*;
use crate::mcp::progress::ProgressSubscription;
use serde_json::json;
use std::task::{Context, Poll, Waker};

fn subscribe(transport: &HttpTransport) -> ProgressSubscription {
    let dispatch = std::sync::Arc::new(RequestDispatch::new());
    assert!(dispatch.claim());
    transport.progress.subscribe(dispatch, transport.session_generation()).unwrap()
}

#[test]
fn http_notifications_are_correlated_to_the_receiving_session_generation() {
    let transport = HttpTransport::new("http://127.0.0.1:1/mcp", Vec::new()).unwrap();
    let old_wire = transport.wire_state();
    transport.reset_session_state();
    let mut subscription = subscribe(&transport);
    let frame = json!({"jsonrpc":"2.0","method":"notifications/progress",
        "params":{"progressToken":subscription.token(),"progress":1}});
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        // This handler is shared by POST SSE and the optional GET stream.
        assert!(transport.handle_sse_message(frame.clone(), 0, &old_wire).await.unwrap().is_none());
        assert!(subscription.poll_update(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(transport.handle_sse_message(frame, 0, &transport.wire_state()).await.unwrap().is_none());
        assert!(matches!(subscription.poll_update(&mut Context::from_waker(Waker::noop())), Poll::Ready(Some(_))));
    });
}

#[test]
fn server_request_is_not_progress_and_final_response_stays_authoritative() {
    let transport = HttpTransport::new("http://127.0.0.1:1/mcp", Vec::new()).unwrap();
    let mut subscription = subscribe(&transport);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let response = json!({"jsonrpc":"2.0","id":7,"result":{"done":true}});
        assert_eq!(transport.handle_sse_message(response, 7, &transport.wire_state()).await.unwrap(), Some(json!({"done":true})));
        assert!(subscription.poll_update(&mut Context::from_waker(Waker::noop())).is_pending());
    });
    let mut disguised = json!({"jsonrpc":"2.0","method":"notifications/progress",
        "params":{"progressToken":subscription.token(),"progress":10}});
    disguised["id"] = json!(7);
    transport.progress.publish(&disguised, transport.session_generation());
    assert!(subscription.poll_update(&mut Context::from_waker(Waker::noop())).is_pending());
}
