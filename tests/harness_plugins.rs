//! A second plugin proves dispatch, project scopes and receipts are not Codex-specific.
use futures_util::future::BoxFuture;
use pi_agent::projects::runtime::ProjectRuntime;
use pi_agent::{
    app::State,
    config::Config,
    core::error::Error,
    harness::{config::ProfileConfig, HarnessDriver, HarnessPlugin, HarnessPlugins},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct FixturePlugin {
    executions: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}
struct FixtureDriver {
    profile: ProfileConfig,
    scope: Option<String>,
    executions: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl HarnessPlugin for FixturePlugin {
    fn kind(&self) -> &'static str {
        "fixture"
    }
    fn validate(&self, config: &ProfileConfig) -> Result<(), String> {
        if config.command == "fixture" {
            Ok(())
        } else {
            Err("Fixture command required".into())
        }
    }
    fn create(
        &self,
        profile: ProfileConfig,
        runtime: Option<Arc<ProjectRuntime>>,
    ) -> Result<Box<dyn HarnessDriver>, String> {
        Ok(Box::new(FixtureDriver {
            profile,
            scope: runtime.map(|r| r.project.id.clone()),
            executions: self.executions.clone(),
            stops: self.stops.clone(),
        }))
    }
}
impl HarnessDriver for FixtureDriver {
    fn descriptor(&self) -> Value {
        json!({"id":self.profile.id,"kind":"fixture","projectRuntime":self.profile.projects})
    }
    fn read<'a>(&'a self, _name: &'a str, _args: Value) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move { Ok(json!({"sessions":[],"nextCursor":null,"scope":self.scope})) })
    }
    fn execute<'a>(
        &'a self,
        _name: &'a str,
        _args: &'a Value,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"turnId":"fixture-turn"}))
        })
    }
    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn drain(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

fn fixture() -> (
    tempfile::TempDir,
    Config,
    HarnessPlugins,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let root = tempfile::tempdir().unwrap();
    for name in ["home", "work"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    let config = toml::from_str(&format!(
        "listen='127.0.0.1:0'\nserver_id='plugin-fixture'\nexecution=false\napi_key='fixture-api-key-at-least-24-bytes'\n[projects]\nroot='{}'\n[[exports]]\nalias='work'\npath='{}'\naccess='rw'\n[[harnesses]]\nid='custom'\nkind='fixture'\ncommand='fixture'\nhome='{}'\nprojects=true\n",
        root.path().join("catalog").display(),root.path().join("work").display(),root.path().join("home").display())).unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let mut plugins = HarnessPlugins::default();
    plugins
        .register(Arc::new(FixturePlugin {
            executions: executions.clone(),
            stops: stops.clone(),
        }))
        .unwrap();
    (root, config, plugins, executions, stops)
}

#[tokio::test]
async fn registered_plugins_dispatch_with_shared_receipts_and_shutdown() {
    let (_root, config, plugins, executions, stops) = fixture();
    let state = State::from_config_with_plugins(&config, plugins).unwrap();
    let profiles = state.harness.profiles();
    assert_eq!(profiles["profiles"][0]["kind"], "fixture");
    let args = json!({"profileId":"custom","epoch":profiles["epoch"],"requestId":"once"});
    let first = state
        .harness
        .call("harness_turn", args.clone())
        .await
        .unwrap();
    assert_eq!(
        state
            .harness
            .call("harness_turn", args.clone())
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        state.harness.call("harness_operation", args).await.unwrap(),
        first
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    state.harness.close().await;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert!(state
        .harness
        .call("harness_sessions", json!({"profileId":"custom"}))
        .await
        .is_err());
}

#[tokio::test]
async fn project_scoped_driver_uses_the_registered_plugin() {
    let (_root, config, plugins, _executions, stops) = fixture();
    let state = State::from_config_with_plugins(&config, plugins).unwrap();
    let registered = pi_agent::projects::call(
        &state,
        0,
        "project_register",
        json!({"name":"Work","alias":"work","path":"","access":"rw"}),
    )
    .await
    .unwrap();
    let project = &registered["project"];
    let page = state
        .harness
        .project_call(
            &state,
            0,
            "harness_sessions",
            json!({"profileId":"custom","projectId":project["id"],"revision":project["revision"]}),
        )
        .await
        .unwrap();
    assert_eq!(page["scope"], project["id"]);
    state
        .harness
        .close_project(project["id"].as_str().unwrap())
        .await;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    state.harness.close().await;
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}

#[test]
fn validation_rejects_missing_duplicate_and_invalid_plugin_configuration() {
    let (_root, mut config, mut plugins, executions, stops) = fixture();
    assert!(pi_agent::harness::config::validate(&config).is_err());
    assert!(pi_agent::harness::config::validate_with_plugins(&config, &plugins).is_ok());
    assert!(plugins
        .register(Arc::new(FixturePlugin { executions, stops }))
        .is_err());
    config.harnesses[0].command = "unexpected".into();
    assert!(pi_agent::harness::config::validate_with_plugins(&config, &plugins).is_err());
}
