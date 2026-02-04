use pi::sse::SseParser;

#[test]
fn sse_flush_processes_data_field_without_colon() {
    let mut parser = SseParser::new();
    parser.feed("data");
    let event = parser.flush();
    assert!(event.is_some(), "Should emit event for \"data\" at EOF");
    let event = event.unwrap();
    assert_eq!(event.data, "");
}

#[test]
fn sse_flush_processes_field_without_value_after_data() {
    let mut parser = SseParser::new();
    parser.feed("data: foo\nevent");
    let event = parser.flush().expect("expected event at EOF");
    assert_eq!(event.data, "foo");
    assert_eq!(event.event, "");
}
