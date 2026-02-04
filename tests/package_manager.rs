#![allow(clippy::too_many_lines)]

mod common;

use asupersync::runtime::RuntimeBuilder;
use common::TestHarness;
use pi::package_manager::{
    PackageManager, PackageScope, ResolveExtensionSourcesOptions, ResolveRoots, ResolvedResource,
    ResourceOrigin,
};
use std::path::{Path, PathBuf};

fn write_json(path: &Path, value: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).expect("serialize json"),
    )
    .expect("write json");
}

fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");
    runtime.block_on(future)
}

fn log_resolved(harness: &TestHarness, label: &str, items: &[ResolvedResource]) {
    harness
        .log()
        .info_ctx("resolved", format!("Resolved {label}"), |ctx| {
            ctx.push(("count".into(), items.len().to_string()));
            for (idx, item) in items.iter().enumerate() {
                ctx.push((
                    format!("{label}[{idx}]"),
                    format!(
                        "enabled={} origin={:?} scope={:?} path={} source={}",
                        item.enabled,
                        item.metadata.origin,
                        item.metadata.scope,
                        item.path.display(),
                        item.metadata.source
                    ),
                ));
            }
        });
}

#[test]
fn package_identity_normalizes_npm_git_and_local_sources() {
    let harness = TestHarness::new("package_identity_normalizes_npm_git_and_local_sources");

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    harness.section("npm");
    for (source, expected) in [
        ("npm:react@18.2.0", "npm:react"),
        ("npm:@types/node@20.0.0", "npm:@types/node"),
        ("npm:lodash", "npm:lodash"),
    ] {
        harness.log().info_ctx("case", "package_identity", |ctx| {
            ctx.push(("source".into(), source.to_string()));
        });
        let identity = manager.package_identity(source);
        harness.log().info_ctx("result", "identity", |ctx| {
            ctx.push(("identity".into(), identity.clone()));
        });
        assert_eq!(identity, expected);
    }

    harness.section("git");
    for source in [
        "git:https://github.com/example-org/example-repo.git@main",
        "https://github.com/example-org/example-repo@main",
        "github.com/example-org/example-repo@main",
    ] {
        let identity = manager.package_identity(source);
        harness.log().info_ctx("case", "package_identity", |ctx| {
            ctx.push(("source".into(), source.to_string()));
            ctx.push(("identity".into(), identity.clone()));
        });
        assert_eq!(identity, "git:github.com/example-org/example-repo");
    }

    harness.section("local");
    let local = manager.package_identity("./a/../b/./pkg");
    harness.log().info_ctx("case", "package_identity", |ctx| {
        ctx.push(("source".into(), "./a/../b/./pkg".to_string()));
        ctx.push(("identity".into(), local.clone()));
    });

    let local_path = local
        .strip_prefix("local:")
        .map(PathBuf::from)
        .expect("local identity prefix");
    assert_eq!(local_path, cwd.join("b").join("pkg"));
}

