use pi::sdk;
use serde_json::json;
use std::path::PathBuf;

const fn assert_clone_debug_send_sync<T: Clone + std::fmt::Debug + Send + Sync>() {}

#[test]
fn sdk_surface_exports_core_types() {
    let _: Option<sdk::ModelRegistry> = None;
    let _: Option<sdk::Config> = None;
    let _: Option<sdk::Session> = None;
    let _: Option<sdk::Agent> = None;
    let _: Option<sdk::AgentSession> = None;
    let _: sdk::ProviderContext = sdk::ProviderContext::default();
    let _: sdk::StreamOptions = sdk::StreamOptions::default();

    let _: sdk::ToolDefinition = sdk::ToolDef {
        name: "read".to_string(),
        description: "Read file".to_string(),
        parameters: json!({"type": "object"}),
    };
}

#[test]
fn sdk_public_types_have_expected_traits() {
    assert_clone_debug_send_sync::<sdk::Message>();
    assert_clone_debug_send_sync::<sdk::ContentBlock>();
    assert_clone_debug_send_sync::<sdk::ToolCall>();
    assert_clone_debug_send_sync::<sdk::ToolDefinition>();
    assert_clone_debug_send_sync::<sdk::AgentEvent>();
    assert_clone_debug_send_sync::<sdk::RpcModelInfo>();
    assert_clone_debug_send_sync::<sdk::RpcSessionState>();
    assert_clone_debug_send_sync::<sdk::RpcSessionStats>();
    assert_clone_debug_send_sync::<sdk::RpcCommandInfo>();
    assert_clone_debug_send_sync::<sdk::RpcExtensionUiResponse>();
}

#[test]
fn sdk_message_round_trips_via_serde() {
    let message = sdk::Message::User(sdk::UserMessage {
        content: sdk::UserContent::Text("hello".to_string()),
        timestamp: 1234,
    });

    let encoded = serde_json::to_value(&message).expect("serialize sdk::Message");
    let decoded: sdk::Message = serde_json::from_value(encoded.clone()).expect("deserialize");
    let reencoded = serde_json::to_value(decoded).expect("re-serialize");

    assert_eq!(reencoded, encoded);
}

#[test]
fn sdk_rpc_state_round_trips_via_serde() {
    let value = json!({
        "model": {
            "id": "claude-sonnet-4-20250514",
            "name": "Claude Sonnet 4",
            "api": "anthropic-messages",
            "provider": "anthropic",
            "baseUrl": "https://api.anthropic.com",
            "reasoning": true,
            "input": ["text", "image"],
            "contextWindow": 200_000,
            "maxTokens": 8192,
            "cost": {
                "input": 3.0,
                "output": 15.0,
                "cacheRead": 0.3,
                "cacheWrite": 3.75
            }
        },
        "thinkingLevel": "low",
        "isStreaming": false,
        "isCompacting": false,
        "steeringMode": "all",
        "followUpMode": "one-at-a-time",
        "sessionFile": null,
        "sessionId": "session-123",
        "sessionName": "demo",
        "autoCompactionEnabled": true,
        "autoRetryEnabled": false,
        "messageCount": 2,
        "contextTokens": 128,
        "contextWindow": 200_000,
        "pendingMessageCount": 0,
        "durabilityMode": "balanced"
    });

    let state: sdk::RpcSessionState =
        serde_json::from_value(value.clone()).expect("deserialize RpcSessionState");
    let reencoded = serde_json::to_value(state).expect("serialize RpcSessionState");
    assert_eq!(reencoded, value);

    let mut legacy = value;
    legacy.as_object_mut().unwrap().remove("contextTokens");
    legacy.as_object_mut().unwrap().remove("contextWindow");
    let legacy: sdk::RpcSessionState = serde_json::from_value(legacy).expect("older RPC state");
    assert_eq!(legacy.context_tokens, 0);
    assert_eq!(legacy.context_window, None);
}

#[test]
fn sdk_extension_ui_response_round_trips_via_serde() {
    let value = json!({
        "kind": "confirmed",
        "confirmed": true
    });
    let decoded: sdk::RpcExtensionUiResponse =
        serde_json::from_value(value.clone()).expect("deserialize RpcExtensionUiResponse");
    let reencoded = serde_json::to_value(decoded).expect("serialize RpcExtensionUiResponse");
    assert_eq!(reencoded, value);
}

#[test]
fn sdk_rpc_transport_client_typed_methods_work_with_subprocess_bridge() {
    if !cfg!(unix) {
        return;
    }

    let script = r#"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  cmd=$(printf '%s\n' "$line" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
  case "$cmd" in
    get_state)
      printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"model":null,"thinkingLevel":"off","isStreaming":false,"isCompacting":false,"steeringMode":"all","followUpMode":"all","sessionFile":null,"sessionId":"sess-1","sessionName":null,"autoCompactionEnabled":true,"autoRetryEnabled":true,"messageCount":0,"pendingMessageCount":0,"durabilityMode":"balanced"}}\n' "$id"
      ;;
    get_available_models)
      printf '{"type":"response","id":"%s","command":"get_available_models","success":true,"data":{"models":[{"id":"m1","name":"Model 1","api":"anthropic-messages","provider":"anthropic","baseUrl":"https://api.example.com","reasoning":false,"input":["text"],"contextWindow":8192,"maxTokens":1024,"cost":{"input":1.0,"output":2.0,"cacheRead":0.0,"cacheWrite":0.0}}]}}\n' "$id"
      ;;
    set_model)
      printf '{"type":"response","id":"%s","command":"set_model","success":true,"data":{"id":"m1","name":"Model 1","api":"anthropic-messages","provider":"anthropic","baseUrl":"https://api.example.com","reasoning":false,"input":["text"],"contextWindow":8192,"maxTokens":1024,"cost":{"input":1.0,"output":2.0,"cacheRead":0.0,"cacheWrite":0.0}}}\n' "$id"
      ;;
    prompt)
      printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
      printf '{"type":"agent_start","sessionId":"sess-1"}\n'
      printf '{"type":"agent_end","sessionId":"sess-1","messages":[]}\n'
      ;;
    compact)
      printf '{"type":"manual_compaction_start"}\n'
      printf '{"type":"manual_compaction_end","result":{"summary":"manual summary","firstKeptEntryId":"kept-2","tokensBefore":8000,"tokensAfter":600},"aborted":false}\n'
      printf '{"type":"response","id":"%s","command":"compact","success":true,"data":{"summary":"manual summary","firstKeptEntryId":"kept-2","tokensBefore":8000,"tokensAfter":600}}\n' "$id"
      ;;
    handoff)
      printf '{"type":"auto_compaction_start","reason":"handoff"}\n'
      printf '{"type":"auto_compaction_end","result":{"summary":"continuation brief","firstKeptEntryId":"kept-1","tokensBefore":9000,"tokensAfter":500},"aborted":false,"willRetry":false}\n'
      printf '{"type":"response","id":"%s","command":"handoff","success":true,"data":{"summary":"continuation brief","firstKeptEntryId":"kept-1","tokensBefore":9000,"tokensAfter":500,"details":{"mode":"handoff"}}}\n' "$id"
      ;;
    extension_ui_response)
      printf '{"type":"response","id":"%s","command":"extension_ui_response","success":true,"data":{"resolved":true}}\n' "$id"
      ;;
    *)
      printf '{"type":"response","id":"%s","command":"%s","success":true}\n' "$id" "$cmd"
      ;;
  esac
