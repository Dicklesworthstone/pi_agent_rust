use super::*;

fn capabilities() -> UpdateCapabilities {
    UpdateCapabilities {
        subscribe: true,
        resources_changed: true,
        prompts_changed: true,
    }
}

fn notification(method: &str, uri: &str) -> Value {
    json!({"jsonrpc":"2.0", "method":method, "params":{"uri":uri}})
}

fn updated(uri: &str) -> Value {
    notification("notifications/resources/updated", uri)
}

fn connected() -> (Arc<ContextUpdates>, ContextUpdateConnection) {
    let inbox = Arc::new(ContextUpdates::default());
    let connection = inbox.connect().unwrap();
    connection.initialize(0, capabilities()).unwrap();
    (inbox, connection)
}

#[test]
fn early_updates_are_retained_until_acknowledgement_and_rolled_back_with_mutations() {
    let (_inbox, connection) = connected();
    let uri = "opaque://a/%2F?q=hello+world#part";
    let subscribe = connection.begin(0, uri, true).unwrap().unwrap();
    connection.publish(&updated(uri), 0);
    connection.publish(&updated("opaque://a//?q=hello world#part"), 0);
    assert_eq!(
        connection.poll(0, true).unwrap()["updated_resources"],
        json!([])
    );
    assert!(connection.begin(0, uri, true).is_err());
    assert!(connection.begin(0, uri, false).is_err());
    subscribe.commit().unwrap();
    let first = connection.poll(0, true).unwrap();
    assert_eq!(first["subscriptions"], json!([uri]));
    assert_eq!(first["updated_resources"], json!([uri]));
    assert!(connection.begin(0, uri, true).unwrap().is_none());
    assert_eq!(
        connection.poll(0, true).unwrap()["updated_resources"],
        json!([])
    );

    let unsubscribe = connection.begin(0, uri, false).unwrap().unwrap();
    connection.publish(&updated(uri), 0);
    drop(unsubscribe);
    assert_eq!(
        connection.poll(0, true).unwrap()["updated_resources"],
        json!([uri])
    );
    connection
        .begin(0, uri, false)
        .unwrap()
        .unwrap()
        .commit()
        .unwrap();
    connection.publish(&updated(uri), 0);
    assert_eq!(
        connection.poll(0, true).unwrap()["subscriptions"],
        json!([])
    );
    assert!(connection.begin(0, uri, false).unwrap().is_none());

    let failed = connection.begin(0, uri, true).unwrap().unwrap();
    connection.publish(&updated(uri), 0);
    drop(failed);
    assert_eq!(
        connection.poll(0, true).unwrap()["updated_resources"],
        json!([])
    );
}

#[test]
fn capability_gates_and_notification_envelopes_are_enforced() {
    let inbox = Arc::new(ContextUpdates::default());
    let connection = inbox.connect().unwrap();
    connection
        .initialize(0, UpdateCapabilities::default())
        .unwrap();
    assert!(connection.begin(0, "urn:example", true).is_err());
    for method in [
        "notifications/resources/list_changed",
        "notifications/prompts/list_changed",
    ] {
        connection.publish(&notification(method, "unused"), 0);
    }
    let empty = connection.poll(0, true).unwrap();
    assert_eq!(empty["resources_list_changed"], false);
    assert_eq!(empty["prompts_list_changed"], false);

    let (_inbox, connection) = connected();
    connection
        .begin(0, "urn:example", true)
        .unwrap()
        .unwrap()
        .commit()
        .unwrap();
    for id in [json!(123), json!("request"), Value::Null] {
        let mut invalid = updated("urn:example");
        invalid["id"] = id;
        connection.publish(&invalid, 0);
    }
    let mut invalid = updated("urn:example");
    invalid["result"] = json!({});
    connection.publish(&invalid, 0);
    invalid.as_object_mut().unwrap().remove("result");
    invalid["jsonrpc"] = json!("1.0");
    connection.publish(&invalid, 0);
    connection.publish(&updated("unsolicited://different"), 0);
    connection.publish(&updated("urn:example"), 9);
    connection.publish(
        &json!({"jsonrpc":"2.0", "method":"notifications/resources/list_changed", "params":[]}),
        0,
    );
    let ignored = connection.poll(0, true).unwrap();
    assert_eq!(ignored["updated_resources"], json!([]));
    assert_eq!(ignored["resources_list_changed"], false);
    for _ in 0..1000 {
        connection.publish(&updated("urn:example"), 0);
        connection.publish(&notification("notifications/resources/list_changed", ""), 0);
        connection.publish(&notification("notifications/prompts/list_changed", ""), 0);
    }
    let changes = connection.poll(0, true).unwrap();
    assert_eq!(changes["updated_resources"], json!(["urn:example"]));
    assert_eq!(changes["resources_list_changed"], true);
    assert_eq!(changes["prompts_list_changed"], true);
    let drained = connection.poll(0, true).unwrap();
    assert_eq!(drained["resources_list_changed"], false);
    assert_eq!(drained["prompts_list_changed"], false);
}

