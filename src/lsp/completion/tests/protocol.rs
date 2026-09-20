//! Real public-tool calls, child stdio, and source-file side effects.

use super::*;
use crate::config::Config;
use crate::tools::Tool as _;
use std::process::{Command, Stdio};

const SOURCE: &str = "// header\nfn main() { Ty }\n";
const WITH_IMPORT: &str = "use example::Type;\n// header\nfn main() { Type }\n";
const PEER: &str = include_str!("server.py");

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new().enable_parking(false)
        .worker_threads(1).blocking_threads(1,8).build().unwrap()
}

fn fixture(root: &Path, mode: &str) -> Option<LspTool> {
    let python=["python3","python"].into_iter().find(|program| {
        Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
            .status().is_ok_and(|status| status.success())
    });
    let Some(python)=python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),"completion protocol peer requires Python");
        eprintln!("SKIP completion protocol fixture: Python unavailable");
        return None;
    };
    let root=root.canonicalize().unwrap();
    std::fs::write(root.join("source.picomp"),if mode=="replace" { "// header\nfn main() { TyXX }\n" } else { SOURCE }).unwrap();
    std::fs::write(root.join(".completion-root"),"").unwrap();
    let script=root.join("completion_peer.py");
    std::fs::write(&script,PEER).unwrap();
    let config: Config=serde_json::from_value(json!({"lsp":{"servers":{"completion-fixture":{
        "command":python,"args":["-I","-u",script.display().to_string(),mode],
        "languages":["plaintext"],"extensions":[".picomp"],"rootMarkers":[".completion-root"]
    }}}})).unwrap();
    Some(LspTool::new(&root,Some(&config)))
}

fn request() -> Value {
    json!({"action":"completion","file":"source.picomp","position":{"line":1,"character":14},"query":"Ty","timeout":5})
}
fn run(tool: &LspTool, runtime: &asupersync::runtime::Runtime, request: Value) -> Result<ToolOutput> {
    runtime.block_on(tool.execute("completion-test",request,None))
}
fn list(tool: &LspTool, runtime: &asupersync::runtime::Runtime) -> String {
    let output=run(tool,runtime,request()).unwrap();
    assert!(!output.is_error);
    output.details.unwrap()["items"][0]["completionId"].as_str().unwrap().to_string()
}
fn select(id: &str, apply: bool) -> Value {
    json!({"action":"completion","completionId":id,"apply":apply,"timeout":5})
}
fn source(root: &Path) -> String { std::fs::read_to_string(root.join("source.picomp")).unwrap() }
fn events(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("completion-requests.jsonl")).unwrap().lines()
        .map(|line|serde_json::from_str(line).unwrap()).collect()
}

#[test]
fn list_resolve_preview_and_apply_auto_imports_through_the_public_tool() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"normal") else { return };
    let rt=runtime();
    let id=list(&tool,&rt);
    assert_eq!(source(temp.path()),SOURCE);
    let preview=run(&tool,&rt,select(&id,false)).unwrap().details.unwrap();
    assert_eq!(preview["applied"],false);
    assert_eq!(preview["canApply"],true);
    assert_eq!(preview["edits"].as_array().unwrap().len(),2);
    assert_eq!(preview["documentation"],"A semantic type.");
    assert_eq!(source(temp.path()),SOURCE);
    let applied=run(&tool,&rt,select(&id,true)).unwrap().details.unwrap();
    assert_eq!(applied["applied"],true);
    assert_eq!(applied["additionalEdits"],1);
    assert_eq!(applied["commandExecuted"],false);
    assert_eq!(source(temp.path()),WITH_IMPORT);
    assert!(run(&tool,&rt,select(&id,true)).is_err());
    let log=events(temp.path());
    assert_eq!(log.iter().filter(|event|event["method"]=="completionItem/resolve").count(),1);
    assert!(!log.iter().any(|event|event["method"]=="workspace/executeCommand"));
}

#[test]
fn server_order_keys_prefix_filter_and_incomplete_flag_reach_the_agent() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"normal") else { return };
    let rt=runtime();
    let mut input=request(); input.as_object_mut().unwrap().remove("query");
    let output=run(&tool,&rt,input).unwrap().details.unwrap();
    assert_eq!(output["isIncomplete"],true);
    assert_eq!(output["items"][0]["label"],"Other");
    assert_eq!(output["items"][1]["label"],"Type");
    let output=run(&tool,&rt,request()).unwrap().details.unwrap();
    assert_eq!(output["count"],1);
    assert_eq!(output["items"][0]["label"],"Type");
}