#[test]
fn installed_path_resolves_project_and_user_scopes_without_external_commands() {
    let harness = TestHarness::new(
        "installed_path_resolves_project_and_user_scopes_without_external_commands",
    );

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    harness.section("npm project");
    let npm_project = run_async(manager.installed_path("npm:react@18.2.0", PackageScope::Project))
        .expect("npm installed_path");
    let npm_project = npm_project.expect("npm returns Some(path)");
    harness
        .log()
        .info_ctx("installed_path", "npm project", |ctx| {
            ctx.push(("path".into(), npm_project.display().to_string()));
        });
    assert_eq!(
        npm_project,
        cwd.join(".pi")
            .join("npm")
            .join("node_modules")
            .join("react")
    );

    harness.section("git project + user");
    let git_source = "git:https://github.com/example-org/example-repo@main";

    let git_project = run_async(manager.installed_path(git_source, PackageScope::Project))
        .expect("git project installed_path")
        .expect("git project returns Some(path)");
    harness
        .log()
        .info_ctx("installed_path", "git project", |ctx| {
            ctx.push(("path".into(), git_project.display().to_string()));
        });
    assert_eq!(
        git_project,
        cwd.join(".pi")
            .join("git")
            .join("github.com")
            .join("example-org")
            .join("example-repo")
    );

    let git_user = run_async(manager.installed_path(git_source, PackageScope::User))
        .expect("git user installed_path")
        .expect("git user returns Some(path)");
    harness.log().info_ctx("installed_path", "git user", |ctx| {
        ctx.push(("path".into(), git_user.display().to_string()));
    });
    let expected_suffix = Path::new(".pi")
        .join("agent")
        .join("git")
        .join("github.com")
        .join("example-org")
        .join("example-repo");
    assert!(git_user.ends_with(&expected_suffix));

    harness.section("local");
    let local_path = run_async(manager.installed_path("./x/../y/thing", PackageScope::Project))
        .expect("local installed_path")
        .expect("local returns Some(path)");
    harness.log().info_ctx("installed_path", "local", |ctx| {
        ctx.push(("path".into(), local_path.display().to_string()));
    });
    assert_eq!(local_path, cwd.join("y").join("thing"));
}

#[test]
fn resolve_with_roots_auto_discovery_ignores_parent_gitignore() {
    let harness = TestHarness::new("resolve_with_roots_auto_discovery_ignores_parent_gitignore");

    let cwd = harness.create_dir("cwd");
    std::fs::write(cwd.join(".gitignore"), ".pi\n").expect("write .gitignore");
    let manager = PackageManager::new(cwd.clone());

    let global_base_dir = harness.create_dir("global");
    let project_base_dir = cwd.join(".pi");
    std::fs::create_dir_all(&project_base_dir).expect("create project base dir");

    let global_settings_path = global_base_dir.join("settings.json");
    let project_settings_path = project_base_dir.join("settings.json");

    let extensions_dir = project_base_dir.join("extensions");
    std::fs::create_dir_all(&extensions_dir).expect("create extensions dir");
    let auto_ext = extensions_dir.join("auto.js");
    std::fs::write(&auto_ext, "export const x = 1;\n").expect("write auto extension");

    write_json(&global_settings_path, &serde_json::json!({}));
    write_json(&project_settings_path, &serde_json::json!({}));

    let roots = ResolveRoots {
        global_settings_path,
        project_settings_path,
        global_base_dir,
        project_base_dir,
    };

    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);
    let item = resolved
        .extensions
        .iter()
        .find(|r| r.path == auto_ext)
        .expect("auto extension present");
    assert!(item.enabled, "auto extension should still be discovered");
}

