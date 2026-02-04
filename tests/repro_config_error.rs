use pi::config::Config;
use std::path::{Path, PathBuf};

struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    fn new(path: &Path) -> Self {
        let original = std::env::current_dir().expect("read current dir");
        std::env::set_current_dir(path).expect("set current dir");
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

#[test]
fn load_errors_on_invalid_project_settings() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let project_dir = temp_dir.path();
    let settings_dir = project_dir.join(".pi");
    std::fs::create_dir_all(&settings_dir).expect("create settings dir");

    let settings_path = settings_dir.join("settings.json");
    std::fs::write(&settings_path, "{ invalid json").expect("write invalid json");

    let _guard = CwdGuard::new(project_dir);
    let result = Config::load();
    assert!(result.is_err(), "expected error for invalid json");
}
