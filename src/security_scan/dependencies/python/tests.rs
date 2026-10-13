use super::*;
use crate::security_scan::dependencies::{Inventory, canonical_name, canonical_version};
#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
use crate::security_scan::dependencies::inventory;

fn uv(body: &str) -> String {
    format!("version = 1\nrevision = 3\nrequires-python = '>=3.8'\n{body}")
}

fn poetry(body: &str) -> String {
    format!("{body}\n[metadata]\nlock-version = '2.1'\npython-versions = '>=3.8'\ncontent-hash = 'fixture'\n")
}

fn parse_inventory(text: &str, format: &str) -> Inventory {
    let mut builder = Builder::default();
    parse(text, &format!("{format}.lock"), format, &mut builder).unwrap();
    builder.finish(vec![])
}

#[test]
fn python_names_normalize_without_guessing_invalid_or_scoped_identities() {
    for raw in ["Friendly_Bard", "friendly..bard", "FRIENDLY-._.-BARD", "friendly-bard"] {
        assert_eq!(name(raw).as_deref(), Some("friendly-bard"));
    }
    assert_eq!(name("A").as_deref(), Some("a"));
    for raw in ["", "-name", "name.", "@scope/name", "a/b", "a b", "naïve", "a\n"] {
        assert!(name(raw).is_none(), "{raw:?}");
    }
    assert!(name(&"a".repeat(257)).is_none());
    assert!(canonical_name("unknown", "requests").is_none());
    assert_eq!(canonical_name("npm", "@scope/pkg").as_deref(), Some("@scope/pkg"));
}

#[test]
fn public_python_versions_preserve_epochs_prereleases_posts_and_development() {
    for (raw, expected) in [
        ("1", "1"), ("1.0", "1.0"), ("01.002.000", "1.2.0"),
        (" V01.2RC01 ", "1.2rc1"), ("0!1.0", "1.0"), ("002!1.0", "2!1.0"),
        ("1.1alpha", "1.1a0"), ("1.1beta_02", "1.1b2"),
        ("1.1preview3", "1.1rc3"), ("1.1pre", "1.1rc0"), ("1.1c4", "1.1rc4"),
        ("1.0-2", "1.0.post2"), ("1.0rev3", "1.0.post3"), ("1.0r", "1.0.post0"),
        ("1.0-post_02", "1.0.post2"), ("1.0.dev", "1.0.dev0"),
        ("1!2.3a4.post5.dev6", "1!2.3a4.post5.dev6"),
        ("2026.10", "2026.10"),
    ] {
        assert_eq!(version(raw).as_deref(), Some(expected), "{raw:?}");
        assert_eq!(version(expected).as_deref(), Some(expected), "idempotence");
    }
    let huge = format!("{}!1", "9".repeat(100));
    assert_eq!(version(&huge), Some(huge.clone()));
    assert_eq!(canonical_version("PyPI", "2.0rc1").as_deref(), Some("2.0rc1"));
    assert!(canonical_version("crates.io", "2.0rc1").is_none());
    assert_eq!(canonical_version("npm", "1.2.3+build").as_deref(), Some("1.2.3+build"));
}

#[test]
fn constraints_direct_references_and_local_builds_are_not_public_versions() {
    for raw in ["", "latest", "*", "1.*", ">=1", "==1.0", "1,2", "1.0+acme.1",
        "git+https://example.invalid/repo", "1.0-", "1.0.", "1..2", "1.0rc-1junk",
        "1.0\nPRIVATE-CANARY", "１.０", "1.0.dev1.post2", "1!", "v", "1 2"] {
        assert!(version(raw).is_none(), "{raw:?}");
    }
    assert!(version(&"1".repeat(257)).is_none());
    assert!(canonical_version("unknown", "1.0.0").is_none());
}

#[test]
fn public_index_requires_the_real_credential_free_pypi_endpoint() {
    for raw in ["https://pypi.org/simple", "https://pypi.org/simple/", "https://pypi.org:443/simple"] {
        assert!(public_index(raw), "{raw}");
    }
    for raw in ["http://pypi.org/simple", "https://pypi.org.evil.invalid/simple",
        "https://user:PRIVATE-CANARY@pypi.org/simple", "https://pypi.org:8443/simple",
        "https://pypi.org/simple?token=PRIVATE-CANARY", "https://pypi.org/simple#fragment",
        "https://pypi.org/other", " https://pypi.org/simple", "https://py\npi.org/simple",
        "https://files.pythonhosted.org/packages/example.whl"] {
        assert!(!public_index(raw), "{raw:?}");
    }
}