#[test]
fn resolve_with_roots_applies_auto_discovery_override_patterns() {
    let harness = TestHarness::new("resolve_with_roots_applies_auto_discovery_override_patterns");

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    let global_base_dir = harness.create_dir("global");
    let project_base_dir = cwd.join(".pi");
    std::fs::create_dir_all(&project_base_dir).expect("create project base dir");

    let global_settings_path = global_base_dir.join("settings.json");
    let project_settings_path = project_base_dir.join("settings.json");

    // Create one auto-discovered extension under the project base dir.
    let extensions_dir = project_base_dir.join("extensions");
    std::fs::create_dir_all(&extensions_dir).expect("create extensions dir");
    let auto_ext = extensions_dir.join("auto.js");
    std::fs::write(&auto_ext, "export const x = 1;\n").expect("write auto extension");

    let roots = ResolveRoots {
        global_settings_path: global_settings_path.clone(),
        project_settings_path: project_settings_path.clone(),
        global_base_dir,
        project_base_dir,
    };

    harness.section("default enabled (no overrides)");
    write_json(&global_settings_path, &serde_json::json!({}));
    write_json(&project_settings_path, &serde_json::json!({}));
    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);
    let item = resolved
        .extensions
        .iter()
        .find(|r| r.path == auto_ext)
        .expect("auto extension present");
    assert!(item.enabled, "auto extension should be enabled by default");
    assert_eq!(item.metadata.origin, ResourceOrigin::TopLevel);

    harness.section("excluded by '!auto.js'");
    write_json(
        &project_settings_path,
        &serde_json::json!({ "extensions": ["!auto.js"] }),
    );
    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);
    let item = resolved
        .extensions
        .iter()
        .find(|r| r.path == auto_ext)
        .expect("auto extension present");
    assert!(
        !item.enabled,
        "auto extension should be disabled by exclude"
    );

    harness.section("force include wins over exclude");
    write_json(
        &project_settings_path,
        &serde_json::json!({ "extensions": ["!auto.js", "+extensions/auto.js"] }),
    );
    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);
    let item = resolved
        .extensions
        .iter()
        .find(|r| r.path == auto_ext)
        .expect("auto extension present");
    assert!(
        item.enabled,
        "auto extension should be enabled by force include"
    );

    harness.section("force exclude overrides force include");
    write_json(
        &project_settings_path,
        &serde_json::json!({ "extensions": ["!auto.js", "+extensions/auto.js", "-extensions/auto.js"] }),
    );
    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);
    let item = resolved
        .extensions
        .iter()
        .find(|r| r.path == auto_ext)
        .expect("auto extension present");
    assert!(
        !item.enabled,
        "auto extension should be disabled by force exclude"
    );
}

#[test]
fn resolve_with_roots_applies_package_filters_and_prefers_project_package() {
    let harness =
        TestHarness::new("resolve_with_roots_applies_package_filters_and_prefers_project_package");

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    let global_base_dir = harness.create_dir("global");
    let project_base_dir = cwd.join(".pi");
    std::fs::create_dir_all(&project_base_dir).expect("create project base dir");

    let global_settings_path = global_base_dir.join("settings.json");
    let project_settings_path = project_base_dir.join("settings.json");

    // Local package root with conventional resource directories.
    let package_root = harness.create_dir("pkg");
    let pkg_extensions = package_root.join("extensions");
    let pkg_skills = package_root.join("skills").join("my-skill");
    let pkg_prompts = package_root.join("prompts");
    let pkg_themes = package_root.join("themes");
    std::fs::create_dir_all(&pkg_extensions).expect("create pkg extensions");
    std::fs::create_dir_all(&pkg_skills).expect("create pkg skills");
    std::fs::create_dir_all(&pkg_prompts).expect("create pkg prompts");
    std::fs::create_dir_all(&pkg_themes).expect("create pkg themes");

    let ext_file = pkg_extensions.join("ext.js");
    let skill_file = pkg_skills.join("SKILL.md");
    let prompt_file = pkg_prompts.join("p.md");
    let theme_file = pkg_themes.join("t.json");
    std::fs::write(&ext_file, "export const ok = true;\n").expect("write ext.js");
    std::fs::write(&skill_file, "# Skill\n").expect("write SKILL.md");
    std::fs::write(&prompt_file, "# Prompt\n").expect("write prompt");
    std::fs::write(&theme_file, "{ \"name\": \"t\" }\n").expect("write theme");

    // Global config disables all extensions from this package (empty filter list).
    write_json(
        &global_settings_path,
        &serde_json::json!({
            "packages": [
                {
                    "source": package_root.display().to_string(),
                    "extensions": []
                }
            ]
        }),
    );

    // Project config re-adds the same package, enabling just `ext.js`.
    write_json(
        &project_settings_path,
        &serde_json::json!({
            "packages": [
                {
                    "source": package_root.display().to_string(),
                    "extensions": ["ext.js"],
                    "skills": ["my-skill"],
                    "prompts": ["p.md"],
                    "themes": ["t.json"]
                }
            ]
        }),
    );

    let roots = ResolveRoots {
        global_settings_path,
        project_settings_path,
        global_base_dir,
        project_base_dir,
    };

    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);
    log_resolved(&harness, "skills", &resolved.skills);
    log_resolved(&harness, "prompts", &resolved.prompts);
    log_resolved(&harness, "themes", &resolved.themes);

    let ext = resolved
        .extensions
        .iter()
        .find(|r| r.path == ext_file)
        .expect("package extension present");
    harness
        .log()
        .info_ctx("assert", "extension enabled state", |ctx| {
            ctx.push(("path".into(), ext.path.display().to_string()));
            ctx.push(("enabled".into(), ext.enabled.to_string()));
            ctx.push(("scope".into(), format!("{:?}", ext.metadata.scope)));
            ctx.push(("origin".into(), format!("{:?}", ext.metadata.origin)));
        });
    assert!(ext.enabled, "project package filter should win over global");
    assert_eq!(ext.metadata.origin, ResourceOrigin::Package);
    assert_eq!(ext.metadata.scope, PackageScope::Project);

    let skill = resolved
        .skills
        .iter()
        .find(|r| r.path == skill_file)
        .expect("package skill present");
    assert!(skill.enabled);

    let prompt = resolved
        .prompts
        .iter()
        .find(|r| r.path == prompt_file)
        .expect("package prompt present");
    assert!(prompt.enabled);

    let theme = resolved
        .themes
        .iter()
        .find(|r| r.path == theme_file)
        .expect("package theme present");
    assert!(theme.enabled);
}