done
"#;

    let options = sdk::RpcTransportOptions {
        binary_path: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), script.to_string()],
        cwd: None,
    };
    let mut client = sdk::RpcTransportClient::connect(options).expect("connect rpc subprocess");

    futures::executor::block_on(async {
        let state = client.get_state().await.expect("get_state");
        assert_eq!(state.session_id, "sess-1");
        assert_eq!(state.thinking_level, "off");
        assert!(state.auto_retry_enabled);
        assert_eq!(state.durability_mode, "balanced");
        assert_eq!(state.context_tokens, 0);
        assert_eq!(state.context_window, None);

        let models = client
            .get_available_models()
            .await
            .expect("get_available_models");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "m1");

        let model = client
            .set_model("anthropic", "m1")
            .await
            .expect("set_model");
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.id, "m1");

        let events = client
            .prompt_with_options("hello", None, Some("steer"))
            .await
            .expect("prompt_with_options");
        assert!(
            events.iter().any(
                |event| event.get("type").and_then(serde_json::Value::as_str) == Some("agent_end")
            ),
            "expected prompt event stream to terminate with agent_end"
        );

        let resolved = client
            .extension_ui_response(
                "req-1",
                1,
                sdk::RpcExtensionUiResponse::Confirmed { confirmed: true },
            )
            .await
            .expect("extension_ui_response");
        assert!(resolved);

        let mut compaction_events = Vec::new();
        let compaction = client
            .compact_with_events(Some("retain decisions"), |event| {
                compaction_events.push(event);
            })
            .await
            .expect("compact_with_events");
        assert_eq!(compaction.summary, "manual summary");
        assert_eq!(compaction.tokens_after, 600);
        assert_eq!(compaction_events.len(), 2);
        assert_eq!(compaction_events[0]["type"], "manual_compaction_start");
        assert_eq!(compaction_events[1]["type"], "manual_compaction_end");
        assert_eq!(
            client.compact().await.unwrap().first_kept_entry_id,
            "kept-2"
        );

        assert!(client.handoff(Some(&"x".repeat(4097))).await.is_err());
        let mut handoff_events = Vec::new();
        let handoff = client
            .handoff_with_events(Some("preserve native media"), |event| {
                handoff_events.push(event);
            })
            .await
            .expect("handoff_with_events");
        assert_eq!(handoff.summary, "continuation brief");
        assert_eq!(handoff.first_kept_entry_id, "kept-1");
        assert_eq!(handoff.tokens_after, 500);
        assert_eq!(handoff.details["mode"], "handoff");
        assert_eq!(handoff_events.len(), 2);
        assert_eq!(handoff_events[0]["type"], "auto_compaction_start");
        assert_eq!(handoff_events[1]["type"], "auto_compaction_end");
        let default_focus = client.handoff(None).await.expect("handoff without focus");
        assert_eq!(default_focus.tokens_before, 9000);
    });
}

#[cfg(unix)]
#[test]
fn sdk_rpc_prompt_streams_live_and_preserves_pre_ack_events() {
    let script = r#"
IFS= read -r line
id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"agent_start","sessionId":"early"}\n'
printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
printf '{"type":"message_update","delta":"first"}\n'
sleep 1
printf '{"type":"agent_end","sessionId":"early","messages":[]}\n'
"#;
    let options = sdk::RpcTransportOptions {
        binary_path: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: None,
    };
    let mut client = sdk::RpcTransportClient::connect(options).expect("connect");
    let started = std::time::Instant::now();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let times = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_cb = std::sync::Arc::clone(&seen);
    let times_cb = std::sync::Arc::clone(&times);
    let events = futures::executor::block_on(client.prompt_with_options_streaming(
        "hello",
        None,
        None,
        move |event| {
            seen_cb
                .lock()
                .unwrap()
                .push(event["type"].as_str().unwrap().to_string());
            times_cb.lock().unwrap().push(started.elapsed());
        },
    ))
    .expect("streaming prompt");
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["agent_start", "message_update", "agent_end"]
    );
    assert_eq!(events.len(), 3);
    assert!(
        times.lock().unwrap()[1] < std::time::Duration::from_millis(700),
        "message_update must be delivered before the delayed agent_end"
    );
}

#[cfg(unix)]
mod callback_only_rpc {
    use super::*;

    fn client(script: &str) -> sdk::RpcTransportClient {
        sdk::RpcTransportClient::connect(sdk::RpcTransportOptions {
            // Timed reads make a missing callback/control a bounded fixture
            // failure instead of leaving the suite parked on its handshake.
            binary_path: PathBuf::from("/bin/bash"),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
        })
        .expect("connect callback-only RPC fixture")
    }

