use super::*;
use crate::eval::js_kernel::{JsCellResponse, JsKernel};
use std::time::Duration;

fn cell(kernel: &JsKernel, code: &str) -> JsCellResponse {
    kernel
        .submit(code.into(), Instant::now() + Duration::from_secs(5))
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .expect("data-module cell must terminate")
}

fn value(kernel: &JsKernel, code: &str) -> String {
    let result = cell(kernel, code);
    assert!(result.ok, "{:?}", result.error);
    result.result.expect("expression must return a value")
}

fn files(root: &Path) -> ModuleFiles {
    let files = ModuleFiles::new(root.to_path_buf(), Arc::new(AtomicBool::new(false)));
    files.begin(
        AgentCx::for_testing_with_io(),
        Instant::now() + Duration::from_secs(5),
    );
    files
}

#[test]
fn json_defaults_work_through_dynamic_static_and_typed_imports() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("data.json"),
        "\u{feff}{\"answer\":42,\"items\":[true,\"雪\"]}",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("facade.mjs"),
        "import data from './data.json' with {type:'json'}; export {data};",
    )
    .unwrap();
    std::fs::write(dir.path().join("facade.mts"),
        "import data from './data.json' with {type:'json'}; const answer: number = data.answer; export {answer};").unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    assert_eq!(
        value(
            &kernel,
            "globalThis.data = (await import('./data.json', {with:{type:'json'}})).default; [data.answer, ...data.items]"
        ),
        "[42,true,\"雪\"]"
    );
    assert_eq!(
        value(&kernel, "(await import('./facade.mjs')).data === data"),
        "true"
    );
    assert_eq!(
        value(&kernel, "(await import('./facade.mts')).answer"),
        "42"
    );
    assert_eq!(
        value(
            &kernel,
            "Object.keys(await import('./data.json', {with:{type:'json'}}))"
        ),
        "[\"default\"]"
    );
}

#[test]
fn json_cache_retains_shared_identity_but_a_fresh_realm_reads_new_data() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"), "{\"answer\":1}").unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    value(
        &kernel,
        "globalThis.first = (await import('./data.json', {with:{type:'json'}})).default; first.answer = 42",
    );
    std::fs::write(dir.path().join("data.json"), "{\"answer\":99}").unwrap();
    assert_eq!(
        value(
            &kernel,
            "globalThis.same = (await import('././data.json', {with:{type:'json'}})).default; [first === same, same.answer]"
        ),
        "[true,42]"
    );
    let fresh = JsKernel::spawn_in(dir.path()).unwrap();
    assert_eq!(
        value(
            &fresh,
            "(await import('./data.json', {with:{type:'json'}})).default.answer"
        ),
        "99"
    );
}

#[test]
fn native_parser_preserves_json_semantics_without_guest_global_helpers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"),
        r#"{"z":0,"__proto__":{"private":true},"a":1,"a":2,"zero":-0,"jsonTrap":3,"01":"first","10":"last"}"#).unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    let result = value(
        &kernel,
        r"
        globalThis.originalParse = JSON.parse;
        JSON.parse = () => { globalThis.usedParse = true; throw new Error('guest parser'); };
        Object.defineProperty(Object.prototype, 'jsonTrap', {set() { globalThis.usedSetter = true; }, configurable:true});
        globalThis.data = (await import('./data.json', {with:{type:'json'}})).default;
        globalThis.observed = [Object.keys(data).join(','), Object.hasOwn(data, '__proto__'),
            Object.getPrototypeOf(data) === Object.prototype, data.__proto__.private,
            Object.is(data.zero, -0), data.a, data.jsonTrap, typeof usedParse, typeof usedSetter];
        delete Object.prototype.jsonTrap; JSON.parse = originalParse; observed
    ",
    );
    assert_eq!(
        result,
        "[\"10,z,__proto__,a,zero,jsonTrap,01\",true,true,true,true,2,3,\"undefined\",\"undefined\"]"
    );
}