#[test]
fn nonresolving_array_items_apply_without_sending_resolve() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"array") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    run(&tool,&rt,select(&id,true)).unwrap();
    assert_eq!(source(temp.path()),"// header\nfn main() { Type }\n");
    assert!(!events(temp.path()).iter().any(|event|event["method"]=="completionItem/resolve"));
}

#[test]
fn plain_insert_text_requires_then_honors_an_explicit_replacement_span() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"plain") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    assert_eq!(run(&tool,&rt,select(&id,false)).unwrap().details.unwrap()["canApply"],false);
    assert!(run(&tool,&rt,select(&id,true)).unwrap_err().to_string().contains("RANGE_REQUIRED"));
    assert_eq!(source(temp.path()),SOURCE);
    let mut input=request();
    input["range"]=json!({"start":{"line":1,"character":12},"end":{"line":1,"character":14}});
    let id=run(&tool,&rt,input).unwrap().details.unwrap()["items"][0]["completionId"].as_str().unwrap().to_string();
    run(&tool,&rt,select(&id,true)).unwrap();
    assert_eq!(source(temp.path()),"// header\nfn main() { Type }\n");
}

#[test]
fn insert_replace_item_replaces_the_suffix_not_only_the_typed_prefix() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"replace") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    let preview=run(&tool,&rt,select(&id,false)).unwrap().details.unwrap();
    assert_eq!(preview["edits"][0]["range"]["end"]["character"],16);
    run(&tool,&rt,select(&id,true)).unwrap();
    assert_eq!(source(temp.path()),"// header\nfn main() { Type }\n");
}

#[test]
fn external_edits_during_listing_or_resolution_are_never_overwritten() {
    for mode in ["drift_list","drift_resolve"] {
        let temp=tempfile::tempdir().unwrap();
        let Some(tool)=fixture(temp.path(),mode) else { return };
        let rt=runtime();
        let error=if mode=="drift_list" { run(&tool,&rt,request()).unwrap_err() }
            else { let id=list(&tool,&rt); run(&tool,&rt,select(&id,true)).unwrap_err() };
        assert!(error.to_string().contains("STALE"),"{mode}: {error}");
        assert_eq!(source(temp.path()),"external\n");
    }
}

#[test]
fn changing_source_after_preview_rejects_the_cached_edit() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"normal") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    run(&tool,&rt,select(&id,false)).unwrap();
    std::fs::write(temp.path().join("source.picomp"),"external\n").unwrap();
    assert!(run(&tool,&rt,select(&id,true)).unwrap_err().to_string().contains("STALE"));
    assert_eq!(source(temp.path()),"external\n");
}

#[test]
fn same_text_reopen_and_expired_handles_do_not_reuse_old_identity() {
    for expire in [false,true] {
        let temp=tempfile::tempdir().unwrap();
        let Some(tool)=fixture(temp.path(),"normal") else { return };
        let rt=runtime(); let id=list(&tool,&rt);
        if expire {
            let mut cache=lock(&tool.completions.0);
            let candidate=cache.items.get_mut(&id).unwrap();
            Arc::get_mut(&mut candidate.source).unwrap().created=Instant::now()-HANDLE_AGE;
        } else {
            let entry=lock(&tool.completions.0).items[&id].source.entry.upgrade().unwrap();
            entry.client.invalidate_all();
            rt.block_on(tool.synced(&temp.path().join("source.picomp"))).unwrap();
        }
        assert!(run(&tool,&rt,select(&id,true)).unwrap_err().to_string().contains("STALE"));
        assert_eq!(source(temp.path()),SOURCE);
    }
}

#[test]
fn another_listing_or_reload_retires_prior_handles() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"normal") else { return };
    let rt=runtime(); let old=list(&tool,&rt); let new=list(&tool,&rt);
    assert_ne!(old,new);
    assert!(run(&tool,&rt,select(&old,true)).is_err());
    run(&tool,&rt,json!({"action":"reload"})).unwrap();
    assert!(run(&tool,&rt,select(&new,true)).is_err());
    assert_eq!(source(temp.path()),SOURCE);
}

#[test]
fn unsupported_malformed_and_empty_servers_do_not_share_one_result() {
    for mode in ["unsupported","encoding","malformed","empty"] {
        let temp=tempfile::tempdir().unwrap();
        let Some(tool)=fixture(temp.path(),mode) else { return };
        let rt=runtime();
        if mode=="empty" {
            let output=run(&tool,&rt,request()).unwrap().details.unwrap();
            assert_eq!(output["count"],0);
            assert_eq!(output["isIncomplete"],false);
        } else { assert!(run(&tool,&rt,request()).is_err(),"{mode}"); }
        assert_eq!(source(temp.path()),SOURCE);
    }
}