    #[test]
    fn streams_in_order_and_cooperative_abort_preserves_the_connection() {
        let mut client = client(
            r#"
IFS= read -r -t 10 request || exit 1
printf '{"type":"agent_start","sessionId":"callback-only"}\n'
printf '{"type":"response","id":"rpc-1","command":"prompt","success":true}\n'
printf '{"type":"fixture_input","frame":%s}\n' "$request"
IFS= read -r -t 10 control || exit 1
printf '{"type":"response","id":"rpc-2","command":"abort","success":true}\n'
index=0
while [ "$index" -lt 300 ]; do
  printf '{"type":"message_update","index":%s,"delta":"bounded"}\n' "$index"
  index=$((index + 1))
done
printf '{"type":"agent_end","messages":[],"error":"Aborted","control":%s}\n' "$control"
IFS= read -r -t 10 probe || exit 1
printf '{"type":"response","id":"rpc-3","command":"get_state","success":true,"data":{"alive":true}}\n'
"#,
        );
        let control = client.control_handle();
        let mut phase = 0;
        let mut deltas = 0;
        let images = vec![sdk::ImageContent {
            data: "YQ==".to_string(),
            mime_type: "image/png".to_string(),
        }];
        let expected = json!({
            "type":"prompt", "id":"rpc-1", "message":"inspect this",
            "images":images, "streamingBehavior":"steer"
        });
        futures::executor::block_on(client.stream_prompt(
            "inspect this",
            Some(images),
            Some("steer"),
            |event| match event["type"].as_str() {
                Some("agent_start") => {
                    assert_eq!(phase, 0);
                    phase = 1;
                }
                Some("fixture_input") => {
                    assert_eq!(phase, 1);
                    assert_eq!(event["frame"], expected);
                    assert_eq!(control.abort().expect("live abort"), "rpc-2");
                    phase = 2;
                }
                Some("message_update") => {
                    assert_eq!(phase, 2);
                    assert_eq!(event["index"], deltas);
                    deltas += 1;
                }
                Some("agent_end") => {
                    assert_eq!(phase, 2);
                    assert_eq!(event["error"], "Aborted");
                    assert_eq!(event["control"], json!({"type":"abort", "id":"rpc-2"}));
                    phase = 3;
                }
                _ => panic!("unexpected event: {event}"),
            },
        ))
        .expect("terminal turn error is delivered as an event");
        assert_eq!(phase, 3);
        // The pre-ACK event-count budget must not become a whole-turn cap.
        // This callback keeps only counters, regardless of the event count.
        assert_eq!(deltas, 300);
        let state =
            futures::executor::block_on(client.request("get_state", serde_json::Map::new()))
                .expect("cooperative abort must preserve the connection");
        assert_eq!(state, json!({"alive":true}));
        client.shutdown().expect("shutdown callback-only fixture");
    }

    #[test]
    fn native_content_validates_before_dispatch_and_delivers_pre_ack_terminal() {
        let mut client = client(
            r#"
IFS= read -r -t 10 request || exit 1
printf '{"type":"fixture_input","frame":%s}\n' "$request"
printf '{"type":"agent_end","messages":[]}\n'
printf '{"type":"response","id":"rpc-1","command":"prompt","success":true}\n'
IFS= read -r -t 10 probe || exit 1
printf '{"type":"response","id":"rpc-2","command":"get_state","success":true,"data":{"alive":true}}\n'
"#,
        );
        let invalid = futures::executor::block_on(client.stream_prompt_with_content(
            Vec::new(),
            None,
            |_| panic!("rejected native input must not deliver events"),
        ));
        assert!(invalid.is_err());
        let content = vec![
            sdk::ContentBlock::Text(sdk::TextContent::new("listen first")),
            sdk::ContentBlock::Media(sdk::MediaContent {
                data: "Yg".to_string(),
                mime_type: "AUDIO/WAV".to_string(),
                name: Some("  voice\n.wav  ".to_string()),
            }),
        ];
        let mut delivered = Vec::new();
        futures::executor::block_on(client.stream_prompt_with_content(
            content,
            Some("followUp"),
            |event| {
                if event["type"] == "fixture_input" {
                    assert_eq!(
                        event["frame"],
                        json!({
                            "type":"prompt", "id":"rpc-1", "streamingBehavior":"followUp",
                            "content":[
                                {"type":"text", "text":"listen first"},
                                {"type":"media", "data":"Yg", "mimeType":"audio/wav", "name":"voice.wav"}
                            ]
                        })
                    );
                }
                delivered.push(event["type"].as_str().unwrap().to_string());
            },
        ))
        .expect("native callback-only stream");
        assert_eq!(delivered, ["fixture_input", "agent_end"]);
        let state =
            futures::executor::block_on(client.request("get_state", serde_json::Map::new()))
                .expect("pre-ACK terminal leaves the next response aligned");
        assert_eq!(state, json!({"alive":true}));
        client.shutdown().expect("shutdown native fixture");
    }

    #[test]
    fn failed_ack_hides_speculative_events_and_allows_the_next_request() {
        let mut client = client(
            r#"
IFS= read -r -t 10 request || exit 1
printf '{"type":"agent_start","sessionId":"not-admitted"}\n'
printf '{"type":"agent_end","messages":[]}\n'
printf '{"type":"response","id":"rpc-1","command":"prompt","success":false,"error":"fixture refusal"}\n'
IFS= read -r -t 10 probe || exit 1
printf '{"type":"response","id":"rpc-2","command":"get_state","success":true,"data":{"alive":true}}\n'
"#,
        );
        let error = futures::executor::block_on(client.stream_prompt(
            "refuse this turn",
            None,
            None,
            |_| panic!("a failed acknowledgement must not expose speculative events"),
        ))
        .expect_err("server refusal");
        assert!(error.to_string().contains("fixture refusal"));
        let state =
            futures::executor::block_on(client.request("get_state", serde_json::Map::new()))
                .expect("acknowledged refusal preserves the connection");
        assert_eq!(state, json!({"alive":true}));
        client.shutdown().expect("shutdown refusal fixture");
    }

    #[test]
    fn malformed_ack_and_early_eof_revoke_the_transport() {
        for reply in [
            r#"{"type":"response","id":"rpc-1","command":"wrong","success":true}"#,
            r#"{"type":"response","id":"rpc-1","command":"prompt","success":"true"}"#,
            r#"{"type":"response","id":"rpc-1","command":"prompt","success":true}"#,
        ] {
            let script =
                format!("IFS= read -r -t 10 request || exit 1\nprintf '%s\\n' '{reply}'\n");
            let mut client = client(&script);
            let control = client.control_handle();
            let error =
                futures::executor::block_on(client.stream_prompt("hello", None, None, |_| {
                    panic!("fixture sends no complete events")
                }))
                .expect_err("malformed acknowledgement or missing terminal");
            assert!(
                error.to_string().contains("wrong command")
                    || error.to_string().contains("boolean success flag")
                    || error.to_string().contains("exited before sending"),
                "{error}"
            );
            assert!(
                control.abort().is_err(),
                "invalid protocol revokes controls"
            );
            client.shutdown().expect("shutdown failed stream");
        }
    }