#[test]
fn resolve_extension_sources_dedupes_normalized_local_paths() {
    let harness = TestHarness::new("resolve_extension_sources_dedupes_normalized_local_paths");

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    let pkg_dir = cwd.join("pkg");
    std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
    let ext_path = pkg_dir.join("ext.js");
    std::fs::write(&ext_path, "export const ok = true;\n").expect("write ext.js");

    let sources = vec!["pkg/./ext.js".to_string(), "pkg/sub/../ext.js".to_string()];

    let resolved = run_async(manager.resolve_extension_sources(
        &sources,
        ResolveExtensionSourcesOptions {
            local: false,
            temporary: true,
        },
    ))
    .expect("resolve_extension_sources");

    log_resolved(&harness, "extensions", &resolved.extensions);

    let enabled = resolved
        .extensions
        .iter()
        .filter(|r| r.enabled)
        .collect::<Vec<_>>();
    assert_eq!(
        enabled.len(),
        1,
        "expected normalized duplicate paths to dedupe"
    );

    let item = enabled[0];
    assert_eq!(item.path, ext_path);
    assert_eq!(item.metadata.scope, PackageScope::Temporary);
    assert_eq!(item.metadata.origin, ResourceOrigin::Package);
    assert_eq!(
        item.metadata.base_dir.as_deref(),
        ext_path.parent(),
        "expected base_dir to point at extension's parent directory"
    );
}

#[test]
fn resolve_extension_sources_directory_uses_package_json_pi_manifest_entries() {
    let harness = TestHarness::new(
        "resolve_extension_sources_directory_uses_package_json_pi_manifest_entries",
    );

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    let package_root = cwd.join("pkg");
    let extensions_dir = package_root.join("extensions");
    std::fs::create_dir_all(&extensions_dir).expect("create extensions dir");
    let ext_file = extensions_dir.join("a.js");
    std::fs::write(&ext_file, "export const a = 1;\n").expect("write a.js");

    write_json(
        &package_root.join("package.json"),
        &serde_json::json!({
            "name": "pkg",
            "private": true,
            "pi": {
                "extensions": ["extensions/a.js"]
            }
        }),
    );

    let sources = vec![package_root.display().to_string()];
    let resolved = run_async(manager.resolve_extension_sources(
        &sources,
        ResolveExtensionSourcesOptions {
            local: false,
            temporary: true,
        },
    ))
    .expect("resolve_extension_sources");

    log_resolved(&harness, "extensions", &resolved.extensions);

    let item = resolved
        .extensions
        .iter()
        .find(|r| r.path == ext_file)
        .expect("manifest extension file present");
    assert!(item.enabled);
    assert_eq!(item.metadata.scope, PackageScope::Temporary);
    assert_eq!(
        item.metadata.base_dir.as_deref(),
        Some(package_root.as_path()),
        "expected base_dir to point at package root for manifest-driven entries"
    );
}

