use pi_agent::{app::State, config::Config, projects::call};
use serde_json::{json, Value};
use std::{os::unix::fs::symlink, sync::Arc};

async fn fixture() -> (tempfile::TempDir, Arc<State>, Value) {
    let root = tempfile::tempdir().unwrap();
    for path in ["export/project/ref", "reference", "catalog", "native"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let config: Config = toml::from_str(&format!(
        r#"
listen="127.0.0.1:0"
execution=false
server_id="search-test"
token="search-test-secret-0123456789"
[projects]
root={catalog:?}
[[exports]]
alias="root"
path={export:?}
access="rw"
[[exports]]
alias="reference"
path={reference:?}
[[harnesses]]
id="codex"
kind="codex"
home={native:?}
projects=true
"#,
        catalog = root.path().join("catalog"),
        export = root.path().join("export"),
        reference = root.path().join("reference"),
        native = root.path().join("native")
    ))
    .unwrap();
    let state = State::from_config(&config).unwrap();
    state
        .execution
        .retain_lock(std::fs::File::open(root.path()).unwrap());
    state.execution.accept();
    let project = call(
        &state,
        0,
        "project_register",
        json!({"name":"P","alias":"root","path":"project","access":"ro",
        "mounts":[{"alias":"reference","path":"","at":"/workspace/ref","access":"ro"}]}),
    )
    .await
    .unwrap()["project"]
        .clone();
    (root, state, project)
}
async fn search(
    state: &Arc<State>,
    project: &Value,
    query: &str,
    mode: &str,
) -> Result<Value, pi_agent::core::error::Error> {
    call(
        state,
        0,
        "project_search",
        json!({"projectId":project["id"],"revision":project["revision"],"query":query,"mode":mode}),
    )
    .await
}
#[tokio::test]
async fn searches_only_the_readonly_project_view_and_applies_ignore_and_private_policies() {
    let (root, state, project) = fixture().await;
    let export = root.path().join("export/project");
    std::fs::write(export.join("ref/shadowed.txt"), "needle secret").unwrap();
    std::fs::write(root.path().join("reference/visible.txt"), "needle mount").unwrap();
    std::fs::write(export.join("visible\nfile.txt"), "before\nneedle main").unwrap();
    std::fs::write(export.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(export.join("ignored.txt"), "needle ignored").unwrap();
    std::fs::write(export.join("binary.bin"), b"needle\0binary").unwrap();
    std::fs::write(root.path().join("native/auth.json"), "needle private").unwrap();
    symlink(root.path().join("native"), export.join("external-link")).unwrap();
    let result = search(&state, &project, "needle", "content").await.unwrap();
    let paths: Vec<_> = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&"ref/visible.txt"));
    assert!(paths.contains(&"visible\nfile.txt"));
    assert_eq!(
        search(&state, &project, "before\nneedle", "content")
            .await
            .unwrap()["matches"][0]["line"],
        1
    );
    assert_eq!(
        search(&state, &project, "no-match", "content")
            .await
            .unwrap()["matches"],
        json!([])
    );
    assert_eq!(
        search(&state, &project, "-leading-query", "content")
            .await
            .unwrap()["matches"],
        json!([])
    );
    assert_eq!(
        search(&state, &project, "visible\n", "path").await.unwrap()["matches"][0]["path"],
        "visible\nfile.txt"
    );
}
#[tokio::test]
async fn rejects_revoked_revisions_and_replaced_directory_identities_before_search() {
    let (root, state, mut project) = fixture().await;
    project["revision"] = json!(999);
    assert_eq!(
        search(&state, &project, "needle", "content")
            .await
            .unwrap_err()
            .code,
        "PROJECT_REVISION_CHANGED"
    );
    project["revision"] = json!(1);
    std::fs::rename(
        root.path().join("export/project"),
        root.path().join("export/old"),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("export/project")).unwrap();
    std::fs::create_dir(root.path().join("export/project/ref")).unwrap();
    assert_eq!(
        search(&state, &project, "needle", "content")
            .await
            .unwrap_err()
            .code,
        "PROJECT_DIRECTORY_CHANGED"
    );
}