    #[test]
    fn dropping_a_live_stream_revokes_controls_and_later_requests() {
        let mut client = client(
            r#"
IFS= read -r -t 10 request || exit 1
printf '{"type":"response","id":"rpc-1","command":"prompt","success":true}\n'
printf '{"type":"agent_start","sessionId":"drop-stream"}\n'
IFS= read -r -t 10 control || exit 1
"#,
        );
        let control = client.control_handle();
        let (cancel, cancelled) = futures::channel::oneshot::channel();
        let mut cancel = Some(cancel);
        let prompt = Box::pin(client.stream_prompt("hello", None, None, move |event| {
            if event["type"] == "agent_start"
                && let Some(cancel) = cancel.take()
            {
                cancel.send(()).expect("cancel live stream");
            }
        }));
        match futures::executor::block_on(futures::future::select(prompt, cancelled)) {
            futures::future::Either::Right((Ok(()), pending)) => drop(pending),
            futures::future::Either::Right((Err(error), _)) => {
                panic!("start event was not delivered: {error}");
            }
            futures::future::Either::Left((result, _)) => {
                panic!("the live stream did not yield to cancellation: {result:?}");
            }
        }
        assert!(control.abort().is_err(), "dropped stream revokes controls");
        assert!(
            futures::executor::block_on(client.request("get_state", serde_json::Map::new()))
                .is_err(),
            "a cancelled stream cannot lend unread frames to another request"
        );
        client.shutdown().expect("join cancelled pipe workers");
    }
}

#[cfg(unix)]
#[test]
fn sdk_rpc_prompt_rejects_wrong_ack_and_reserved_request_fields() {
    let script = r#"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{"type":"response","id":"%s","command":"wrong","success":true}\n' "$id"
done
"#;
    let options = sdk::RpcTransportOptions {
        binary_path: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: None,
    };
    let mut client = sdk::RpcTransportClient::connect(options).expect("connect");
    let error = futures::executor::block_on(client.prompt("hello")).expect_err("wrong command");
    assert!(error.to_string().contains("wrong command"), "{error}");

    let options = sdk::RpcTransportOptions {
        binary_path: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), "sleep 5".into()],
        cwd: None,
    };
    let mut client = sdk::RpcTransportClient::connect(options).expect("connect");
    let mut payload = serde_json::Map::new();
    payload.insert("id".into(), json!("caller-controlled"));
    let error = futures::executor::block_on(client.request("get_state", payload))
        .expect_err("reserved id must be rejected before write");
    assert!(error.to_string().contains("reserved type/id"), "{error}");
}

