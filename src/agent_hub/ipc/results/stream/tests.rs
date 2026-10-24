use super::super::{
    AgentCx, MAX_DOCUMENT_BYTES, MAX_RESPONSE_BYTES, live_identity, read_deadline, resolve_at,
};
use super::*;
use serde_json::json;

fn document(content: &str) -> Document {
    Document {
        reference: "peer-1/value".into(),
        content: content.to_string(),
        content_type: "text/plain".into(),
        metadata: json!({"scope":"parentChannel"}),
    }
}

fn source() -> String {
    format!("alpha\nbeta\n{}", "雪🙂x".repeat(20_000))
}

fn pages(content: &str) -> Vec<WireResponse> {
    let mut pages = Pages::new(document(content)).unwrap();
    let mut replies = Vec::new();
    while let Some(reply) = pages.next() {
        replies.push(reply);
    }
    replies
}

fn wire(replies: Vec<WireResponse>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for reply in replies {
        let mut frame = serde_json::to_vec(&reply).unwrap();
        frame.push(b'\n');
        assert!(frame.len() <= MAX_RESPONSE_BYTES);
        bytes.extend(frame);
    }
    bytes
}

fn page(reply: WireResponse) -> Page {
    match reply {
        WireResponse::Page(page) => page,
        _ => panic!("expected page"),
    }
}

#[test]
fn small_results_keep_the_complete_frame_and_large_results_reconstruct_exactly() {
    for content in ["small\n雪".to_string(), source()] {
        let messages = pages(&content);
        assert_eq!(messages.len() == 1, content.len() < MAX_COMPLETE_BYTES);
        let count = messages.len();
        let mut collector = Collector::new("peer-1/value");
        for (index, response) in messages.into_iter().enumerate() {
            // Exercise the actual wire representation, not shared Rust objects.
            let frame = serde_json::to_vec(&response).unwrap();
            assert!(frame.len() < MAX_RESPONSE_BYTES);
            let response = serde_json::from_slice(&frame).unwrap();
            let result = collector.push(response).unwrap();
            if index + 1 == count {
                assert_eq!(result.unwrap().content, content);
            } else {
                assert!(result.is_none(), "partial data escaped as a document");
            }
        }
        assert!(collector.finished);
        assert!(collector.partial.is_none());
    }
}

#[test]
fn page_boundaries_never_split_utf8_or_create_empty_progress() {
    let content = format!(
        "{}🙂{}",
        "a".repeat(PAGE_BYTES - 1),
        "x".repeat(MAX_COMPLETE_BYTES)
    );
    let mut next_offset = 0;
    for (index, response) in pages(&content).into_iter().enumerate() {
        let page = page(response);
        assert_eq!(page.offset, next_offset);
        assert!(!page.content.is_empty() && page.content.len() <= PAGE_BYTES);
        assert_eq!(page.header.is_some(), index == 0);
        if index == 0 {
            assert_eq!(page.content.len(), PAGE_BYTES - 1);
        }
        next_offset += page.content.len();
        assert!(content.is_char_boundary(next_offset));
        assert_eq!(page.done, next_offset == content.len());
    }
    assert_eq!(next_offset, content.len());
}

#[test]
fn corrupted_payload_cannot_pass_final_fingerprint_or_be_rescued() {
    let mut messages = pages(&source()).into_iter();
    let mut first = page(messages.next().unwrap());
    first.content.replace_range(..1, "Z");
    let mut collector = Collector::new("peer-1/value");
    assert!(collector.push(WireResponse::Page(first)).unwrap().is_none());
    let mut failed = false;
    for message in messages {
        match collector.push(message) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("corrupt document accepted"),
            Err(error) => {
                assert!(error.to_string().contains("fingerprint"));
                failed = true;
            }
        }
    }
    assert!(failed && collector.finished && collector.partial.is_none());
    assert!(
        collector
            .push(WireResponse::Ok(document("replacement")))
            .is_err()
    );
}

