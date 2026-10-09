use pi_agent::{app::State, config::Config, projects::call};
use serde_json::{json, Value};
use std::{os::unix::fs::symlink, sync::Arc};
fn fixture() -> (tempfile::TempDir, Arc<State>) {
    let root = tempfile::tempdir().unwrap();
    for path in [
        "export/project/ref",
        "export/project/private-native",
        "reference",
        "catalog",
    ] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let config:Config=toml::from_str(&format!("listen=\"127.0.0.1:0\"\nexecution=false\nserver_id=\"watch-test\"\ntoken=\"watch-fixture-token-0123456789\"\n[projects]\nroot={catalog:?}\n[[exports]]\nalias=\"root\"\npath={export:?}\naccess=\"rw\"\n[[exports]]\nalias=\"reference\"\npath={reference:?}\n[[harnesses]]\nid=\"codex\"\nkind=\"codex\"\nhome={native:?}\nprojects=true\n",catalog=root.path().join("catalog"),export=root.path().join("export"),reference=root.path().join("reference"),native=root.path().join("export/project/private-native"))).unwrap();
    let state = State::from_config(&config).unwrap();
    (root, state)
}
async fn project(state: &Arc<State>) -> Value {
    call(state,0,"project_register",json!({"name":"P","alias":"root","path":"project","access":"ro","mounts":[{"alias":"reference","path":"","at":"/workspace/ref","access":"ro"}]})).await.unwrap()["project"].clone()
}
async fn observe(state: &Arc<State>, project: &Value, watch: Option<&Value>) -> Value {
    let mut args = json!({"projectId":project["id"],"revision":project["revision"]});
    if let Some(watch) = watch {
        args["watchId"] = watch["watchId"].clone();
    }
    call(state, 0, "project_watch", args).await.unwrap()
}
#[tokio::test]
async fn detects_external_writes_new_directories_and_mounts_without_scanning_private_or_shadowed_trees(
) {
    let (root, state) = fixture();
    let project = project(&state).await;
    let first = observe(&state, &project, None).await;
    assert_eq!(first["truncated"], false);
    assert_eq!(
        observe(&state, &project, Some(&first)).await["version"],
        first["version"]
    );
    let base = root.path().join("export/project");
    std::fs::write(base.join("external.md"), "written by an independent CLI").unwrap();
    let changed = observe(&state, &project, Some(&first)).await;
    assert_ne!(changed["version"], first["version"]);
    std::fs::write(base.join("ref/shadowed.md"), "private shadow").unwrap();
    std::fs::write(base.join("private-native/auth.json"), "secret").unwrap();
    assert_eq!(
        observe(&state, &project, Some(&changed)).await["version"],
        changed["version"]
    );
    std::fs::write(root.path().join("reference/visible.md"), "attached mount").unwrap();
    let mounted = observe(&state, &project, Some(&changed)).await;
    assert_ne!(mounted["version"], changed["version"]);
    std::fs::create_dir(base.join("new")).unwrap();
    let created = observe(&state, &project, Some(&mounted)).await;
    std::fs::write(base.join("new/nested.md"), "later write").unwrap();
    assert_ne!(
        observe(&state, &project, Some(&created)).await["version"],
        created["version"]
    );
    symlink(root.path().join("reference"), base.join("link")).unwrap();
    let linked = observe(&state, &project, Some(&created)).await;
    std::fs::write(
        root.path().join("reference/next.md"),
        "authorized mount only",
    )
    .unwrap();
    let version = observe(&state, &project, Some(&linked)).await;
    assert_ne!(version["version"], linked["version"]);
    assert!(version.as_object().unwrap().keys().all(|key| [
        "watchId",
        "version",
        "gap",
        "truncated"
    ]
    .contains(&key.as_str())));
    let args = json!({"projectId":project["id"],"revision":project["revision"],"watchId":first["watchId"]});
    assert_eq!(
        call(&state, 0, "project_unwatch", args.clone())
            .await
            .unwrap()["released"],
        true
    );
    let restarted = call(&state, 0, "project_watch", args).await.unwrap();
    assert_eq!(restarted["gap"], true);
    assert_ne!(restarted["watchId"], first["watchId"]);
}
#[tokio::test]
async fn fences_watch_tokens_by_project_revision_owner_and_directory_identity() {
    let (root, state) = fixture();
    let project = project(&state).await;
    let first = observe(&state, &project, None).await;
    let args = json!({"projectId":project["id"],"revision":project["revision"],"watchId":first["watchId"]});
    assert!(call(&state, 1, "project_watch", args.clone())
        .await
        .is_err());
    let mut stale = args.clone();
    stale["revision"] = json!(2);
    assert_eq!(
        call(&state, 0, "project_watch", stale)
            .await
            .unwrap_err()
            .code,
        "PROJECT_REVISION_CHANGED"
    );
    let base = root.path().join("export/project");
    std::fs::rename(&base, root.path().join("export/old")).unwrap();
    std::fs::create_dir(&base).unwrap();
    assert!(call(&state, 0, "project_watch", args).await.is_err());
}
