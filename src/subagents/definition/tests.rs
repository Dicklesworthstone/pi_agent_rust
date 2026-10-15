use super::*;
use crate::subagents::{AgentScope, SubagentTool, child_args, discover_agents_with_roots};
use crate::tools::Tool;
use serde_json::json;

fn definition(extra: &str) -> Result<AgentDefinition> {
    parse(
        &format!("---\nname: analyst\ndescription: inspect changes\n{extra}\n---\nFollow the assignment.\n"),
        Path::new("definitions/analyst.md"),
        AgentSource::User,
    )
}

fn argv(agent: &AgentDefinition) -> Vec<String> {
    child_args(agent, "inspect", None, None, agent.output_schema.as_ref())
        .into_iter().map(|value| value.to_string_lossy().into_owned()).collect()
}

#[test]
fn native_csv_and_single_line_schema_keep_their_meaning() {
    let agent = definition("tools: read, grep\nskills: one.md,two.md\nmodel: provider/model\nthinking: high\noutput_schema: {\"type\":\"object\"}").unwrap();
    assert_eq!(agent.tools, Some(vec!["read".into(), "grep".into()]));
    assert_eq!(agent.skills, [PathBuf::from("definitions/one.md"), PathBuf::from("definitions/two.md")]);
    assert_eq!(agent.model.as_deref(), Some("provider/model"));
    assert_eq!(agent.reasoning.as_deref(), Some("high"));
    assert_eq!(agent.output_schema, Some(json!({"type":"object"})));
    assert!(!argv(&agent).iter().any(|value| value == "--no-tools"));
}

#[test]
fn list_tools_and_explicit_empty_sets_never_inherit_default_tools() {
    for extra in ["tools: [read, 'grep',]\n", "tools:\n  - read\n  - \"grep\" # comment"] {
        assert_eq!(definition(extra).unwrap().tools, Some(vec!["read".into(), "grep".into()]));
    }
    for extra in ["tools:", "tools: # none", "tools: ''", "tools: \"\"", "tools: []", "tools: [ ]"] {
        let agent = definition(extra).unwrap();
        assert_eq!(agent.tools, Some(Vec::new()), "{extra}");
        let args = argv(&agent);
        assert!(args.iter().any(|value| value == "--no-tools"), "{args:?}");
        assert!(args.windows(2).any(|pair| pair[0] == "--tools" && pair[1].is_empty()));
    }
    let omitted = definition("").unwrap();
    assert!(omitted.tools.is_none());
    let args = argv(&omitted);
    assert!(!args.iter().any(|value| value == "--no-tools"));
    assert!(args.windows(2).any(|pair| pair[0] == "--tools" && pair[1].contains("bash")));
}

#[test]
#[cfg(feature = "tui")]
fn empty_definition_argv_is_accepted_by_the_real_cli_without_builtin_tools() {
    use clap::Parser as _;
    let agent = definition("tools: []").unwrap();
    let mut args = vec![std::ffi::OsString::from("pi")];
    args.extend(child_args(&agent, "inspect", None, None, None));
    let cli = crate::cli::Cli::try_parse_from(args).unwrap();
    assert!(cli.no_tools);
    assert!(cli.enabled_tools().is_empty());
}

#[test]
fn quoted_skill_members_preserve_commas_apostrophes_hashes_and_unicode() {
    let agent = definition("skills: ['team''s # notes, 雪.md', \"10\", It's-fine.md]").unwrap();
    assert_eq!(agent.skills, [
        PathBuf::from("definitions/team's # notes, 雪.md"),
        PathBuf::from("definitions/10"),
        PathBuf::from("definitions/It's-fine.md"),
    ]);
    let agent = definition("skills:\n  - 'first,second.md'\n  - \"quoted\\\"name.md\"").unwrap();
    assert_eq!(agent.skills[0], PathBuf::from("definitions/first,second.md"));
    assert_eq!(agent.skills[1], PathBuf::from("definitions/quoted\"name.md"));
    assert_eq!(definition("skills: '10'").unwrap().skills[0], PathBuf::from("definitions/10"));
}