#[test]
fn connection_and_generation_changes_expire_watches_without_resurrection() {
    let (inbox, first) = connected();
    first
        .begin(0, "urn:first", true)
        .unwrap()
        .unwrap()
        .commit()
        .unwrap();
    let pending = first.begin(0, "urn:pending", true).unwrap().unwrap();
    first.retire(0);
    assert!(pending.commit().is_err());
    assert!(first.initialize(0, capabilities()).is_err());
    let second = inbox.connect().unwrap();
    second.initialize(0, capabilities()).unwrap();
    second
        .begin(0, "urn:second", true)
        .unwrap()
        .unwrap()
        .commit()
        .unwrap();
    first.publish(&updated("urn:second"), 0);
    first.retire(0);
    assert!(
        first.poll(0, false).is_err(),
        "old readers cannot consume replacement notices"
    );
    let reconnected = second.poll(0, true).unwrap();
    assert_eq!(reconnected["expired_subscriptions"], json!(["urn:first"]));
    assert_eq!(reconnected["subscriptions"], json!(["urn:second"]));
    assert_eq!(reconnected["updated_resources"], json!([]));

    second.initialize(2, capabilities()).unwrap();
    second.publish(&updated("urn:second"), 0);
    assert!(second.poll(0, true).is_err());
    let renewed = second.poll(2, true).unwrap();
    assert_eq!(renewed["expired_subscriptions"], json!(["urn:second"]));
    assert_eq!(renewed["subscriptions"], json!([]));
    second
        .begin(2, "urn:last", true)
        .unwrap()
        .unwrap()
        .commit()
        .unwrap();
    let dead = second.poll(2, false).unwrap();
    assert_eq!(dead["connected"], false);
    assert_eq!(dead["expired_subscriptions"], json!(["urn:last"]));
    assert!(second.initialize(2, capabilities()).is_err());
}

#[test]
fn old_http_poll_cannot_drain_a_later_retired_generation() {
    let (_inbox, connection) = connected();
    connection.retire(0);
    connection.initialize(1, capabilities()).unwrap();
    connection
        .begin(1, "urn:newer", true)
        .unwrap()
        .unwrap()
        .commit()
        .unwrap();
    connection.retire(1);
    assert!(connection.poll(0, false).is_err());
    assert_eq!(
        connection.poll(2, false).unwrap()["expired_subscriptions"],
        json!(["urn:newer"])
    );
}

#[test]
fn counts_bytes_and_expiry_backlog_remain_bounded() {
    let (inbox, connection) = connected();
    for index in 0..MAX_SUBSCRIPTIONS {
        connection
            .begin(0, &format!("urn:{index}"), true)
            .unwrap()
            .unwrap()
            .commit()
            .unwrap();
    }
    assert!(connection.begin(0, "urn:overflow", true).is_err());
    connection.retire(0);
    let next = inbox.connect().unwrap();
    next.initialize(0, capabilities()).unwrap();
    next.begin(0, "urn:extra", true)
        .unwrap()
        .unwrap()
        .commit()
        .unwrap();
    next.retire(0);
    let expired = inbox.poll_disconnected();
    assert_eq!(
        expired["expired_subscriptions"].as_array().unwrap().len(),
        MAX_SUBSCRIPTIONS
    );
    assert_eq!(expired["expired_overflow"], true);
    assert_eq!(inbox.poll_disconnected()["expired_overflow"], false);

    let (_inbox, connection) = connected();
    for index in 0..4 {
        let uri = format!("{index}{}", "x".repeat(MAX_URI_BYTES - 1));
        connection
            .begin(0, &uri, true)
            .unwrap()
            .unwrap()
            .commit()
            .unwrap();
    }
    assert!(connection.begin(0, "urn:small", true).is_err());
    for uri in [
        String::new(),
        "a\nb".to_string(),
        "x".repeat(MAX_URI_BYTES + 1),
    ] {
        assert!(connection.begin(0, &uri, true).is_err());
    }
}

#[test]
fn clearing_revoked_inbox_fences_private_handshakes_and_discards_identifiers() {
    let (inbox, connection) = connected();
    connection
        .begin(0, "private://document", true)
        .unwrap()
        .unwrap()
        .commit()
        .unwrap();
    connection.publish(&updated("private://document"), 0);
    inbox.clear();
    let cleared = inbox.poll_disconnected();
    assert_eq!(cleared["expired_subscriptions"], json!([]));
    assert_eq!(cleared["subscriptions"], json!([]));
    assert!(connection.initialize(0, capabilities()).is_err());
    let private = inbox.connect().unwrap();
    inbox.clear();
    assert!(private.initialize(0, capabilities()).is_err());
    let replacement = inbox.connect().unwrap();
    replacement.initialize(0, capabilities()).unwrap();
}

#[test]
fn malformed_advertised_context_capabilities_are_rejected() {
    for caps in [
        json!({"resources":true}),
        json!({"resources":{"subscribe":"yes"}}),
        json!({"resources":{"listChanged":null}}),
        json!({"prompts":[]}),
        json!({"prompts":{"listChanged":1}}),
    ] {
        assert!(UpdateCapabilities::from_initialize(&json!({"capabilities":caps})).is_err());
    }
    assert!(UpdateCapabilities::from_initialize(&json!({"capabilities":{}})).is_ok());
}

mod native_peer;