#[test]
fn uv_keeps_all_locked_variants_and_deduplicates_normalized_public_coordinates() {
    let text = uv(r#"
[[package]]
name = "Requests"
version = "02.031.0"
source = { registry = "https://pypi.org/simple" }
resolution-markers = ["python_full_version < '3.10'"]
wheels = [{ url = "https://files.pythonhosted.org/unused.whl", hash = "sha256:unused" }]
[[package]]
name = "requests"
version = "2.31.0"
source = { registry = "https://pypi.org/simple/" }
resolution-markers = ["python_full_version >= '3.10'"]
[[package]]
name = "requests"
version = "2.32.0rc1"
source = { registry = "https://pypi.org/simple" }
[[package]]
name = "PRIVATE-CANARY"
version = "0.1.0"
source = { editable = "." }
"#);
    let inventory = parse_inventory(&text, "uv");
    assert_eq!(inventory.packages.len(), 2);
    assert_eq!(inventory.packages[0].ecosystem, "PyPI");
    assert_eq!(inventory.packages[0].name, "requests");
    assert_eq!(inventory.packages[0].version, "2.31.0");
    assert_eq!(inventory.packages[0].locations.len(), 2);
    assert_eq!(inventory.packages[0].locations[1].package_path.as_deref(), Some("package[1]"));
    assert_eq!(inventory.excluded.len(), 1);
    assert!(!serde_json::to_string(&inventory).unwrap().contains("PRIVATE-CANARY"));
}

#[test]
fn uv_missing_private_git_path_and_ambiguous_origins_are_explicit_exclusions() {
    for source in ["", "source = {}", "source = {registry = 'https://private.invalid/simple'}",
        "source = {git = 'https://example.invalid/PRIVATE-CANARY'}", "source = {path = 'local.whl'}",
        "source = {virtual = '.'}", "source = {url = 'https://files.pythonhosted.org/pkg.whl'}",
        "source = {registry = 'https://pypi.org/simple', editable = '.'}"] {
        let text = uv(&format!("[[package]]\nname='PRIVATE-CANARY'\nversion='1.0'\n{source}\n"));
        let result = parse_inventory(&text, "uv");
        assert!(result.packages.is_empty());
        assert_eq!(result.excluded.len(), 1);
        assert!(!serde_json::to_string(&result).unwrap().contains("PRIVATE-CANARY"));
    }
    let missing_version = uv("[[package]]\nname='example'\nsource={registry='https://pypi.org/simple'}\n");
    assert_eq!(parse_inventory(&missing_version, "uv").excluded[0].reason, "version_not_recorded");
    let local = uv("[[package]]\nname='PRIVATE-CANARY'\nversion='1.0+internal'\nsource={registry='https://pypi.org/simple'}\n");
    let result = parse_inventory(&local, "uv");
    assert!(result.packages.is_empty());
    assert_eq!(result.excluded[0].reason, "python_local_version_not_public_release");
}

#[test]
fn poetry_default_pypi_is_distinct_from_an_explicit_empty_or_foreign_source() {
    for source in ["", "source = {type='legacy', url='https://pypi.org/simple/', reference='pypi'}"] {
        let result = parse_inventory(&poetry(&format!("[[package]]\nname='Django'\nversion='5.0rc1'\n{source}")), "poetry");
        assert_eq!(result.packages.len(), 1);
        assert_eq!(result.packages[0].name, "django");
        assert_eq!(result.packages[0].version, "5.0rc1");
    }
    for source in ["source={}", "source={type='legacy',url='https://private.invalid/simple'}",
        "source={type='git',url='https://pypi.org/simple'}",
        "source={type='url',url='https://pypi.org/simple'}", "develop=true",
        "source={type='legacy',url='https://pypi.org/simple',subdirectory='private'}"] {
        let result = parse_inventory(&poetry(&format!("[[package]]\nname='PRIVATE-CANARY'\nversion='1.0'\n{source}")), "poetry");
        assert!(result.packages.is_empty());
        assert_eq!(result.excluded.len(), 1);
        assert!(!serde_json::to_string(&result).unwrap().contains("PRIVATE-CANARY"));
    }
}

#[test]
fn poetry_groups_markers_and_optional_packages_do_not_narrow_an_inventory() {
    let result = parse_inventory(&poetry(r#"
[[package]]
name='Example_Pkg'
version='1!1.0.post2'
optional=true
groups=['dev']
markers="sys_platform == 'win32'"
[[package]]
name='example-pkg'
version='2.0.dev1'
optional=false
groups=['main']
"#), "poetry");
    assert_eq!(result.packages.len(), 2);
    assert!(result.packages.iter().all(|package| package.name == "example-pkg"));
}

#[test]
fn schema_and_descriptor_failures_are_not_successful_empty_inventories() {
    for text in ["version=2\npackage=[]", "version=1", "version=-1\npackage=[]",
        "version=1\nrevision=-1\npackage=[]", "version=1\nversion=1\npackage=[]",
        "version=1\n[[package]]\nname='PRIVATE-CANARY'\nversion=1",
        "version=1\n[[package]]\nname='PRIVATE-CANARY'\nsource={registry=17}",
        "version=1\n[[package]]\nname='PRIVATE-CANARY'\nsource='wrong'"] {
        let failure = parse(text, "uv.lock", "uv", &mut Builder::default()).unwrap_err();
        assert!(!failure.to_string().contains("PRIVATE-CANARY"));
    }
    for schema in ["1.0", "2.2", "3.0", "PRIVATE-CANARY"] {
        let text = format!("package=[]\n[metadata]\nlock-version='{schema}'\n");
        let failure = parse(&text, "poetry.lock", "poetry", &mut Builder::default()).unwrap_err();
        assert!(!failure.to_string().contains("PRIVATE-CANARY"));
    }
    for schema in ["1.1", "2.0", "2.1"] {
        let text = format!("package=[]\n[metadata]\nlock-version='{schema}'\n");
        assert!(parse(&text, "poetry.lock", "poetry", &mut Builder::default()).is_ok());
    }
}

#[test]
fn private_rows_still_consume_the_shared_entry_budget() {
    let mut builder = Builder { entries: super::super::MAX_ENTRIES, ..Builder::default() };
    let text = uv("[[package]]\nname='private'\nsource={virtual='.'}");
    assert!(parse(&text, "uv.lock", "uv", &mut builder).is_err());
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn real_default_inventory_combines_python_lockfiles_and_retains_hashes_and_locations() {
    let dir = tempfile::tempdir().unwrap();
    let text = uv("[[package]]\nname='Example_Pkg'\nversion='1.0'\nsource={registry='https://pypi.org/simple'}");
    std::fs::write(dir.path().join("uv.lock"), &text).unwrap();
    std::fs::write(dir.path().join("poetry.lock"), poetry("[[package]]\nname='example-pkg'\nversion='1.0'")).unwrap();
    let result = inventory(dir.path(), &[]).unwrap();
    assert_eq!(result.lockfiles.len(), 2);
    assert_eq!(result.packages.len(), 1);
    assert_eq!(result.packages[0].locations.len(), 2);
    assert!(result.lockfiles.iter().any(|file| file.path == "uv.lock"
        && file.format == "uv" && file.sha256 == super::super::digest(text.as_bytes())));
    assert_eq!(inventory(dir.path(), &["./uv.lock".into()]).unwrap().lockfiles.len(), 1);
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn python_inventory_reuses_real_descriptor_confinement_and_does_not_resolve_manifests() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("uv.lock"), uv("package=[]")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("uv.lock"), dir.path().join("uv.lock")).unwrap();
    assert!(inventory(dir.path(), &["uv.lock".into()]).is_err());
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
    assert!(inventory(dir.path(), &["linked/uv.lock".into()]).is_err());
    assert!(inventory(dir.path(), &["../uv.lock".into()]).is_err());
    assert!(inventory(dir.path(), &["pyproject.toml".into()]).is_err());
    assert!(inventory(dir.path(), &["requirements.txt".into()]).is_err());
}