#[test]
fn json_attribute_contract_is_checked_even_for_cached_modules() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"), "42").unwrap();
    std::fs::write(dir.path().join("code.mjs"), "export default 42;").unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    for prefix in [false, true] {
        if prefix {
            assert_eq!(
                value(
                    &kernel,
                    "(await import('./data.json', {with:{type:'json'}})).default"
                ),
                "42"
            );
        }
        for code in [
            "await import('./data.json')",
            "await import('./data.json', {with:{}})",
            "await import('./data.json', {with:{type:'javascript'}})",
            "await import('./data.json', {with:{type:'json', extra:'no'}})",
            "await import('./code.mjs', {with:{type:'json'}})",
        ] {
            let result = cell(&kernel, code);
            assert!(!result.ok, "accepted {code}");
            assert!(result.error.unwrap().contains("EVAL_MODULE_ATTRIBUTES"));
        }
    }
}

#[test]
fn invalid_data_never_executes_a_prefix_or_echoes_its_contents() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    for (index, source) in [
        "globalThis.executed = true; // PRIVATE-CANARY",
        "{\"PRIVATE-CANARY\":}",
        "null; globalThis.executed = true;",
        "[1,]",
        "\0PRIVATE-CANARY",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("invalid-{index}.json");
        std::fs::write(dir.path().join(&name), source).unwrap();
        let result = cell(
            &kernel,
            &format!("await import('./{name}', {{with:{{type:'json'}}}})"),
        );
        assert!(!result.ok);
        let error = result.error.unwrap();
        assert!(error.contains("EVAL_MODULE_JSON"), "{error}");
        assert!(!error.contains("PRIVATE-CANARY"));
    }
    assert_eq!(value(&kernel, "typeof executed"), "\"undefined\"");
}

#[test]
fn scalar_json_values_are_real_default_exports_including_null() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    for (index, source) in ["null", "true", "0", "\"雪\"", "[1,2]"].iter().enumerate() {
        let name = format!("scalar-{index}.json");
        std::fs::write(dir.path().join(&name), source).unwrap();
        assert_eq!(
            value(
                &kernel,
                &format!("(await import('./{name}', {{with:{{type:'json'}}}})).default")
            ),
            *source
        );
    }
}

#[test]
fn json_loads_share_existing_byte_budgets_and_current_owner_admission() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"), "{\"value\":42}").unwrap();
    let files = files(dir.path());
    let name = files.resolve_path("eval_script", "./data.json").unwrap();
    files.0.borrow_mut().source_bytes = MAX_SOURCE_BYTES - 1;
    assert!(files.read_module(&name).err().unwrap().contains("LIMIT"));
    files.0.borrow_mut().source_bytes = 0;
    files.begin(
        AgentCx::for_testing(),
        Instant::now() + Duration::from_secs(5),
    );
    assert!(files.read_module(&name).err().unwrap().contains("DENIED"));
    files.begin(AgentCx::for_testing_with_io(), Instant::now());
    assert!(files.read_module(&name).err().unwrap().contains("TIMEOUT"));
    files.begin(
        AgentCx::for_testing_with_io(),
        Instant::now() + Duration::from_secs(5),
    );
    std::fs::File::create(&name)
        .unwrap()
        .set_len(2 * 1024 * 1024 + 1)
        .unwrap();
    assert!(files.read_module(&name).err().unwrap().contains("LIMIT"));
}

#[cfg(unix)]
#[test]
fn aliases_cannot_cross_workspace_or_data_code_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("data.json"), "42").unwrap();
    std::fs::write(root.join("code.mjs"), "export default 42;").unwrap();
    std::fs::write(dir.path().join("outside.json"), "0").unwrap();
    for (target, alias) in [
        ("data.json", "alias.json"),
        ("data.json", "alias.mjs"),
        ("code.mjs", "code.json"),
        ("../outside.json", "escape.json"),
    ] {
        std::os::unix::fs::symlink(target, root.join(alias)).unwrap();
    }
    let files = files(&root);
    assert_eq!(
        files.resolve_path("eval_script", "./data.json").unwrap(),
        files.resolve_path("eval_script", "./alias.json").unwrap()
    );
    for name in ["./alias.mjs", "./code.json", "./escape.json"] {
        assert!(
            files.resolve_path("eval_script", name).is_err(),
            "accepted {name}"
        );
    }
}