#[cfg(unix)]
#[test]
fn sdk_rpc_control_handle_can_abort_during_a_live_prompt() {
    let script = r#"
IFS= read -r prompt
prompt_id=$(printf '%s\n' "$prompt" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$prompt_id"
printf '{"type":"agent_start","sessionId":"sess-control"}\n'
IFS= read -r control
control_id=$(printf '%s\n' "$control" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
control_type=$(printf '%s\n' "$control" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
printf '{"type":"response","id":"%s","command":"%s","success":true}\n' "$control_id" "$control_type"
printf '{"type":"agent_end","sessionId":"sess-control","control":"%s","messages":[]}\n' "$control_type"
"#;
    let options = sdk::RpcTransportOptions {
        binary_path: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: None,
    };
    let mut client = sdk::RpcTransportClient::connect(options).expect("connect");
    let control = client.control_handle();
    let control_id = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let stored = std::sync::Arc::clone(&control_id);
    let events = futures::executor::block_on(client.prompt_with_options_streaming(
        "hello",
        None,
        None,
        move |event| {
            if event["type"] == "agent_start" {
                let id = control
                    .abort()
                    .expect("dispatch abort while prompt owns stdout");
                *stored.lock().unwrap() = Some(id);
            }
        },
    ))
    .expect("prompt completes after abort acknowledgement");
    assert_eq!(events.last().unwrap()["control"], "abort");
    assert!(
        control_id
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(|id| id.starts_with("rpc-")),
        "control command must receive an SDK-owned unique request id"
    );
}

#[cfg(unix)]
#[test]
fn sdk_rpc_control_handle_ids_share_the_client_sequence() {
    let options = sdk::RpcTransportOptions {
        binary_path: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), "cat >/dev/null".into()],
        cwd: None,
    };
    let client = sdk::RpcTransportClient::connect(options).expect("connect");
    let control = client.control_handle();
    let first = control.steer("one").expect("steer dispatch");
    let second = control.follow_up("two").expect("follow-up dispatch");
    let third = control.abort().expect("abort dispatch");
    assert_eq!(
        [first.as_str(), second.as_str(), third.as_str()],
        ["rpc-1", "rpc-2", "rpc-3"]
    );
}

#[cfg(unix)]
fn rpc_pipe_fixture(script: &str, cwd: Option<PathBuf>) -> sdk::RpcTransportClient {
    sdk::RpcTransportClient::connect(sdk::RpcTransportOptions {
        binary_path: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd,
    })
    .expect("connect pipe fixture")
}

#[cfg(unix)]
#[test]
fn sdk_rpc_request_yields_and_cancelled_exchange_revokes_controls() {
    let directory = tempfile::tempdir().unwrap();
    let mut client = rpc_pipe_fixture(
        "IFS= read -r request; attempt=0; while [ ! -f release ] && [ \"$attempt\" -lt 500 ]; do sleep 0.02; attempt=$((attempt + 1)); done",
        Some(directory.path().to_path_buf()),
    );
    let control = client.control_handle();
    let request = Box::pin(client.request("get_state", serde_json::Map::new()));
    match futures::executor::block_on(futures::future::select(request, futures::future::ready(())))
    {
        futures::future::Either::Right(((), pending)) => drop(pending),
        futures::future::Either::Left((result, _)) => {
            panic!("an unanswered request blocked its executor: {result:?}");
        }
    }
    let error = control.abort().expect_err("cancelled transport is revoked");
    assert!(error.to_string().contains("transport is closed"), "{error}");
    let error = futures::executor::block_on(client.request("get_state", serde_json::Map::new()))
        .expect_err("a cancelled exchange cannot lend its unread frames to the next call");
    assert!(error.to_string().contains("transport is closed"), "{error}");
    client.shutdown().expect("join cancelled pipe workers");
}

#[cfg(unix)]
#[test]
fn sdk_rpc_stalled_stdin_write_yields_and_can_be_cancelled() {
    // More than an OS pipe can hold. The fixture never reads stdin and exits
    // eventually, so a regression fails rather than hanging the entire suite.
    let directory = tempfile::tempdir().unwrap();
    let mut client = rpc_pipe_fixture(
        "attempt=0; while [ ! -f release ] && [ \"$attempt\" -lt 500 ]; do sleep 0.02; attempt=$((attempt + 1)); done",
        Some(directory.path().to_path_buf()),
    );
    let control = client.control_handle();
    let mut payload = serde_json::Map::new();
    payload.insert("message".into(), json!("x".repeat(8 * 1024 * 1024)));
    let request = Box::pin(client.request("prompt", payload));
    match futures::executor::block_on(futures::future::select(request, futures::future::ready(())))
    {
        futures::future::Either::Right(((), pending)) => drop(pending),
        futures::future::Either::Left((result, _)) => {
            panic!("a full stdin pipe blocked its executor: {result:?}");
        }
    }
    assert!(control.steer("too late").is_err());
    client.shutdown().expect("join interrupted writer");
}

#[cfg(unix)]
#[test]
fn sdk_rpc_live_prompt_cancellation_runs_on_the_same_executor() {
    let directory = tempfile::tempdir().unwrap();
    let script = r#"
IFS= read -r request
id=$(printf '%s\n' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
printf '{"type":"agent_start","sessionId":"cancel-live"}\n'
attempt=0
while [ ! -f release ] && [ "$attempt" -lt 500 ]; do
  sleep 0.02
  attempt=$((attempt + 1))
done
printf '{"type":"agent_end","messages":[]}\n'
"#;
    let mut client = rpc_pipe_fixture(script, Some(directory.path().to_path_buf()));
    let control = client.control_handle();
    let (cancel, cancelled) = futures::channel::oneshot::channel();
    let mut cancel = Some(cancel);
    let prompt = Box::pin(client.prompt_with_options_streaming(
        "start a cancellable turn",
        None,
        None,
        move |event| {
            if event["type"] == "agent_start"
                && let Some(cancel) = cancel.take()
            {
                cancel.send(()).unwrap();
            }
        },
    ));
    match futures::executor::block_on(futures::future::select(prompt, cancelled)) {
        futures::future::Either::Right((Ok(()), pending)) => drop(pending),
        futures::future::Either::Right((Err(error), _)) => {
            panic!("prompt did not emit its start event: {error}");
        }
        futures::future::Either::Left((result, _)) => {
            panic!("prompt kept the ready cancellation future from running: {result:?}");
        }
    }
    let error = control
        .abort()
        .expect_err("cancelled live turn closes its connection");
    assert!(error.to_string().contains("transport is closed"), "{error}");
    client.shutdown().unwrap();
}

#[cfg(unix)]
#[test]
fn sdk_rpc_fragmented_response_yields_and_resumes_without_losing_bytes() {
    use std::future::Future as _;
    use std::task::Poll;

    let directory = tempfile::tempdir().expect("fixture directory");
    let script = r#"
IFS= read -r request
id=$(printf '%s\n' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"value":"hel' "$id"
printf ready > partial-ready
attempt=0
while [ ! -f release-response ] && [ "$attempt" -lt 1000 ]; do
  sleep 0.02
  attempt=$((attempt + 1))
done
printf 'lo"}}\r\n'
"#;
    let mut client = rpc_pipe_fixture(script, Some(directory.path().to_path_buf()));
    let mut request = Box::pin(client.request("get_state", serde_json::Map::new()));
    let first_poll = futures::executor::block_on(std::future::poll_fn(|cx| {
        Poll::Ready(request.as_mut().poll(cx))
    }));
    assert!(
        first_poll.is_pending(),
        "a request must yield while awaiting pipe I/O"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !directory.path().join("partial-ready").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture did not write its prefix"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let partial_poll = futures::executor::block_on(std::future::poll_fn(|cx| {
        Poll::Ready(request.as_mut().poll(cx))
    }));
    assert!(
        partial_poll.is_pending(),
        "an incomplete JSON frame must yield"
    );
    std::fs::write(directory.path().join("release-response"), "release").unwrap();
    let response = futures::executor::block_on(request).expect("completed fragmented response");
    assert_eq!(response, json!({"value": "hello"}));
    client.shutdown().expect("shutdown fragmented fixture");
}

#[cfg(unix)]
#[test]
fn sdk_rpc_completed_server_rejections_preserve_the_connection() {
    let script = r#"
while IFS= read -r request; do
  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  cmd=$(printf '%s\n' "$request" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
  if [ "$cmd" = get_state ]; then
    printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"alive":true}}\n' "$id"
  else
    printf '{"type":"response","id":"%s","command":"%s","success":false,"error":"fixture refusal"}\n' "$id" "$cmd"
  fi
done
"#;
    let mut client = rpc_pipe_fixture(script, None);
    futures::executor::block_on(async {
        assert!(client.prompt("refuse prompt").await.is_err());
        assert!(
            client
                .request("unknown", serde_json::Map::new())
                .await
                .is_err()
        );
        let response = client
            .request("get_state", serde_json::Map::new())
            .await
            .unwrap();
        assert_eq!(response, json!({"alive": true}));
    });
    client.shutdown().unwrap();
}

#[cfg(unix)]
#[test]
fn sdk_rpc_wrong_response_command_closes_the_protocol_lane() {
    let script = r#"
IFS= read -r request
id=$(printf '%s\n' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"response","id":"%s","command":"wrong","success":true}\n' "$id"
"#;
    let mut client = rpc_pipe_fixture(script, None);
    let control = client.control_handle();
    let error = futures::executor::block_on(client.request("get_state", serde_json::Map::new()))
        .expect_err("matching IDs do not authorize the wrong command");
    assert!(error.to_string().contains("wrong command"), "{error}");
    let error = control
        .abort()
        .expect_err("invalid protocol closes controls too");
    assert!(error.to_string().contains("transport is closed"), "{error}");
}