#[test]
fn comments_do_not_strip_quoted_data_or_prose_apostrophes() {
    for (input, expected) in [
        (" 'team''s # notes' # comment", " 'team''s # notes' "),
        (" It's fine # comment", " It's fine "),
        (" \"say \\\"#yes\\\"\" # comment", " \"say \\\"#yes\\\"\" "),
        (" token#part # comment", " token#part "),
    ] {
        assert_eq!(commentless(input), expected);
    }
    let agent = definition("model: 'provider/team''s-model' # comment").unwrap();
    assert_eq!(agent.model.as_deref(), Some("provider/team's-model"));
}

#[test]
fn bom_crlf_and_markdown_body_are_not_reformatted_or_reparsed() {
    let body = "\r\n## Task\r\n```yaml\r\n---\r\nname: body-not-metadata\r\n```\r\n";
    let raw = format!("\u{feff}---\r\nname: analyst\r\ndescription: inspect\r\ntools: []\r\n---\r\n{body}");
    let agent = parse(&raw, Path::new("agent.md"), AgentSource::Project).unwrap();
    assert_eq!(agent.name, "analyst");
    assert_eq!(agent.system_prompt, body);
}

#[test]
fn literal_blocks_preserve_lines_and_nested_field_names_are_data() {
    let raw = "---\nname: analyst\ndescription: |-\n  first paragraph\n\n    indented detail\n  final line\noutput_schema: |\n  {\n    \"type\": \"object\",\n    \"properties\": {\"model\": {\"type\": \"string\"}}\n  }\n---\nbody";
    let agent = parse(raw, Path::new("agent.md"), AgentSource::User).unwrap();
    assert_eq!(agent.description, "first paragraph\n\n  indented detail\nfinal line");
    assert!(agent.model.is_none());
    assert_eq!(agent.output_schema.unwrap()["properties"]["model"]["type"], "string");
    let lines = ["  ", "  first", "", "  second", ""];
    let field = Field { inline: "|+", continuation: &lines };
    assert_eq!(block_text(&field, Path::new("agent.md")).unwrap(), "\nfirst\n\nsecond\n\n");
}

#[test]
fn folded_descriptions_accept_plain_lines_and_refuse_ambiguous_folding() {
    let raw = "---\nname: analyst\ndescription: >-\n  Inspect code\n  and report defects.\n---\nbody";
    let agent = parse(raw, Path::new("agent.md"), AgentSource::User).unwrap();
    assert_eq!(agent.description, "Inspect code and report defects.");
    for lines in [vec!["  first", "", "  second"], vec!["  first", "    more indented"]] {
        let field = Field { inline: ">", continuation: &lines };
        assert!(block_text(&field, Path::new("agent.md")).is_err());
    }
}

#[test]
fn ambiguous_duplicate_or_unhandled_policy_fields_fail_closed() {
    for extra in [
        "tools: read\ntools: bash", "model: a\nmodel: b", "thinking: low\nreasoning: high",
        "output: {}\noutput_schema: {}", "name: replacement", "unknown_policy: true",
        "tools: &shared [read]", "tools: *shared", "tools: !tag read",
        "tools:\n  - read\n    - bash", "tools: [read\n  - bash",
        "description: replaced", "model: \"unclosed", "model: 'unclosed",
        "output_schema: {not json", "output_schema: {\"type\":17}", "output_schema: []",
    ] {
        assert!(definition(extra).is_err(), "accepted {extra}");
    }
    for raw in ["body", "---\nname: x\ndescription: y", "---\nname: x\n---\nbody"] {
        assert!(parse(raw, Path::new("agent.md"), AgentSource::User).is_err());
    }
}