#[test]
fn text_defaults_preserve_exact_content_without_guest_constructors_or_execution() {
    let dir = tempfile::tempdir().unwrap();
    let source = "\u{feff}# Heading\r\n雪\0'\"` ${globalThis.executed = true}\n<script>globalThis.executed = true</script>\n";
    std::fs::write(dir.path().join("template.md"), source).unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    let result = value(
        &kernel,
        r"
        globalThis.originalString = String;
        String = () => { globalThis.usedString = true; throw new Error('guest String'); };
        globalThis.asset = (await import('./template.md', {with:{type:'text'}})).default;
        String = originalString; [asset, typeof executed, typeof usedString]
    ",
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result).unwrap(),
        serde_json::json!([source, "undefined", "undefined"])
    );
    assert_eq!(
        value(
            &kernel,
            "Object.keys(await import('./template.md', {with:{type:'text'}}))"
        ),
        "[\"default\"]"
    );
}

#[test]
fn supported_text_suffixes_are_inert_data_and_unknown_suffixes_remain_refused() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    for (index, extension) in [
        "txt", "md", "csv", "tsv", "svg", "html", "css", "xml", "yaml", "yml", "toml", "jsonl",
        "ndjson",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("asset.{extension}");
        let data =
            format!("not necessarily valid {extension}; globalThis.executed = true; #{index}");
        std::fs::write(dir.path().join(&name), &data).unwrap();
        let result = value(
            &kernel,
            &format!("(await import('./{name}', {{with:{{type:'text'}}}})).default"),
        );
        assert_eq!(serde_json::from_str::<String>(&result).unwrap(), data);
    }
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();
    assert_eq!(
        value(
            &kernel,
            "(await import('./empty.txt', {with:{type:'text'}})).default"
        ),
        "\"\""
    );
    for name in ["unknown.bin", "extensionless", "code.cjs"] {
        std::fs::write(dir.path().join(name), "data").unwrap();
        let result = cell(
            &kernel,
            &format!("await import('./{name}', {{with:{{type:'text'}}}})"),
        );
        assert!(!result.ok, "accepted {name}");
    }
    assert_eq!(value(&kernel, "typeof executed"), "\"undefined\"");
}

#[test]
fn text_attributes_remain_mandatory_after_cache_and_transitive_imports() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("asset.txt"), "retained").unwrap();
    std::fs::write(
        dir.path().join("facade.mjs"),
        "import asset from './asset.txt' with {type:'text'}; export {asset};",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    assert_eq!(
        value(&kernel, "(await import('./facade.mjs')).asset"),
        "\"retained\""
    );
    value(
        &kernel,
        "globalThis.assetModule = await import('./asset.txt', {with:{type:'text'}}); assetModule.default",
    );
    std::fs::write(dir.path().join("asset.txt"), "edited").unwrap();
    assert_eq!(
        value(
            &kernel,
            "globalThis.sameAsset = await import('././asset.txt', {with:{type:'text'}}); [assetModule === sameAsset, sameAsset.default]"
        ),
        "[true,\"retained\"]"
    );
    for code in [
        "await import('./asset.txt')",
        "await import('./asset.txt', {with:{}})",
        "await import('./asset.txt', {with:{type:'json'}})",
        "await import('./asset.txt', {with:{type:'text', extra:'no'}})",
        "await import('./facade.mjs', {with:{type:'text'}})",
    ] {
        let result = cell(&kernel, code);
        assert!(!result.ok, "accepted {code}");
        assert!(result.error.unwrap().contains("EVAL_MODULE_ATTRIBUTES"));
    }
    let fresh = JsKernel::spawn_in(dir.path()).unwrap();
    assert_eq!(
        value(
            &fresh,
            "(await import('./asset.txt', {with:{type:'text'}})).default"
        ),
        "\"edited\""
    );
}

