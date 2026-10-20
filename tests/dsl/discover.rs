use std::fs;
use std::path::PathBuf;

use vault_dsl::discover;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("vault-dsl-{label}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The unreadable-directory test leaves a 0o000 subdir; reopen it so removal works.
            if let Ok(entries) = fs::read_dir(&self.0) {
                for e in entries.flatten() {
                    let _ = fs::set_permissions(e.path(), fs::Permissions::from_mode(0o755));
                }
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

const CONFIG: &str = "environments: { local: { target: { base_url: http://127.0.0.1:1 } } }\n";
const TEST: &str = "test: a\nsteps: [{ name: s, request: { method: GET, path: /x } }]\n";

#[test]
fn a_suite_root_named_fixtures_is_still_discovered() {
    let tmp = TempDir::new("fixtures-root");
    let root = tmp.0.join("fixtures");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("vault.yaml"), CONFIG).unwrap();
    fs::write(root.join("a.test.yaml"), TEST).unwrap();
    let suite = discover(&root).unwrap();
    assert_eq!(suite.tests.len(), 1);
}

#[cfg(unix)]
#[test]
fn unreadable_directory_fails_the_load() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("locked");
    let root = &tmp.0;
    fs::write(root.join("vault.yaml"), CONFIG).unwrap();
    let locked = root.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("hidden.test.yaml"), TEST).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        return; // root ignores directory permissions; nothing to prove here
    }
    let err = discover(root).unwrap_err();
    assert!(err.to_string().contains("locked"), "{err}");
}