#[test]
fn offsets_lengths_revisions_headers_and_early_completion_are_not_implicit_restarts() {
    for mutation in 0..6 {
        let mut messages = pages(&source()).into_iter();
        let first = messages.next().unwrap();
        let mut second = page(messages.next().unwrap());
        match mutation {
            0 => second.offset += 1,
            1 => second.total_bytes += 1,
            2 => second.fingerprint = "0".repeat(64),
            3 => {
                second.header = Some(Header {
                    content_type: "text/plain".into(),
                    metadata: json!({}),
                })
            }
            4 => second.done = true,
            _ => second.reference = "other-reader/value".into(),
        }
        let mut collector = Collector::new("peer-1/value");
        assert!(collector.push(first).unwrap().is_none());
        assert!(collector.push(WireResponse::Page(second)).is_err());
        assert!(collector.finished && collector.partial.is_none());
    }
}

#[test]
fn malformed_first_pages_and_unbounded_headers_fail_before_accumulation() {
    for mutation in 0..6 {
        let mut first = page(pages(&source()).remove(0));
        match mutation {
            0 => first.header = None,
            1 => first.offset = 1,
            2 => first.content.clear(),
            3 => first.total_bytes = MAX_DOCUMENT_BYTES + 1,
            4 => first.fingerprint.clear(),
            _ => {
                first.header.as_mut().unwrap().metadata =
                    json!({"oversized":"x".repeat(METADATA_BYTES)})
            }
        }
        let mut collector = Collector::new("peer-1/value");
        assert!(collector.push(WireResponse::Page(first)).is_err());
        assert!(collector.partial.is_none());
    }
    let mut doc = document("small");
    doc.metadata = json!({"error":"x".repeat(METADATA_BYTES)});
    assert!(Pages::new(doc).is_err());
}

#[test]
fn error_or_complete_frame_after_a_prefix_discards_the_partial_document() {
    for replacement in [
        WireResponse::Error("PI_HUB_IPC_REVOKED: reader retired".into()),
        WireResponse::Ok(document("do not substitute this result")),
    ] {
        let mut collector = Collector::new("peer-1/value");
        assert!(
            collector
                .push(pages(&source()).remove(0))
                .unwrap()
                .is_none()
        );
        assert!(collector.push(replacement).is_err());
        assert!(collector.finished && collector.partial.is_none());
    }
}

#[test]
fn actual_socket_stream_accepts_many_frames_but_not_an_unfinished_prefix() {
    let content = source();
    let encoded = wire(pages(&content));
    let (address, handle) = super::super::tests::serve(encoded);
    let owner = AgentCx::for_testing_with_io();
    let result = resolve_at(
        &owner,
        address,
        "a".repeat(64),
        "peer-1/value",
        read_deadline(&owner).unwrap(),
    );
    handle.join().unwrap();
    assert_eq!(result.unwrap().content, content);

    let encoded = wire(vec![pages(&source()).remove(0)]);
    let (address, handle) = super::super::tests::serve(encoded);
    let error = resolve_at(
        &owner,
        address,
        "a".repeat(64),
        "peer-1/value",
        read_deadline(&owner).unwrap(),
    )
    .unwrap_err();
    handle.join().unwrap();
    assert!(error.to_string().contains("PI_HUB_IPC_CLOSED"));
}

#[test]
fn transfer_revocation_rejects_later_pages_and_debug_never_contains_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut hub = crate::agent_hub::AgentHubRegistry::default();
    hub.set_dir_for_tests(dir.path().to_path_buf());
    let entry = hub
        .register("stream-reader", "stream ownership fixture")
        .unwrap();
    hub.mark_running(&entry.id, 42);
    let identity = live_identity(&hub, &entry.id).unwrap();
    let mut transfer = Transfer::new(document(&source()), &entry.id, identity).unwrap();
    assert!(!format!("{transfer:?}").contains("alpha"));
    assert!(
        transfer
            .next_with_identity(live_identity(&hub, &entry.id))
            .unwrap()
            .is_some()
    );
    hub.mark_process_reaped(&entry.id);
    let refused = transfer
        .next_with_identity(live_identity(&hub, &entry.id))
        .is_err();
    hub.settle(&entry.id, crate::agent_hub::ChildStatus::Cancelled);
    assert!(refused, "a reaped process consumed a later result page");
}

#[test]
fn a_completed_collector_rejects_trailing_frames_and_drops_no_required_data() {
    let mut collector = Collector::new("peer-1/value");
    assert_eq!(
        collector
            .push(WireResponse::Ok(document("done")))
            .unwrap()
            .unwrap()
            .content,
        "done"
    );
    assert!(
        collector
            .push(WireResponse::Ok(document("replacement")))
            .is_err()
    );
}