#[test]
fn unsupported_modes_overlaps_and_resolve_substitutions_leave_source_untouched() {
    for mode in ["command","indent","overlap","mutate"] {
        let temp=tempfile::tempdir().unwrap();
        let Some(tool)=fixture(temp.path(),mode) else { return };
        let rt=runtime(); let id=list(&tool,&rt);
        assert!(run(&tool,&rt,select(&id,true)).is_err(),"{mode}");
        assert_eq!(source(temp.path()),SOURCE);
        assert!(!events(temp.path()).iter().any(|event|event["method"]=="workspace/executeCommand"));
    }
}

#[test]
fn completion_never_authorizes_unsolicited_server_edits() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"unsolicited") else { return };
    let rt=runtime(); list(&tool,&rt);
    assert_eq!(source(temp.path()),SOURCE);
    let log=events(temp.path());
    let reply=log.iter().find(|event|event["id"]=="unsolicited-edit" && event.get("method").is_none()).unwrap();
    assert_eq!(reply["result"]["applied"],false);
}

#[test]
fn caller_timeout_bounds_initialization_as_well_as_completion_response() {
    for mode in ["hang_initialize","hang"] {
        let temp=tempfile::tempdir().unwrap();
        let Some(tool)=fixture(temp.path(),mode) else { return };
        let rt=runtime(); let mut input=request(); input["timeout"]=json!(1);
        let start=Instant::now();
        let error=run(&tool,&rt,input).unwrap_err();
        assert!(error.to_string().contains("LSP_TIMEOUT"),"{error}");
        assert!(start.elapsed()<Duration::from_secs(10),"caller budget was ignored");
        assert_eq!(source(temp.path()),SOURCE);
        assert!(lock(&tool.completions.0).items.is_empty());
    }
}

#[test]
fn invalid_cursor_and_conflicting_handles_fail_without_starting_a_server() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"normal") else { return };
    let rt=runtime(); let mut input=request(); input["position"]["character"]=json!(999);
    assert!(run(&tool,&rt,input).is_err());
    let mut input=request(); input["completionId"]=json!("guessed");
    assert!(run(&tool,&rt,input).is_err());
    assert!(tool.registry.status().is_empty());
    assert!(!temp.path().join("completion-requests.jsonl").exists());
}

#[test]
fn a_completion_cannot_write_with_an_owner_that_lacks_io_authority() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"normal") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    let restricted=asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
    let owner={
        let _guard=restricted.set_current_restricted();
        AgentCx::for_current_or_request()
    };
    let budget=Budget { owner,started:Instant::now(),timeout:Duration::from_secs(5) };
    assert!(!budget.owner.capabilities().io);
    let error=rt.block_on(tool.select_completion(&id,true,None,&budget)).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_PERMISSION"));
    assert_eq!(source(temp.path()),SOURCE);
}

fn with_values(id: &str, apply: bool, values: Value) -> Value {
    let mut input=select(id,apply);
    input["snippetValues"]=values;
    input
}

#[test]
fn snippet_defaults_preview_and_explicit_values_apply_without_mutating_the_candidate() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"snippet") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    let preview=run(&tool,&rt,select(&id,false)).unwrap().details.unwrap();
    assert_eq!(preview["canApply"],true);
    assert_eq!(preview["edits"][0]["newText"],"Type(argument)");
    let chosen=run(&tool,&rt,with_values(&id,false,json!({"1":"input"}))).unwrap().details.unwrap();
    assert_eq!(chosen["edits"][0]["newText"],"Type(input)");
    assert_eq!(source(temp.path()),SOURCE);
    let again=run(&tool,&rt,select(&id,false)).unwrap().details.unwrap();
    assert_eq!(again["edits"][0]["newText"],"Type(argument)");
    run(&tool,&rt,with_values(&id,true,json!({"1":"different"}))).unwrap();
    assert_eq!(source(temp.path()),"// header\nfn main() { Type(different) }\n");
}

#[test]
fn snippet_required_fields_cannot_be_erased_accidentally_or_inherited_from_preview() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"snippet_required") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    let preview=run(&tool,&rt,select(&id,false)).unwrap().details.unwrap();
    assert_eq!(preview["canApply"],false);
    assert_eq!(preview["missingPlaceholders"],json!([1,2]));
    let supplied=json!({"1":"first","2":"second"});
    assert_eq!(run(&tool,&rt,with_values(&id,false,supplied.clone())).unwrap().details.unwrap()["canApply"],true);
    assert!(run(&tool,&rt,select(&id,true)).unwrap_err().to_string().contains("VALUES_REQUIRED"));
    assert_eq!(source(temp.path()),SOURCE);
    run(&tool,&rt,with_values(&id,true,supplied)).unwrap();
    assert_eq!(source(temp.path()),"// header\nfn main() { Type(first, second) }\n");
}