#[cfg(unix)]
#[test]
fn sdk_rpc_duplicate_failure_acknowledgement_closes_the_live_turn() {
    let script = r#"
IFS= read -r request
id=$(printf '%s\n' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
printf '{"type":"response","id":"%s","command":"prompt","success":false,"error":"late refusal"}\n' "$id"
printf '{"type":"agent_end","messages":[]}\n'
"#;
    let mut client = rpc_pipe_fixture(script, None);
    let control = client.control_handle();
    let error = futures::executor::block_on(client.prompt("hello"))
        .expect_err("a duplicate failure is a protocol error, not a completed refusal");
    assert!(
        error.to_string().contains("duplicate acknowledgement"),
        "{error}"
    );
    assert!(
        control.abort().is_err(),
        "later terminal frames cannot enter another turn"
    );
}

#[cfg(unix)]
#[test]
fn sdk_rpc_non_boolean_acknowledgements_close_requests_and_prompts() {
    for prompt in [false, true] {
        let script = r#"
IFS= read -r request
id=$(printf '%s\n' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
cmd=$(printf '%s\n' "$request" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
printf '{"type":"response","id":"%s","command":"%s","success":"false"}\n' "$id" "$cmd"
"#;
        let mut client = rpc_pipe_fixture(script, None);
        let control = client.control_handle();
        let error = if prompt {
            futures::executor::block_on(client.prompt("hello")).map(|_| ())
        } else {
            futures::executor::block_on(client.request("get_state", serde_json::Map::new()))
                .map(|_| ())
        }
        .expect_err("malformed flags do not complete an operation");
        assert!(
            error.to_string().contains("boolean success flag"),
            "{error}"
        );
        assert!(control.abort().is_err());
    }
}

#[cfg(unix)]
fn rpc_fixture_process_status(pid: u32) -> Option<sysinfo::ProcessStatus> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(sysinfo::Process::status)
}

#[cfg(unix)]
#[test]
fn sdk_rpc_shutdown_and_drop_stop_live_and_reaped_root_descendants() {
    for root_exits in [false, true] {
        for explicit_shutdown in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let tail = if root_exits { "exit 0" } else { "wait" };
            let script =
                format!("sleep 30 </dev/null &\nprintf '%s %s' \"$$\" \"$!\" > pids\n{tail}");
            let mut client = rpc_pipe_fixture(&script, Some(directory.path().to_path_buf()));
            let control = client.control_handle();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let pids = loop {
                if let Ok(text) = std::fs::read_to_string(directory.path().join("pids")) {
                    let pids: Vec<u32> = text
                        .split_whitespace()
                        .filter_map(|s| s.parse().ok())
                        .collect();
                    if pids.len() == 2 {
                        break pids;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture did not publish its PIDs"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            };
            if root_exits {
                while rpc_fixture_process_status(pids[0]).is_some_and(|status| {
                    !matches!(
                        status,
                        sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
                    )
                }) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "fixture root did not exit"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            if explicit_shutdown {
                client.shutdown().expect("explicit tree shutdown");
            }
            drop(client);
            assert!(
                control.abort().is_err(),
                "a retained control cannot retain a live child"
            );
            assert!(
                rpc_fixture_process_status(pids[0]).is_none(),
                "root must be reaped"
            );
            while rpc_fixture_process_status(pids[1]).is_some_and(|status| {
                !matches!(
                    status,
                    sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
                )
            }) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "descendant survived client shutdown"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

#[test]
fn sdk_rpc_plan_reviews_bind_the_displayed_submission_and_require_complete_state() {
    let mut state: sdk::RpcPlanState = serde_json::from_value(json!({
        "planMode": "pending_approval",
        "plan": "Inspect the source, implement the change, then verify the result.",
        "reviewId": "p1-first",
    }))
    .expect("complete pending state");
    let first = state.pending_review().unwrap().unwrap();
    state.review_id = Some("p1-second".to_string());
    let second = state.pending_review().unwrap().unwrap();
    assert_eq!(first.text(), second.text());
    assert_ne!(first, second, "identical text is not the same submission");

    for (text, id) in [
        (None, Some("p1-valid".to_string())),
        (Some("  ".to_string()), Some("p1-valid".to_string())),
        (
            Some("é".repeat(128 * 1024 + 1)),
            Some("p1-valid".to_string()),
        ),
        (Some("proposal".to_string()), None),
        (Some("proposal".to_string()), Some(" ".to_string())),
        (Some("proposal".to_string()), Some("x".repeat(65))),
        (Some("proposal".to_string()), Some("bad\0id".to_string())),
    ] {
        state.plan = text;
        state.review_id = id;
        assert!(state.pending_review().is_err());
    }
    state.plan_mode = sdk::RpcPlanMode::Approved;
    assert!(state.pending_review().unwrap().is_none());
    assert!(
        serde_json::from_value::<sdk::RpcPlanState>(json!({
            "planMode": "approve_on_resume", "plan": null, "reviewId": null,
        }))
        .is_err()
    );
}

#[test]
fn sdk_rpc_plan_changes_preserve_committed_decisions_and_persistence_uncertainty() {
    let change: sdk::RpcPlanChange = serde_json::from_value(json!({
        "planMode": "approved", "plan": null, "reviewId": null,
        "changed": true, "approved": true,
        "persistence": {"status": "unconfirmed", "reason": "save interrupted"},
        "stateError": "snapshot busy",
    }))
    .expect("live approval with an unconfirmed save remains a successful change");
    assert!(change.changed);
    assert_eq!(change.state.plan_mode, sdk::RpcPlanMode::Approved);
    assert_eq!(change.state_error.as_deref(), Some("snapshot busy"));
    assert_eq!(
        change.persistence,
        sdk::RpcPlanPersistence::Unconfirmed {
            reason: "save interrupted".to_string(),
        }
    );
    for persistence in [
        json!({"status": "unconfirmed"}),
        json!({"status": "eventually_saved"}),
    ] {
        assert!(
            serde_json::from_value::<sdk::RpcPlanChange>(json!({
                "planMode": "approved", "plan": "proposal", "reviewId": null,
                "changed": true, "persistence": persistence,
            }))
            .is_err(),
            "unknown or incomplete durability must never be treated as saved"
        );
    }
}

#[cfg(unix)]
#[test]
fn sdk_rpc_plan_controls_preserve_review_identity_and_use_save_only_recovery() {
    let directory = tempfile::tempdir().expect("protocol fixture directory");
    let mut client = rpc_pipe_fixture(
        r#"
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-1","command":"set_plan_mode","success":true,"data":{"planMode":"planning","plan":null,"reviewId":null,"changed":true,"persistence":{"status":"memory_only"}}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-2","command":"get_plan","success":true,"data":{"planMode":"pending_approval","plan":"same reviewed text","reviewId":"p1-first"}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-3","command":"get_plan","success":true,"data":{"planMode":"pending_approval","plan":"same reviewed text","reviewId":"p1-second"}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-4","command":"approve_plan","success":false,"error":"PLAN_REVIEW_STALE"}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-5","command":"reject_plan","success":true,"data":{"planMode":"planning","plan":"same reviewed text","reviewId":null,"changed":true,"rejected":true,"persistence":{"status":"unconfirmed","reason":"save interrupted"}}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-6","command":"save_plan","success":true,"data":{"planMode":"planning","plan":"same reviewed text","reviewId":null,"changed":false,"persistence":{"status":"saved"}}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-7","command":"restore_plan","success":true,"data":{"planMode":"pending_approval","plan":"same reviewed text","reviewId":"p1-restored","changed":true,"persistence":{"status":"memory_only"}}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-8","command":"approve_plan","success":true,"data":{"planMode":"approved","plan":"same reviewed text","reviewId":null,"changed":true,"approved":true,"persistence":{"status":"unconfirmed","reason":"disk unavailable"}}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-9","command":"set_plan_mode","success":true,"data":{"planMode":"off","plan":null,"reviewId":null,"changed":true,"persistence":{"status":"saved"}}}'
"#,
        Some(directory.path().to_path_buf()),
    );
    futures::executor::block_on(async {
        let entered = client.set_plan_mode(true).await.expect("enter planning");
        assert_eq!(entered.persistence, sdk::RpcPlanPersistence::MemoryOnly);
        let first = client
            .get_plan()
            .await
            .unwrap()
            .pending_review()
            .unwrap()
            .unwrap();
        let second = client
            .get_plan()
            .await
            .unwrap()
            .pending_review()
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
        let stale = client.approve_plan_review(&first).await.unwrap_err();
        assert!(stale.to_string().contains("PLAN_REVIEW_STALE"));
        let rejected = client
            .reject_plan_review(&second)
            .await
            .expect("reject current review");
        assert_eq!(rejected.state.plan_mode, sdk::RpcPlanMode::Planning);
        assert!(matches!(
            rejected.persistence,
            sdk::RpcPlanPersistence::Unconfirmed { .. }
        ));
        let saved = client
            .checkpoint_plan()
            .await
            .expect("save without repeating rejection");
        assert!(!saved.changed);
        assert_eq!(saved.persistence, sdk::RpcPlanPersistence::Saved);
        let restored = client
            .restore_plan_checkpoint()
            .await
            .expect("restore for fresh review");
        let review = restored.state.pending_review().unwrap().unwrap();
        assert_ne!(review, second);
        let approved = client
            .approve_plan_review(&review)
            .await
            .expect("committed approval");
        assert_eq!(approved.state.plan_mode, sdk::RpcPlanMode::Approved);
        assert_eq!(approved.state.plan.as_deref(), Some(review.text()));
        assert!(matches!(
            approved.persistence,
            sdk::RpcPlanPersistence::Unconfirmed { .. }
        ));
        let exited = client.set_plan_mode(false).await.expect("exit and unpin");
        assert_eq!(exited.state.plan_mode, sdk::RpcPlanMode::Off);
    });
    let recorded = std::fs::read_to_string(directory.path().join("frames.jsonl")).unwrap();
    let frames: Vec<serde_json::Value> = recorded
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        frames,
        vec![
            json!({"type":"set_plan_mode", "id":"rpc-1", "mode":"on"}),
            json!({"type":"get_plan", "id":"rpc-2"}),
            json!({"type":"get_plan", "id":"rpc-3"}),
            json!({"type":"approve_plan", "id":"rpc-4", "reviewId":"p1-first"}),
            json!({"type":"reject_plan", "id":"rpc-5", "reviewId":"p1-second"}),
            json!({"type":"save_plan", "id":"rpc-6"}),
            json!({"type":"restore_plan", "id":"rpc-7"}),
            json!({"type":"approve_plan", "id":"rpc-8", "reviewId":"p1-restored"}),
            json!({"type":"set_plan_mode", "id":"rpc-9", "mode":"off"}),
        ]
    );
    client.shutdown().expect("shutdown protocol fixture");
}

#[cfg(unix)]
#[test]
fn sdk_rpc_session_transitions_deliver_ui_events_without_stealing_terminal_responses() {
    let directory = tempfile::tempdir().expect("session protocol fixture directory");
    let mut client = rpc_pipe_fixture(
        r#"
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"extension_ui_request","id":"ui-new","requestGeneration":11,"method":"confirm","title":"Create session?"}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-2","command":"extension_ui_response","success":true,"data":{"resolved":true}}'
printf '%s\n' '{"type":"response","id":"rpc-1","command":"new_session","success":true,"data":{"cancelled":false}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"ask_request","id":"ask-switch","questions":[{"id":"switch-choice","question":"Keep the current session?","options":[{"label":"Keep"},{"label":"Switch"}]}]}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-4","command":"ask_response","success":true,"data":{"resolved":true}}'
printf '%s\n' '{"type":"response","id":"rpc-3","command":"switch_session","success":true,"data":{"cancelled":true}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"extension_ui_request","id":"ui-fork","requestGeneration":42,"method":"confirm","title":"Inspect fork?"}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-6","command":"extension_ui_response","success":true,"data":{"resolved":true}}'
printf '%s\n' '{"type":"session_transition_hook_error","id":"rpc-5","command":"fork","phase":"after","committed":true,"error":"after-hook failed"}'
printf '%s\n' '{"type":"response","id":"rpc-5","command":"fork","success":true,"data":{"text":"selected text","cancelled":false}}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"ask_request","id":"ask-content","questions":[{"id":"review","question":"Review the selected input?","options":[{"label":"Now"},{"label":"Later"}]}]}'
IFS= read -r frame || exit 1
printf '%s\n' "$frame" >> frames.jsonl
printf '%s\n' '{"type":"response","id":"rpc-8","command":"ask_response","success":true,"data":{"resolved":true}}'
printf '%s\n' '{"type":"response","id":"rpc-7","command":"fork","success":true,"data":{"text":"native text","content":[{"type":"text","text":"native text"},{"type":"image","data":"AQ==","mimeType":"image/png"}],"cancelled":false}}'
"#,
        Some(directory.path().to_path_buf()),
    );
    let control = client.control_handle();
    futures::executor::block_on(async {
        let mut new_events = Vec::new();
        let created = client
            .new_session_with_events(Some(std::path::Path::new("parent.jsonl")), |event| {
                assert_eq!(event["type"], "extension_ui_request");
                control
                    .extension_ui_response(
                        event["id"].as_str().unwrap(),
                        event["requestGeneration"].as_u64().unwrap(),
                        sdk::RpcExtensionUiResponse::Confirmed { confirmed: true },
                    )
                    .expect("reply during new-session hook");
                new_events.push(event);
            })
            .await
            .expect("new-session terminal response");
        assert!(!created.cancelled);
        assert_eq!(new_events.len(), 1);

        let mut switch_events = Vec::new();
        let switched = client
            .switch_session_with_events(std::path::Path::new("target.jsonl"), |event| {
                assert_eq!(event["type"], "ask_request");
                control
                    .ask_response(
                        event["id"].as_str().unwrap(),
                        sdk::AskResponse {
                            answers: vec![sdk::AskAnswer {
                                question_id: "switch-choice".to_string(),
                                selected: vec!["Keep".to_string()],
                                other: None,
                            }],
                            dismissed: false,
                        },
                    )
                    .expect("reply during switch hook");
                switch_events.push(event);
            })
            .await
            .expect("cancelled switch is a successful protocol response");
        assert!(switched.cancelled);
        assert_eq!(switch_events.len(), 1);

        let mut fork_events = Vec::new();
        let forked = client
            .fork_with_events("entry-text", |event| {
                if event["type"] == "extension_ui_request" {
                    control
                        .extension_ui_response(
                            event["id"].as_str().unwrap(),
                            event["requestGeneration"].as_u64().unwrap(),
                            sdk::RpcExtensionUiResponse::Confirmed { confirmed: true },
                        )
                        .expect("reply during fork hook");
                }
                fork_events.push(event);
            })
            .await
            .expect("after-hook diagnostic does not undo committed fork");
        assert!(!forked.cancelled);
        assert_eq!(forked.text, "selected text");
        assert_eq!(fork_events.len(), 2);
        assert_eq!(
            fork_events[1],
            json!({
                "type": "session_transition_hook_error",
                "id": "rpc-5",
                "command": "fork",
                "phase": "after",
                "committed": true,
                "error": "after-hook failed",
            })
        );

        let mut content_events = Vec::new();
        let native = client
            .fork_with_content_and_events("entry-native", |event| {
                assert_eq!(event["type"], "ask_request");
                control
                    .ask_response(
                        event["id"].as_str().unwrap(),
                        sdk::AskResponse {
                            answers: Vec::new(),
                            dismissed: true,
                        },
                    )
                    .expect("dismiss live ask card");
                content_events.push(event);
            })
            .await
            .expect("native fork terminal response");
        assert!(!native.cancelled);
        assert_eq!(native.text, "native text");
        assert_eq!(content_events.len(), 1);
        assert_eq!(
            serde_json::to_value(native.content.expect("selected attachments")).unwrap(),
            json!([
                {"type":"text","text":"native text"},
                {"type":"image","data":"AQ==","mimeType":"image/png"},
            ])
        );
    });
    let recorded = std::fs::read_to_string(directory.path().join("frames.jsonl")).unwrap();
    let frames: Vec<serde_json::Value> = recorded
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        frames,
        vec![
            json!({"type":"new_session","id":"rpc-1","parentSession":"parent.jsonl"}),
            json!({
                "type":"extension_ui_response","id":"rpc-2",
                "requestId":"ui-new","requestGeneration":11,"confirmed":true,
            }),
            json!({"type":"switch_session","id":"rpc-3","sessionPath":"target.jsonl"}),
            json!({
                "type":"ask_response","id":"rpc-4","requestId":"ask-switch",
                "answers":[{"questionId":"switch-choice","selected":["Keep"]}],
            }),
            json!({"type":"fork","id":"rpc-5","entryId":"entry-text"}),
            json!({
                "type":"extension_ui_response","id":"rpc-6",
                "requestId":"ui-fork","requestGeneration":42,"confirmed":true,
            }),
            json!({"type":"fork","id":"rpc-7","entryId":"entry-native"}),
            json!({
                "type":"ask_response","id":"rpc-8","requestId":"ask-content","dismissed":true,
            }),
        ]
    );
    client
        .shutdown()
        .expect("shutdown session protocol fixture");
}

#[test]
fn sdk_is_only_public_root_module_and_basic_sdk_imports_exclusively_from_sdk() {
    fn unhidden_root_pub_mods(lib_source: &str) -> Vec<String> {
        let mut unhidden = Vec::new();
        let mut pending_hidden = false;
        for raw_line in lib_source.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            if line == "#[doc(hidden)]" {
                pending_hidden = true;
                continue;
            }
            if line.starts_with("#[") {
                continue;
            }
            if let Some(rest) = line.strip_prefix("pub mod ") {
                let name = rest
                    .trim_end_matches(';')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('{');
                if !pending_hidden {
                    unhidden.push(name.to_string());
                }
            }
            pending_hidden = false;
        }
        unhidden
    }

    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lib_rs = std::fs::read_to_string(manifest_dir.join("src/lib.rs")).expect("read src/lib.rs");
    assert_eq!(
        unhidden_root_pub_mods(&lib_rs),
        vec!["sdk".to_string()],
        "pi::sdk must be the sole non-#[doc(hidden)] root pub mod in src/lib.rs"
    );

    // Planted negative: an accidental unhidden `pub mod tools;` must be caught.
    assert_eq!(
        unhidden_root_pub_mods("#[doc(hidden)]\npub mod agent;\npub mod sdk;\npub mod tools;\n"),
        vec!["sdk".to_string(), "tools".to_string()]
    );

    let basic_sdk = std::fs::read_to_string(manifest_dir.join("examples/basic_sdk.rs"))
        .expect("read examples/basic_sdk.rs");
    for (line_no, raw_line) in basic_sdk.lines().enumerate() {
        let code = raw_line.split("//").next().unwrap_or("");
        let mut rest = code;
        while let Some(pos) = rest.find("pi::") {
            let after = &rest[pos + 4..];
            assert!(
                after.starts_with("sdk::") || after.starts_with("sdk "),
                "examples/basic_sdk.rs:{} imports internal module outside pi::sdk: {}",
                line_no + 1,
                raw_line.trim()
            );
            rest = after;
        }
    }
}