#[test]
fn malformed_lists_are_not_ignored_or_coerced_into_tools() {
    for extra in [
        "tools: 42", "tools: true", "tools: null", "tools: [read, false]",
        "tools: [null]", "tools: [42]", "tools: [read,,grep]", "tools: [{}]",
        "tools: [[read]]", "tools: ['']", "tools: ['read,write']", "tools: ['--tools']",
        "tools: [\"read\\u0000write\"]", "skills:\n  - name: embedded-map",
        "tools:\n\t- read", "tools:\n - read\n  - grep",
    ] {
        let error = definition(extra).unwrap_err().to_string();
        assert!(error.contains("PI_AGENT_DEFINITION_INVALID"), "{extra}: {error}");
    }
}

#[test]
fn definition_structured_policy_uses_the_existing_native_json_validator() {
    let any_json = definition("structured: true").unwrap();
    assert_eq!(any_json.output_schema, Some(json!({})));
    assert!(crate::subagents::validate_child_output("ordinary prose", any_json.output_schema.as_ref().unwrap()).is_err());
    assert_eq!(crate::subagents::validate_child_output("42", any_json.output_schema.as_ref().unwrap()).unwrap(), 42);
    let typed = definition("structured: true\noutput: {\"type\":\"object\",\"required\":[\"verdict\"]}").unwrap();
    assert!(crate::subagents::validate_child_output("{}", typed.output_schema.as_ref().unwrap()).is_err());
    assert!(crate::subagents::validate_child_output("{\"verdict\":\"pass\"}", typed.output_schema.as_ref().unwrap()).is_ok());
    assert!(definition("structured: false").unwrap().output_schema.is_none());
    for extra in ["structured: null", "structured: \"true\"", "structured: false\noutput: {}"] {
        assert!(definition(extra).is_err());
    }
}

#[test]
fn syntax_errors_do_not_echo_schema_or_policy_values() {
    for extra in ["output_schema: PRIVATE-CANARY", "tools: [PRIVATE-CANARY, 42]", "structured: PRIVATE-CANARY"] {
        let message = definition(extra).unwrap_err().to_string();
        assert!(!message.contains("PRIVATE-CANARY"));
    }
}

#[test]
fn bounded_input_and_list_limits_are_enforced_before_use() {
    assert!(definition(&format!("#{}", "x".repeat(MAX_HEADER_BYTES))).is_err());
    let raw = format!("---\nname: x\ndescription: y\n---\n{}", "x".repeat(MAX_FILE_BYTES));
    assert!(parse(&raw, Path::new("agent.md"), AgentSource::User).is_err());
    let accepted = format!("tools: [{}]", vec!["read"; MAX_LIST_ITEMS].join(","));
    assert_eq!(definition(&accepted).unwrap().tools.unwrap().len(), MAX_LIST_ITEMS);
    let refused = format!("tools: [{}]", vec!["read"; MAX_LIST_ITEMS + 1].join(","));
    assert!(definition(&refused).is_err());
}

fn write_agent(directory: &Path, filename: &str, extra: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join(filename), format!("---\nname: analyst\ndescription: inspect\n{extra}\n---\nReturn JSON.\n")).unwrap();
}

#[test]
fn discovery_rejects_same_directory_duplicates_but_preserves_project_override() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global");
    let cwd = dir.path().join("workspace");
    let user = global.join("agents");
    write_agent(&user, "one.md", "tools: [read]");
    write_agent(&cwd.join(".pi/agents"), "one.md", "tools: []");
    let loaded = discover_agents_with_roots(&cwd, &global, AgentScope::Both).unwrap();
    assert_eq!(loaded["analyst"].tools, Some(Vec::new()));
    write_agent(&user, "two.md", "tools: [bash]");
    assert!(discover_agents_with_roots(&cwd, &global, AgentScope::User).is_err());
}

