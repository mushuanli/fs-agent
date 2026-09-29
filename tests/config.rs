use fs_agent::config::{Config, State};
use std::sync::{Arc, Mutex, MutexGuard};

// Credential resolution reads the process environment, so the tests serialize.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENVIRONMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn load(body: &str) -> Result<Arc<State>, String> {
    let text = format!("listen = \"127.0.0.1:0\"\n{body}");
    toml::from_str::<Config>(&text).unwrap().state()
}

/// A temporary directory plus a valid child directory to export.
fn fixture() -> (tempfile::TempDir, String) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("x1");
    std::fs::create_dir(&path).unwrap();
    (root, path.display().to_string())
}

#[test]
fn derives_alias_from_path_and_defaults_to_read_only() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "username = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .unwrap();
    assert_eq!(state.clients.len(), 1);
    assert_eq!(state.clients[0].username.as_deref(), Some("li"));
    assert_eq!(state.clients[0].exports, vec!["x1"]);
    assert!(state.clients[0].write_exports.is_empty());
    assert!(!state.exports["x1"].writable());
}

#[test]
fn read_write_access_implies_exclusive_writes() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "username = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = \"{path}\"\naccess = \"rw\"\n"
    ))
    .unwrap();
    assert_eq!(state.clients[0].write_exports, vec!["x1"]);
    assert!(state.exports["x1"].writable());
    assert!(fs_agent::filesystem::Export::exclusive(&path).is_err());
}

#[test]
fn explicit_alias_overrides_the_directory_name() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "username = \"li\"\npassword = \"12345678\"\n[[exports]]\nalias = \"ds\"\npath = \"{path}\"\n"
    ))
    .unwrap();
    assert_eq!(state.clients[0].exports, vec!["ds"]);
}

#[test]
fn token_clients_use_bearer_without_a_username() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "token = \"token-at-least-24-bytes-long\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .unwrap();
    assert!(state.clients[0].username.is_none());
}

#[test]
fn environment_variants_still_resolve() {
    let _guard = lock();
    std::env::set_var("FS_SERVER_TEST_PASSWORD", "environment-password-12");
    let (_root, path) = fixture();
    let state = load(&format!(
        "username = \"dave\"\npassword_env = \"FS_SERVER_TEST_PASSWORD\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .unwrap();
    assert_eq!(state.clients[0].username.as_deref(), Some("dave"));
    assert_eq!(state.clients[0].token, "environment-password-12");
}

#[test]
fn rejects_short_or_conflicting_credentials() {
    let _guard = lock();
    let (_root, path) = fixture();
    for body in [
        format!("username = \"li\"\npassword = \"short\"\n[[exports]]\npath = \"{path}\"\n"),
        format!("username = \"li\"\npassword = \"12345678\"\ntoken = \"token-at-least-24-bytes-long\"\n[[exports]]\npath = \"{path}\"\n"),
        format!("username = \"li\"\npassword = \"12345678\"\npassword_env = \"FS_SERVER_PASSWORD\"\n[[exports]]\npath = \"{path}\"\n"),
        format!("username = \"li\"\n[[exports]]\npath = \"{path}\"\n"),
        "username = \"li\"\npassword = \"12345678\"\n".to_owned(),
    ] {
        assert!(load(&body).is_err(), "{body}");
    }
}

#[test]
fn reports_a_missing_username_and_environment_variable() {
    let _guard = lock();
    std::env::remove_var("FS_SERVER_USER");
    std::env::remove_var("FS_SERVER_TEST_UNSET_PASSWORD");
    let (_root, path) = fixture();
    let missing_username = load(&format!(
        "password = \"12345678\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .err()
    .expect("a missing username must be rejected");
    assert!(
        missing_username.contains("username is required"),
        "{missing_username}"
    );
    let missing_variable = load(&format!(
        "username = \"li\"\npassword_env = \"FS_SERVER_TEST_UNSET_PASSWORD\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .err()
    .expect("a missing environment variable must be rejected");
    assert!(
        missing_variable.contains("FS_SERVER_TEST_UNSET_PASSWORD"),
        "{missing_variable}"
    );
}

#[test]
fn rejects_invalid_duplicate_and_overlapping_exports() {
    let _guard = lock();
    let root = tempfile::tempdir().unwrap();
    let spaced = root.path().join("my dir");
    let outer = root.path().join("a");
    let inner = outer.join("b");
    for path in [&spaced, &outer, &inner] {
        std::fs::create_dir(path).unwrap();
    }
    let base = "username = \"li\"\npassword = \"12345678\"\n";
    for (body, expected) in [
        (
            format!("{base}[[exports]]\npath = \"{}\"\n", spaced.display()),
            "set \"alias\"",
        ),
        (
            format!(
                "{base}[[exports]]\nalias = \"same\"\npath = \"{}\"\n[[exports]]\nalias = \"same\"\npath = \"{}\"\n",
                spaced.display(),
                inner.display()
            ),
            "already defined",
        ),
        (
            format!(
                "{base}[[exports]]\npath = \"{}\"\n[[exports]]\npath = \"{}\"\n",
                outer.display(),
                inner.display()
            ),
            "overlaps",
        ),
    ] {
        let error = load(&body).err().expect("the export must be rejected");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn rejects_an_unknown_access_value() {
    let parsed: Result<Config, _> = toml::from_str(
        "listen = \"127.0.0.1:0\"\nusername = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = \"/tmp\"\naccess = \"rx\"\n",
    );
    assert!(parsed.is_err());
}

#[test]
fn stable_node_identity_is_explicit_and_validated() {
    let _guard = lock();
    let (_root, path) = fixture();
    let config = |id: &str| {
        format!("server_id = {id:?}\nusername = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = {path:?}\n")
    };
    assert_eq!(
        load(&config("my-node")).unwrap().server_id.as_deref(),
        Some("my-node")
    );
    assert_eq!(
        load(&config("my-node")).unwrap().server_id.as_deref(),
        Some("my-node")
    );
    assert!(load(&config("../other-node")).is_err());
    assert!(load(&config("")).is_err());
}