#[test]
fn text_file_failures_and_shared_source_budget_never_return_partial_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("asset.txt");
    let source = "\u{feff}text\r\n";
    std::fs::write(&path, source).unwrap();
    let files = files(dir.path());
    let name = files.resolve_path("eval_script", "./asset.txt").unwrap();
    files.0.borrow_mut().source_bytes = MAX_SOURCE_BYTES - source.len();
    assert_eq!(files.read_module(&name).unwrap().code, source);
    assert_eq!(files.0.borrow().source_bytes, MAX_SOURCE_BYTES);
    assert!(files.read_module(&name).err().unwrap().contains("LIMIT"));
    files.0.borrow_mut().source_bytes = 0;
    std::fs::write(&path, [0xff, 0xfe]).unwrap();
    assert!(files.read_module(&name).err().unwrap().contains("ENCODING"));
    std::fs::File::create(&path)
        .unwrap()
        .set_len(2 * 1024 * 1024 + 1)
        .unwrap();
    assert!(files.read_module(&name).err().unwrap().contains("LIMIT"));
    std::fs::create_dir(dir.path().join("directory.txt")).unwrap();
    let directory = files
        .resolve_path("eval_script", "./directory.txt")
        .unwrap();
    assert!(
        files
            .read_module(&directory)
            .err()
            .unwrap()
            .contains("PATH")
    );
    assert_eq!(files.0.borrow().source_bytes, 0);
}

#[cfg(unix)]
#[test]
fn text_aliases_and_special_files_obey_the_shared_data_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("text.txt"), "42").unwrap();
    std::fs::write(root.join("data.json"), "42").unwrap();
    std::fs::write(root.join("code.mjs"), "export default 42;").unwrap();
    std::fs::write(dir.path().join("outside.txt"), "outside").unwrap();
    for (target, alias) in [
        ("text.txt", "alias.md"),
        ("text.txt", "alias.json"),
        ("text.txt", "alias.mjs"),
        ("data.json", "data.txt"),
        ("code.mjs", "code.txt"),
        ("../outside.txt", "escape.txt"),
    ] {
        std::os::unix::fs::symlink(target, root.join(alias)).unwrap();
    }
    let files = files(&root);
    assert_eq!(
        files.resolve_path("eval_script", "./text.txt").unwrap(),
        files.resolve_path("eval_script", "./alias.md").unwrap()
    );
    for name in [
        "./alias.json",
        "./alias.mjs",
        "./data.txt",
        "./code.txt",
        "./escape.txt",
    ] {
        assert!(
            files.resolve_path("eval_script", name).is_err(),
            "accepted {name}"
        );
    }
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        root.join("pipe.txt"),
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    let name = files.resolve_path("eval_script", "./pipe.txt").unwrap();
    assert!(files.read_module(&name).err().unwrap().contains("PATH"));
}

#[test]
fn cached_text_resolution_requires_the_current_cells_io_authority() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("asset.txt"), "retained").unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    assert_eq!(
        value(
            &kernel,
            "(await import('./asset.txt', {with:{type:'text'}})).default"
        ),
        "\"retained\""
    );
    let receiver = {
        let owner = AgentCx::for_testing();
        let _guard = owner.cx().clone().set_current_restricted();
        kernel
            .submit(
                "await import('./asset.txt', {with:{type:'text'}})".into(),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap()
    };
    let result = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(!result.ok);
    assert!(result.error.unwrap().contains("EVAL_MODULE_DENIED"));
    assert_eq!(
        value(
            &kernel,
            "(await import('./asset.txt', {with:{type:'text'}})).default"
        ),
        "\"retained\""
    );
}