#[test]
fn resolve_extension_sources_directory_without_resources_falls_back_to_dir_entry() {
    let harness = TestHarness::new(
        "resolve_extension_sources_directory_without_resources_falls_back_to_dir_entry",
    );

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    let package_root = cwd.join("empty_pkg");
    std::fs::create_dir_all(&package_root).expect("create empty package root");

    let sources = vec![package_root.display().to_string()];
    let resolved = run_async(manager.resolve_extension_sources(
        &sources,
        ResolveExtensionSourcesOptions {
            local: false,
            temporary: true,
        },
    ))
    .expect("resolve_extension_sources");

    log_resolved(&harness, "extensions", &resolved.extensions);

    let item = resolved
        .extensions
        .iter()
        .find(|r| r.path == package_root)
        .expect("fallback directory extension entry present");
    assert!(item.enabled);
    assert_eq!(item.metadata.scope, PackageScope::Temporary);
    assert_eq!(
        item.metadata.base_dir.as_deref(),
        Some(package_root.as_path()),
        "expected base_dir to point at directory entry"
    );
}

#[test]
fn resolve_with_roots_auto_discovers_extension_directory_entries() {
    let harness = TestHarness::new("resolve_with_roots_auto_discovers_extension_directory_entries");

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    let global_base_dir = harness.create_dir("global");
    let project_base_dir = cwd.join(".pi");
    std::fs::create_dir_all(&project_base_dir).expect("create project base dir");

    let global_settings_path = global_base_dir.join("settings.json");
    let project_settings_path = project_base_dir.join("settings.json");
    write_json(&global_settings_path, &serde_json::json!({}));
    write_json(&project_settings_path, &serde_json::json!({}));

    let roots = ResolveRoots {
        global_settings_path,
        project_settings_path,
        global_base_dir,
        project_base_dir: project_base_dir.clone(),
    };

    let extensions_dir = project_base_dir.join("extensions");
    let pkg_dir = extensions_dir.join("demo_pkg");
    std::fs::create_dir_all(&pkg_dir).expect("create demo_pkg dir");

    let manifest_ext = pkg_dir.join("custom.js");
    let index_js = pkg_dir.join("index.js");
    std::fs::write(&manifest_ext, "export const x = 1;\n").expect("write custom.js");
    std::fs::write(&index_js, "export const y = 2;\n").expect("write index.js");

    write_json(
        &pkg_dir.join("package.json"),
        &serde_json::json!({
            "name": "demo_pkg",
            "private": true,
            "pi": {
                "extensions": ["custom.js"]
            }
        }),
    );

    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);

    assert!(
        resolved.extensions.iter().any(|r| r.path == manifest_ext),
        "expected manifest-listed extension to be auto-discovered"
    );
    assert!(
        !resolved.extensions.iter().any(|r| r.path == index_js),
        "expected manifest entries to take precedence over index.js fallback"
    );
}