#[test]
fn snippet_mirrors_insert_literal_unicode_values_without_evaluation() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"snippet_mirror") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    let value="${2:literal} \\ 😀 $(not_a_command)";
    let output=run(&tool,&rt,with_values(&id,true,json!({"1":value}))).unwrap().details.unwrap();
    assert_eq!(output["commandExecuted"],false);
    assert_eq!(source(temp.path()),format!("// header\nfn main() {{ Type({value}, {value}) }}\n"));
    assert!(!events(temp.path()).iter().any(|event|event["method"]=="workspace/executeCommand"));
}

#[test]
fn lazy_snippet_auto_imports_remain_plain_and_share_the_existing_transaction() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"snippet_lazy") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    let input=json!({"1":"input"});
    let preview=run(&tool,&rt,with_values(&id,false,input.clone())).unwrap().details.unwrap();
    assert_eq!(preview["additionalEdits"],1);
    assert_eq!(preview["edits"][0]["newText"],"Type(input, input)");
    assert_eq!(preview["edits"][1]["newText"],"use example::Type;\n// literal $0 ${1:keep}\n");
    run(&tool,&rt,with_values(&id,true,input)).unwrap();
    assert_eq!(source(temp.path()),"use example::Type;\n// literal $0 ${1:keep}\n// header\nfn main() { Type(input, input) }\n");
    assert_eq!(events(temp.path()).iter().filter(|event|event["method"]=="completionItem/resolve").count(),1);
}

#[test]
fn snippet_choices_nested_defaults_and_list_defaults_survive_public_dispatch() {
    for (mode,values,expected) in [
        ("snippet_choice",json!({"1":"green"}),"// header\nfn main() { Type(green) }\n"),
        ("snippet_nested",json!({"2":"chosen"}),"// header\nfn main() { Type(outer(chosen)) }\n"),
        ("snippet_defaults",json!({"1":"chosen"}),"use example::Type;\n// header\nfn main() { Type(chosen) }\n"),
    ] {
        let temp=tempfile::tempdir().unwrap();
        let Some(tool)=fixture(temp.path(),mode) else { return };
        let rt=runtime(); let id=list(&tool,&rt);
        run(&tool,&rt,with_values(&id,true,values)).unwrap();
        assert_eq!(source(temp.path()),expected,"{mode}");
    }
}

#[test]
fn unsupported_or_malformed_snippets_never_apply_partial_insertions() {
    for mode in ["snippet_variable","snippet_transform","snippet_malformed","snippet_conflict"] {
        let temp=tempfile::tempdir().unwrap();
        let Some(tool)=fixture(temp.path(),mode) else { return };
        let rt=runtime(); let id=list(&tool,&rt);
        assert_eq!(run(&tool,&rt,select(&id,false)).unwrap().details.unwrap()["canApply"],false);
        assert!(run(&tool,&rt,with_values(&id,true,json!({"1":"override"}))).is_err());
        assert_eq!(source(temp.path()),SOURCE,"{mode}");
    }
}

#[test]
fn snippet_value_usage_errors_do_not_consume_valid_handles() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"snippet") else { return };
    let rt=runtime();
    let mut listing=request(); listing["snippetValues"]=json!({"1":"bad"});
    assert!(run(&tool,&rt,listing).is_err());
    assert!(tool.registry.status().is_empty());
    assert!(run(&tool,&rt,json!({"action":"status","snippetValues":{}})).is_err());
    let id=list(&tool,&rt);
    for values in [json!({"01":"bad"}),json!({"2":"unknown"}),json!({"1":42}),json!({"1":"x".repeat(16385)})] {
        assert!(run(&tool,&rt,with_values(&id,true,values)).is_err());
        assert_eq!(source(temp.path()),SOURCE);
    }
    run(&tool,&rt,with_values(&id,true,json!({"1":"valid"}))).unwrap();
    assert_eq!(source(temp.path()),"// header\nfn main() { Type(valid) }\n");
}

#[test]
fn source_drift_after_snippet_preview_prevents_both_insertion_and_auto_imports() {
    let temp=tempfile::tempdir().unwrap();
    let Some(tool)=fixture(temp.path(),"snippet_lazy") else { return };
    let rt=runtime(); let id=list(&tool,&rt);
    run(&tool,&rt,with_values(&id,false,json!({"1":"value"}))).unwrap();
    std::fs::write(temp.path().join("source.picomp"),"external\n").unwrap();
    assert!(run(&tool,&rt,with_values(&id,true,json!({"1":"value"}))).unwrap_err().to_string().contains("STALE"));
    assert_eq!(source(temp.path()),"external\n");
}