#[test]
fn file_reader_refuses_oversized_and_non_utf8_sources() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.md");
    std::fs::write(&path, vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
    assert!(read(&path, AgentSource::User).is_err());
    std::fs::write(&path, [0xff, 0xfe]).unwrap();
    assert!(read(&path, AgentSource::User).is_err());
    assert!(read(dir.path(), AgentSource::User).is_err());
}

#[cfg(unix)]
fn child(path: &Path, answer: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let event = json!({"type":"agent_end","messages":[{
        "role":"assistant","stopReason":"stop","content":[{"type":"text","text":answer}]
    }]}).to_string();
    let quoted = event.replace('\'', "'\\''");
    let file = path.join("child.sh");
    std::fs::write(&file, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > child-argv.txt\nprintf 'run\\n' >> child-runs.txt\nprintf '%s\\n' '{quoted}'\n")).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    file
}

#[cfg(unix)]
fn execute(tool: &SubagentTool, input: Value) -> Result<crate::tools::ToolOutput> {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().unwrap().block_on(tool.execute("definition-test", input, None))
}

#[test]
#[cfg(unix)]
fn typed_definition_reaches_real_child_argv_and_parent_result_validation() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global");
    write_agent(&global.join("agents"), "analyst.md", "tools: []\nstructured: true\noutput: |\n  {\"type\":\"object\",\"required\":[\"verdict\"],\"properties\":{\"verdict\":{\"const\":\"pass\"}}}");
    let binary = child(dir.path(), "{\"verdict\":\"pass\"}");
    let tool = SubagentTool::with_paths(dir.path().to_path_buf(), global, binary);
    let result = execute(&tool, json!({"agent":"analyst","task":"review","scope":"user","schemaMode":"strict"})).unwrap();
    assert!(!result.is_error, "{result:?}");
    let details = result.details.unwrap();
    assert_eq!(details["results"][0]["data"]["verdict"], "pass");
    assert_eq!(details["results"][0]["schemaValid"], true);
    assert_eq!(details["results"][0]["tools"], json!([]));
    let args = std::fs::read_to_string(dir.path().join("child-argv.txt")).unwrap();
    assert!(args.lines().any(|line| line == "--no-tools"));
    assert!(args.contains("--append-system-prompt\n"));
    assert!(args.contains("single JSON value matching this JSON Schema"));
    assert_eq!(std::fs::read_to_string(dir.path().join("child-runs.txt")).unwrap(), "run\n");
}

#[test]
#[cfg(unix)]
fn definition_schema_survives_footer_opt_out_and_uses_bounded_corrective_retry() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global");
    write_agent(&global.join("agents"), "analyst.md", "tools: [read]\nstructured: true\noutput: {\"type\":\"object\"}");
    let tool = SubagentTool::with_paths(dir.path().to_path_buf(), global, child(dir.path(), "not JSON"))
        .with_structured_results(false);
    let result = execute(&tool, json!({"agent":"analyst","task":"review","scope":"user","schemaMode":"strict"})).unwrap();
    assert!(result.is_error);
    let details = result.details.unwrap();
    assert_eq!(details["results"][0]["schemaValid"], false);
    assert_eq!(details["results"][0]["schemaRetries"], 1);
    assert_eq!(std::fs::read_to_string(dir.path().join("child-runs.txt")).unwrap(), "run\nrun\n");
}

#[test]
#[cfg(unix)]
fn invalid_definition_prevents_any_child_launch() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global");
    write_agent(&global.join("agents"), "analyst.md", "tools: [read, null]");
    let tool = SubagentTool::with_paths(dir.path().to_path_buf(), global, child(dir.path(), "unused"));
    let error = execute(&tool, json!({"agent":"analyst","task":"review","scope":"user"})).unwrap_err();
    assert!(error.to_string().contains("PI_AGENT_DEFINITION_INVALID"));
    assert!(!dir.path().join("child-runs.txt").exists());
}