#[test]
fn resolve_with_roots_auto_discovers_extension_directory_index_fallback() {
    let harness =
        TestHarness::new("resolve_with_roots_auto_discovers_extension_directory_index_fallback");

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    let global_base_dir = harness.create_dir("global");
    let project_base_dir = cwd.join(".pi");
    std::fs::create_dir_all(&project_base_dir).expect("create project base dir");

    let global_settings_path = global_base_dir.join("settings.json");
    let project_settings_path = project_base_dir.join("settings.json");
    write_json(&global_settings_path, &serde_json::json!({}));
    write_json(&project_settings_path, &serde_json::json!({}));

    let roots = ResolveRoots {
        global_settings_path,
        project_settings_path,
        global_base_dir,
        project_base_dir: project_base_dir.clone(),
    };

    let extensions_dir = project_base_dir.join("extensions");
    let pkg_dir = extensions_dir.join("index_pkg");
    std::fs::create_dir_all(&pkg_dir).expect("create index_pkg dir");

    let index_js = pkg_dir.join("index.js");
    std::fs::write(&index_js, "export const ok = true;\n").expect("write index.js");

    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);

    assert!(
        resolved.extensions.iter().any(|r| r.path == index_js),
        "expected index.js fallback to be auto-discovered for extension directory"
    );
}

#[cfg(unix)]
#[test]
fn resolve_with_roots_auto_discovery_follows_symlink_extension_dirs() {
    use std::os::unix::fs::symlink;

    let harness =
        TestHarness::new("resolve_with_roots_auto_discovery_follows_symlink_extension_dirs");

    let cwd = harness.create_dir("cwd");
    let manager = PackageManager::new(cwd.clone());

    let global_base_dir = harness.create_dir("global");
    let project_base_dir = cwd.join(".pi");
    std::fs::create_dir_all(&project_base_dir).expect("create project base dir");

    let global_settings_path = global_base_dir.join("settings.json");
    let project_settings_path = project_base_dir.join("settings.json");
    write_json(&global_settings_path, &serde_json::json!({}));
    write_json(&project_settings_path, &serde_json::json!({}));

    let roots = ResolveRoots {
        global_settings_path,
        project_settings_path,
        global_base_dir,
        project_base_dir: project_base_dir.clone(),
    };

    let real_pkg_dir = harness.create_dir("real_pkg");
    let real_index = real_pkg_dir.join("index.js");
    std::fs::write(&real_index, "export const ok = true;\n").expect("write index.js");

    let extensions_dir = project_base_dir.join("extensions");
    std::fs::create_dir_all(&extensions_dir).expect("create extensions dir");
    let link_dir = extensions_dir.join("linked_pkg");
    symlink(&real_pkg_dir, &link_dir).expect("create symlink");

    let linked_index = link_dir.join("index.js");
    assert!(
        linked_index.exists(),
        "expected index.js to exist via symlink"
    );

    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);

    assert!(
        resolved.extensions.iter().any(|r| r.path == linked_index),
        "expected symlinked extension directory to be auto-discovered"
    );
}

fn fixture_source_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("resource_loader")
        .join(name)
}

fn copy_fixture_dir(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).expect("create fixture dest");
    let entries = std::fs::read_dir(src).expect("read fixture dir");
    for entry in entries {
        let entry = entry.expect("fixture entry");
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let meta = entry.metadata().expect("fixture metadata");
        if meta.is_dir() {
            copy_fixture_dir(&src_path, &dest_path);
        } else if meta.is_file() {
            std::fs::copy(&src_path, &dest_path).expect("copy fixture file");
        }
    }
}

fn assert_paths_sorted(label: &str, items: &[ResolvedResource]) {
    let mut prev: Option<String> = None;
    for item in items {
        let path = item.path.to_string_lossy().to_string();
        if let Some(prev) = prev.as_ref() {
            assert!(
                prev <= &path,
                "{label} paths not sorted: prev={prev} next={path}"
            );
        }
        prev = Some(path);
    }
}

#[test]
fn resolve_with_roots_fixture_project_package_overrides_global_and_filters_resources() {
    let harness = TestHarness::new(
        "resolve_with_roots_fixture_project_package_overrides_global_and_filters_resources",
    );

    let src_root = fixture_source_dir("resolve_basic");
    let dest_root = harness.temp_path("fixture");
    copy_fixture_dir(&src_root, &dest_root);

    let cwd = dest_root.join("project");
    let manager = PackageManager::new(cwd.clone());

    let global_base_dir = dest_root.join("global");
    let project_base_dir = cwd.join(".pi");

    let roots = ResolveRoots {
        global_settings_path: global_base_dir.join("settings.json"),
        project_settings_path: project_base_dir.join("settings.json"),
        global_base_dir: global_base_dir.clone(),
        project_base_dir: project_base_dir.clone(),
    };

    let resolved = run_async(manager.resolve_with_roots(&roots)).expect("resolve_with_roots");
    log_resolved(&harness, "extensions", &resolved.extensions);
    log_resolved(&harness, "skills", &resolved.skills);
    log_resolved(&harness, "prompts", &resolved.prompts);
    log_resolved(&harness, "themes", &resolved.themes);

    let pkg_root = cwd.join("packages").join("pkg_shared");
    let pkg_ext_enabled = pkg_root.join("extensions").join("pkg_ext.js");
    let pkg_ext_disabled = pkg_root.join("extensions").join("other.js");
    let pkg_prompt_enabled = pkg_root.join("prompts").join("pkg_prompt.md");
    let pkg_prompt_disabled = pkg_root.join("prompts").join("other.md");
    let pkg_skill_enabled = pkg_root.join("skills").join("pkg-skill").join("SKILL.md");
    let pkg_skill_disabled = pkg_root.join("skills").join("other-skill").join("SKILL.md");
    let pkg_theme_enabled = pkg_root.join("themes").join("pkg_theme.json");
    let pkg_theme_disabled = pkg_root.join("themes").join("other_theme.json");

    for (path, expected_enabled, kind) in [
        (pkg_ext_enabled, true, "extension"),
        (pkg_ext_disabled, false, "extension"),
        (pkg_prompt_enabled, true, "prompt"),
        (pkg_prompt_disabled, false, "prompt"),
        (pkg_skill_enabled, true, "skill"),
        (pkg_skill_disabled, false, "skill"),
        (pkg_theme_enabled, true, "theme"),
        (pkg_theme_disabled, false, "theme"),
    ] {
        let resolved = match kind {
            "extension" => resolved
                .extensions
                .iter()
                .find(|r| r.path == path)
                .expect("package extension entry missing"),
            "prompt" => resolved
                .prompts
                .iter()
                .find(|r| r.path == path)
                .expect("package prompt entry missing"),
            "skill" => resolved
                .skills
                .iter()
                .find(|r| r.path == path)
                .expect("package skill entry missing"),
            "theme" => resolved
                .themes
                .iter()
                .find(|r| r.path == path)
                .expect("package theme entry missing"),
            _ => unreachable!("unexpected kind"),
        };

        assert_eq!(resolved.enabled, expected_enabled);
        assert_eq!(resolved.metadata.origin, ResourceOrigin::Package);
        assert_eq!(resolved.metadata.scope, PackageScope::Project);
        assert_eq!(resolved.metadata.base_dir.as_ref(), Some(&pkg_root));
        assert_eq!(resolved.metadata.source, "./packages/pkg_shared");
    }

    let global_prompts = resolved
        .prompts
        .iter()
        .filter(|r| r.metadata.origin == ResourceOrigin::TopLevel)
        .filter(|r| r.metadata.scope == PackageScope::User)
        .cloned()
        .collect::<Vec<_>>();
    assert_paths_sorted("global prompts", &global_prompts);

    let project_prompts = resolved
        .prompts
        .iter()
        .filter(|r| r.metadata.origin == ResourceOrigin::TopLevel)
        .filter(|r| r.metadata.scope == PackageScope::Project)
        .cloned()
        .collect::<Vec<_>>();
    assert_paths_sorted("project prompts", &project_prompts);
}
