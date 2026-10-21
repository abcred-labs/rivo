use rivo_storage::{ArtifactMetadata, ArtifactStore, Memory, MemoryCandidate, Store};
use rivo_test_support::{assert_redacted, HttpResponse, ScriptedHttpServer, TestEnvironment};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

mod support;

static APP_SERVER_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_app_server_test() -> MutexGuard<'static, ()> {
    APP_SERVER_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("app-server test lock poisoned")
}

fn app_server_test_timeout() -> Duration {
    // Named-pipe and child-process startup can be delayed on shared Windows
    // runners. These tests validate message ordering and cleanup, not startup
    // latency, so give the Windows runner additional room before declaring the
    // bridge stalled.
    if cfg!(windows) {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(10)
    }
}

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_rivo"))
}

fn schema3_environment() -> TestEnvironment {
    let environment = TestEnvironment::new().unwrap();
    std::fs::write(environment.config_path(), "version = 6\n").unwrap();
    environment
}

fn run(environment: &TestEnvironment, args: &[&str]) -> rivo_test_support::CliOutput {
    environment
        .command(binary())
        .args(args.iter().copied())
        .run()
        .unwrap()
}

#[test]
fn archive_upgrade_defaults_to_read_only_check_and_accepts_check_flag() {
    let binary_digest = Sha256::digest(std::fs::read(binary()).unwrap());
    for args in [vec!["upgrade"], vec!["upgrade", "--check"]] {
        let environment = schema3_environment();
        let before = std::fs::read(environment.config_path()).unwrap();
        let platform = std::env::consts::OS;
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => panic!("unsupported test architecture: {other}"),
        };
        let server = ScriptedHttpServer::start(vec![HttpResponse::json(
            serde_json::json!({
                "schema": 2, "product": "rivo", "version": "999.0.0",
                "platform": platform, "arch": arch,
            })
            .to_string(),
        )])
        .unwrap();
        let output = environment
            .command(binary())
            .args(args)
            .env("RIVO_RELEASE_BASE_URL", server.base_url())
            .run()
            .unwrap();
        output.assert_success();
        assert!(output.stdout.contains("999.0.0"));
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].request_line,
            format!("GET /{platform}-{arch}/release-manifest.json HTTP/1.1")
        );
        assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(environment.data_dir()).unwrap().count(),
            0
        );
    }
    assert_eq!(
        Sha256::digest(std::fs::read(binary()).unwrap()),
        binary_digest
    );
}

#[test]
fn upgrade_help_exposes_apply_and_noninteractive_confirmation() {
    let environment = schema3_environment();
    let output = run(&environment, &["upgrade", "--help"]);
    output.assert_success();
    assert!(output.stdout.contains("--check"));
    assert!(output.stdout.contains("--apply"));
    assert!(output.stdout.contains("--yes"));
}

fn set_test_credential(environment: &TestEnvironment, name: &str) {
    environment
        .command(binary())
        .args([
            "config",
            "credentials",
            "set",
            name,
            "--from-env",
            "RIVO_TEST_CREDENTIAL",
        ])
        .env("RIVO_TEST_CREDENTIAL", "test-only-credential-value")
        .run()
        .unwrap()
        .assert_success();
}

// 固定 discovery 是测试夹具证据；真实刷新路径由独立黑盒测试覆盖。
fn configure_fixture(environment: &TestEnvironment, base_url: &str) {
    support::write_routing_fixture(
        environment.config_path(),
        environment.workspace(),
        base_url,
        "fixture-model",
        "RIVO_FIXTURE_KEY",
    );
}

fn remove_provider(environment: &TestEnvironment, id: &str) {
    run(environment, &["config", "provider", "remove", id, "--yes"]).assert_success();
}

fn start_loopback_mcp_fixture() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = thread::spawn(move || {
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            let (status, body) = if request.contains("notifications/initialized") {
                ("202 Accepted", String::new())
            } else if request.contains("tools/list") {
                ("200 OK", r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"fixture tool","inputSchema":{"type":"object"}}]}}"#.into())
            } else if request.contains("resources/list") {
                ("200 OK", r#"{"jsonrpc":"2.0","id":1,"result":{"resources":[{"uri":"memory://fixture","name":"fixture resource"}]}}"#.into())
            } else if request.contains("prompts/list") {
                ("200 OK", r#"{"jsonrpc":"2.0","id":1,"result":{"prompts":[{"name":"summarize","description":"fixture prompt"}]}}"#.into())
            } else {
                (
                    "200 OK",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}"#.into(),
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (format!("http://{address}/mcp"), fixture)
}

#[test]
fn e2e_app_server_stdio_is_json_rpc_only() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let mut child = Command::new(binary())
        .args(["app-server", "--stdio"])
        .env("RIVO_CONFIG", environment.config_path())
        .env("RIVO_DATA_DIR", environment.data_dir())
        .env("HOME", environment.root())
        .env("USERPROFILE", environment.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "not json").unwrap();
    // Half-close stdin immediately after a burst. EOF must drain accepted
    // replies, rather than winning a race against the stdout writer.
    for id in 1..=32 {
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":{id},"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"fixture","version":"1"}}}}}}"#).unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let lines = String::from_utf8(output.stdout).unwrap();
    let messages = lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 33);
    assert_eq!(messages[0]["error"]["code"], "invalid_request");
    for (index, message) in messages.iter().skip(1).enumerate() {
        assert_eq!(message["id"], index + 1);
        assert_eq!(message["result"]["server"]["name"], "rivo");
    }
    assert_eq!(messages[1]["result"]["server"]["name"], "rivo");
    assert_eq!(
        messages[1]["result"]["capabilities"]["supports_resume"],
        true
    );
}

fn read_rpc(stdout: &mut BufReader<ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    assert_ne!(
        stdout.read_line(&mut line).unwrap(),
        0,
        "app-server exited early"
    );
    serde_json::from_str(line.trim_end()).unwrap()
}

fn start_app_server(environment: &TestEnvironment) -> Child {
    Command::new(binary())
        .args(["app-server", "--stdio"])
        .env("RIVO_CONFIG", environment.config_path())
        .env("RIVO_DATA_DIR", environment.data_dir())
        .env("HOME", environment.root())
        .env("USERPROFILE", environment.root())
        .env("RIVO_FIXTURE_KEY", "bridge-secret")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn e2e_app_server_artifact_read_delete_is_scope_bound_and_logical_only() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, "http://127.0.0.1:1/v1");
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"artifact-fixture","version":"1"}}}}}}"#).unwrap();
    let initialized = read_rpc(&mut stdout);
    let sidecar = initialized["result"]["capabilities"]["sidecar"].clone();
    let node_id = sidecar["node_id"].as_str().unwrap();
    let workspace_id = sidecar["workspace_id"].as_str().unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"workspace_write"}},"idempotency_key":"artifact-start"}}}}"#, serde_json::to_string(&workspace).unwrap()).unwrap();
    let session = read_rpc(&mut stdout)["result"]["session_ref"]
        .as_str()
        .unwrap()
        .to_owned();

    let database = environment.data_dir().join("rivo").join("rivo.db");
    let store = Store::open_runtime(&database).unwrap();
    let artifact_store = ArtifactStore::new(environment.workspace());
    let artifact = artifact_store
        .put(
            &ArtifactMetadata {
                artifact_id: "rpc-artifact".into(),
                node_id: node_id.into(),
                workspace_id: workspace_id.into(),
                session_id: session.clone(),
                kind: "text/plain".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                download_scope: "session".into(),
            },
            b"sidecar artifact",
        )
        .unwrap();
    store.save_artifact(&artifact).unwrap();

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"artifact/read","params":{{"session_ref":{},"artifact_id":"rpc-artifact","idempotency_key":"artifact-read"}}}}"#, serde_json::to_string(&session).unwrap()).unwrap();
    let read = read_rpc(&mut stdout);
    assert_eq!(read["result"]["artifact"]["artifact_id"], "rpc-artifact");
    assert_eq!(read["result"]["artifact"]["size"], 16);
    assert_eq!(read["result"]["content_base64"], "c2lkZWNhciBhcnRpZmFjdA==");
    assert!(!read.to_string().contains(".rivo\\artifacts"));

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"artifact/read","params":{{"session_ref":"other-session","artifact_id":"rpc-artifact","idempotency_key":"artifact-wrong-scope"}}}}"#).unwrap();
    assert_eq!(read_rpc(&mut stdout)["error"]["code"], "artifact_not_found");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":5,"method":"artifact/delete","params":{{"session_ref":{},"artifact_id":"rpc-artifact","idempotency_key":"artifact-delete"}}}}"#, serde_json::to_string(&session).unwrap()).unwrap();
    assert_eq!(read_rpc(&mut stdout)["result"]["deleted"], true);
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":6,"method":"artifact/read","params":{{"session_ref":{},"artifact_id":"rpc-artifact","idempotency_key":"artifact-read-after-delete"}}}}"#, serde_json::to_string(&session).unwrap()).unwrap();
    assert_eq!(read_rpc(&mut stdout)["error"]["code"], "artifact_expired");
    let oversized = artifact_store
        .put(
            &ArtifactMetadata {
                artifact_id: "rpc-large".into(),
                node_id: node_id.into(),
                workspace_id: workspace_id.into(),
                session_id: session.clone(),
                kind: "text".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                download_scope: "session".into(),
            },
            &vec![b'x'; 48 * 1024],
        )
        .unwrap();
    store.save_artifact(&oversized).unwrap();
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":7,"method":"artifact/read","params":{"session_ref":session,"artifact_id":"rpc-large","idempotency_key":"large-read"}})).unwrap();
    assert_eq!(read_rpc(&mut stdout)["error"]["code"], "artifact_too_large");
    let modified = environment.workspace().join(".rivo/artifacts/rpc-large");
    std::fs::write(&modified, b"user-edited content").unwrap();
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":8,"method":"artifact/delete","params":{"session_ref":session,"artifact_id":"rpc-large","idempotency_key":"modified-delete"}})).unwrap();
    assert_eq!(
        read_rpc(&mut stdout)["error"]["code"],
        "artifact_cleanup_failed"
    );
    assert_eq!(
        store
            .artifact_for_scope("rpc-large", node_id, workspace_id, &session)
            .unwrap()
            .unwrap()
            .state,
        "cleanup_failed"
    );
    assert_eq!(std::fs::read(&modified).unwrap(), b"user-edited content");
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":9,"method":"artifact/read","params":{"session_ref":session,"artifact_id":"rpc-large","idempotency_key":"cleanup-audit-read"}})).unwrap();
    assert_eq!(
        read_rpc(&mut stdout)["error"]["code"],
        "artifact_cleanup_failed"
    );
    drop(stdin);
    drop(stdout);
    assert!(child.wait().unwrap().success());
}

#[test]
fn e2e_app_server_failed_turn_cleans_runtime_artifacts() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, "http://127.0.0.1:1/v1");
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"cleanup-fixture","version":"1"}}}}}}"#).unwrap();
    let initialized = read_rpc(&mut stdout);
    let sidecar = initialized["result"]["capabilities"]["sidecar"].clone();
    let node_id = sidecar["node_id"].as_str().unwrap();
    let workspace_id = sidecar["workspace_id"].as_str().unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"workspace_write"}},"idempotency_key":"cleanup-start"}}}}"#, serde_json::to_string(&workspace).unwrap()).unwrap();
    let session = read_rpc(&mut stdout)["result"]["session_ref"]
        .as_str()
        .unwrap()
        .to_owned();

    let database = environment.data_dir().join("rivo").join("rivo.db");
    let store = Store::open_runtime(&database).unwrap();
    let artifact = ArtifactStore::new(environment.workspace())
        .put(
            &ArtifactMetadata {
                artifact_id: "failed-turn-artifact".into(),
                node_id: node_id.into(),
                workspace_id: workspace_id.into(),
                session_id: session.clone(),
                kind: "text/plain".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                download_scope: "session".into(),
            },
            b"runtime-owned",
        )
        .unwrap();
    let artifact_path = environment
        .workspace()
        .join(".rivo")
        .join("artifacts")
        .join("failed-turn-artifact");
    assert!(artifact_path.exists());
    store.save_artifact(&artifact).unwrap();

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"session_ref":{},"prompt":"force provider failure","runtime_settings":{{}},"idempotency_key":"cleanup-turn"}}}}"#, serde_json::to_string(&session).unwrap()).unwrap();
    let mut terminal = None;
    while terminal.is_none() {
        let message = read_rpc(&mut stdout);
        if message.pointer("/params/type")
            == Some(&serde_json::Value::String("turn_complete".into()))
        {
            terminal = Some(message);
        }
    }
    let terminal = terminal.unwrap();
    assert_ne!(terminal["params"]["payload"]["outcome"], "completed");

    let cleaned = Store::open_runtime(&database)
        .unwrap()
        .artifact_for_scope("failed-turn-artifact", node_id, workspace_id, &session)
        .unwrap()
        .unwrap();
    assert_eq!(cleaned.state, "deleted");
    assert!(!artifact_path.exists());

    drop(stdin);
    drop(stdout);
    assert!(child.wait().unwrap().success());
    let mut diagnostics = String::new();
    stderr.read_to_string(&mut diagnostics).unwrap();
    assert!(diagnostics.is_empty(), "{diagnostics}");

    let restart_owned = ArtifactStore::new(environment.workspace())
        .put(
            &ArtifactMetadata {
                artifact_id: "restart-owned-artifact".into(),
                node_id: node_id.into(),
                workspace_id: workspace_id.into(),
                session_id: session.clone(),
                kind: "text/plain".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                download_scope: "session".into(),
            },
            b"restart-runtime-owned",
        )
        .unwrap();
    let restart_path = environment
        .workspace()
        .join(".rivo")
        .join("artifacts")
        .join("restart-owned-artifact");
    Store::open_runtime(&database)
        .unwrap()
        .save_artifact(&restart_owned)
        .unwrap();
    assert!(restart_path.exists());

    let expired = ArtifactStore::new(environment.workspace())
        .put(
            &ArtifactMetadata {
                artifact_id: "expired-startup-artifact".into(),
                node_id: node_id.into(),
                workspace_id: workspace_id.into(),
                session_id: session.clone(),
                kind: "text/plain".into(),
                expires_at: "2000-01-01T00:00:00Z".into(),
                download_scope: "session".into(),
            },
            b"expired-runtime-owned",
        )
        .unwrap();
    let expired_path = environment
        .workspace()
        .join(".rivo")
        .join("artifacts")
        .join("expired-startup-artifact");
    Store::open_runtime(&database)
        .unwrap()
        .save_artifact(&expired)
        .unwrap();
    assert!(expired_path.exists());

    let mut restarted = start_app_server(&environment);
    let mut restarted_stdin = restarted.stdin.take().unwrap();
    let mut restarted_stdout = BufReader::new(restarted.stdout.take().unwrap());
    let mut restarted_stderr = restarted.stderr.take().unwrap();
    writeln!(restarted_stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"cleanup-restart","version":"1"}}}}}}"#).unwrap();
    assert_eq!(read_rpc(&mut restarted_stdout)["id"], 1);
    let expired_state = Store::open_runtime(&database)
        .unwrap()
        .artifact_for_scope("expired-startup-artifact", node_id, workspace_id, &session)
        .unwrap()
        .unwrap();
    assert_eq!(expired_state.state, "deleted");
    assert!(!expired_path.exists());
    let restart_state = Store::open_runtime(&database)
        .unwrap()
        .artifact_for_scope("restart-owned-artifact", node_id, workspace_id, &session)
        .unwrap()
        .unwrap();
    assert_eq!(restart_state.state, "deleted");
    assert!(!restart_path.exists());
    drop(restarted_stdin);
    drop(restarted_stdout);
    assert!(restarted.wait().unwrap().success());
    diagnostics.clear();
    restarted_stderr.read_to_string(&mut diagnostics).unwrap();
    assert!(diagnostics.is_empty(), "{diagnostics}");
}

#[test]
fn e2e_app_server_streams_fake_provider_events_and_terminal_state() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(chat_stream("bridge-answer")),
        HttpResponse::sse(chat_stream("resumed-answer")),
    ])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, &base_url);
    configure_fixture(&environment, &base_url);
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"fixture","version":"1"}}}}}}"#).unwrap();
    assert_eq!(read_rpc(&mut stdout)["id"], 1);
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"workspace_write"}},"idempotency_key":"start-1"}}}}"#, serde_json::to_string(&workspace).unwrap()).unwrap();
    let start = read_rpc(&mut stdout);
    let session_ref = start["result"]["session_ref"].as_str().unwrap().to_owned();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"session_ref":{},"prompt":"return a fixture answer","runtime_settings":{{}},"idempotency_key":"turn-1"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();

    let mut start_response = None;
    let mut events = Vec::new();
    while start_response.is_none()
        || !events.iter().any(|event: &serde_json::Value| {
            event.pointer("/params/type")
                == Some(&serde_json::Value::String("turn_complete".into()))
        })
    {
        let message = read_rpc(&mut stdout);
        if message["id"] == 3 {
            start_response = Some(message);
        } else if message["method"] == "event" {
            events.push(message);
        }
    }
    let start_response = start_response.unwrap();
    let turn_id = start_response["result"]["turn_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(events.iter().any(|event| event.pointer("/params/type")
        == Some(&serde_json::Value::String("message_delta".into()))));
    assert!(events
        .iter()
        .any(|event| event.pointer("/params/payload/outcome")
            == Some(&serde_json::Value::String("completed".into()))));
    let sequences = events
        .iter()
        .map(|event| event["params"]["seq"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(events
        .iter()
        .all(|event| !event.to_string().contains("bridge-secret")));

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{{"session_ref":{},"prompt":"return a fixture answer","runtime_settings":{{}},"idempotency_key":"turn-1"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    let duplicate = loop {
        let message = read_rpc(&mut stdout);
        if message["id"] == 4 {
            break message;
        }
        assert_eq!(message["method"], "event");
    };
    assert_eq!(duplicate["result"]["turn_id"], turn_id, "{duplicate}");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"session_ref":{},"prompt":"different prompt","runtime_settings":{{}},"idempotency_key":"turn-1"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    let conflict = loop {
        let message = read_rpc(&mut stdout);
        if message["id"] == 5 {
            break message;
        }
        assert_eq!(message["method"], "event");
    };
    assert_eq!(conflict["error"]["code"], "invalid_request");

    drop(stdin);
    drop(stdout);
    assert!(child.wait().unwrap().success());
    let mut diagnostics = String::new();
    stderr.read_to_string(&mut diagnostics).unwrap();
    assert!(diagnostics.is_empty());

    let mut resumed = start_app_server(&environment);
    let mut resumed_stdin = resumed.stdin.take().unwrap();
    let mut resumed_stdout = BufReader::new(resumed.stdout.take().unwrap());
    let mut resumed_stderr = resumed.stderr.take().unwrap();
    writeln!(resumed_stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"fixture","version":"1"}}}}}}"#).unwrap();
    assert_eq!(read_rpc(&mut resumed_stdout)["id"], 1);
    writeln!(resumed_stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{{"session_ref":{},"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"workspace_write"}},"idempotency_key":"resume-1"}}}}"#, serde_json::to_string(&session_ref).unwrap(), serde_json::to_string(&workspace).unwrap()).unwrap();
    assert_eq!(
        read_rpc(&mut resumed_stdout)["result"]["session_ref"],
        session_ref
    );
    writeln!(resumed_stdin, r#"{{"jsonrpc":"2.0","id":5,"method":"session/replay","params":{{"session_ref":{},"from_seq":0}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    let replay = read_rpc(&mut resumed_stdout);
    assert_eq!(replay["id"], 5);
    let replay_events = replay["result"]["events"].as_array().unwrap();
    assert!(
        !replay_events.is_empty(),
        "durable replay returned no events"
    );
    assert!(replay_events
        .iter()
        .any(|event| event.pointer("/params/type")
            == Some(&serde_json::Value::String("message_delta".into()))));
    assert!(replay_events
        .iter()
        .any(|event| event.pointer("/params/type")
            == Some(&serde_json::Value::String("turn_complete".into()))));
    let replay_sequences = replay_events
        .iter()
        .map(|event| event["params"]["seq"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(replay_sequences.windows(2).all(|pair| pair[0] < pair[1]));
    // Reconnect must use the durable idempotency result, without another
    // provider request or tool execution; scope/payload conflicts stay closed.
    for (id, session, prompt, conflict) in [
        (9, session_ref.as_str(), "changed after restart", true),
        (8, session_ref.as_str(), "return a fixture answer", false),
        (10, "other-session", "return a fixture answer", true),
    ] {
        writeln!(resumed_stdin, "{}", serde_json::json!({
            "jsonrpc":"2.0", "id":id, "method":"turn/start", "params": {
                "session_ref":session, "prompt":prompt, "runtime_settings":{}, "idempotency_key":"turn-1"
            }
        })).unwrap();
        let response = loop {
            let message = read_rpc(&mut resumed_stdout);
            if message["id"] == id {
                break message;
            }
        };
        if conflict {
            assert_eq!(
                response["error"]["code"], "idempotency_conflict",
                "{response}"
            );
        } else {
            assert_eq!(response["result"]["turn_id"], turn_id, "{response}");
            assert_eq!(response["result"]["replayed"], true);
        }
    }
    writeln!(resumed_stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"session_ref":{},"prompt":"resume the fixture","runtime_settings":{{}},"idempotency_key":"turn-2"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    let mut resume_events = Vec::new();
    let mut resume_response = None;
    while resume_response.is_none()
        || !resume_events.iter().any(|event: &serde_json::Value| {
            event.pointer("/params/type")
                == Some(&serde_json::Value::String("turn_complete".into()))
        })
    {
        let message = read_rpc(&mut resumed_stdout);
        if message["id"] == 3 {
            resume_response = Some(message);
        } else if message["method"] == "event" {
            resume_events.push(message);
        }
    }
    let resumed_turn_id = resume_response.unwrap()["result"]["turn_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(resume_events
        .iter()
        .any(|event| event.pointer("/params/payload/content/text")
            == Some(&serde_json::Value::String("resumed-answer".into()))));
    writeln!(resumed_stdin, r#"{{"jsonrpc":"2.0","id":6,"method":"sidecar/task/read","params":{{"session_ref":{},"turn_id":{}}}}}"#, serde_json::to_string(&session_ref).unwrap(), serde_json::to_string(&resumed_turn_id).unwrap()).unwrap();
    let turn_read = loop {
        let message = read_rpc(&mut resumed_stdout);
        if message["id"] == 6 {
            break message;
        }
    };
    assert_eq!(
        turn_read["result"]["task"]["state"], "completed",
        "{turn_read}"
    );
    assert!(turn_read["result"]["task"]["task_id"].is_string());
    writeln!(resumed_stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"session/close","params":{{"session_ref":{},"idempotency_key":"close-1"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    loop {
        let message = read_rpc(&mut resumed_stdout);
        if message["id"] == 4 {
            assert_eq!(message["result"]["closed"], true);
            break;
        }
    }
    drop(resumed_stdin);
    drop(resumed_stdout);
    assert!(resumed.wait().unwrap().success());
    diagnostics.clear();
    resumed_stderr.read_to_string(&mut diagnostics).unwrap();
    assert!(diagnostics.is_empty());
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn e2e_app_server_memory_mutations_are_scoped_and_hash_bound() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let workspace = environment.workspace().display().to_string();
    let canonical_workspace = std::fs::canonicalize(environment.workspace()).unwrap();
    let other_workspace = environment.root().join("other-workspace");
    std::fs::create_dir_all(&other_workspace).unwrap();
    let other_workspace = std::fs::canonicalize(other_workspace).unwrap();
    configure_fixture(&environment, "http://127.0.0.1:1/v1");
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"memory-fixture","version":"1"}}}}}}"#).unwrap();
    assert_eq!(read_rpc(&mut stdout)["id"], 1);
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{}},"idempotency_key":"memory-start"}}}}"#, serde_json::to_string(&workspace).unwrap()).unwrap();
    let session_ref = read_rpc(&mut stdout)["result"]["session_ref"]
        .as_str()
        .unwrap()
        .to_owned();

    let candidate_body = "candidate body";
    let candidate_hash = hex::encode(Sha256::digest(candidate_body.as_bytes()));
    let rejected_body = "candidate to reject";
    let rejected_hash = hex::encode(Sha256::digest(rejected_body.as_bytes()));
    let other_body = "other workspace candidate";
    let other_hash = hex::encode(Sha256::digest(other_body.as_bytes()));
    {
        let store = Store::open(environment.data_dir().join("rivo").join("rivo.db")).unwrap();
        store
            .save_memory(&Memory {
                id: "memory-own".into(),
                workspace: canonical_workspace.display().to_string(),
                mode: "profile".into(),
                body: "own memory".into(),
                source: "fixture".into(),
            })
            .unwrap();
        store
            .save_memory(&Memory {
                id: "memory-other".into(),
                workspace: other_workspace.display().to_string(),
                mode: "profile".into(),
                body: "other memory".into(),
                source: "fixture".into(),
            })
            .unwrap();
        for (id, body, body_sha256, workspace) in [
            (
                "candidate-own",
                candidate_body,
                candidate_hash.as_str(),
                canonical_workspace.display().to_string(),
            ),
            (
                "candidate-reject",
                rejected_body,
                rejected_hash.as_str(),
                canonical_workspace.display().to_string(),
            ),
            (
                "candidate-other",
                other_body,
                other_hash.as_str(),
                other_workspace.display().to_string(),
            ),
        ] {
            store
                .save_memory_candidate(&MemoryCandidate {
                    id: id.into(),
                    workspace,
                    mode: "profile".into(),
                    body: body.into(),
                    source: "fixture".into(),
                    body_sha256: body_sha256.into(),
                })
                .unwrap();
        }
    }

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"memory/list","params":{{"session_ref":{}}}}}"#,
        serde_json::to_string(&session_ref).unwrap()
    )
    .unwrap();
    let listed = read_rpc(&mut stdout);
    assert_eq!(
        listed["result"]["workspace"],
        canonical_workspace.display().to_string()
    );
    assert_eq!(listed["result"]["memories"].as_array().unwrap().len(), 1);
    assert_eq!(listed["result"]["memories"][0]["memory_id"], "memory-own");
    let candidates = listed["result"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(candidates
        .iter()
        .all(|candidate| candidate["candidate_id"] != "candidate-other"));

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"memory/confirm","params":{{"session_ref":{},"candidate_id":"candidate-other","body_sha256":{},"idempotency_key":"memory-confirm-other"}}}}"#, serde_json::to_string(&session_ref).unwrap(), serde_json::to_string(&other_hash).unwrap()).unwrap();
    assert_eq!(read_rpc(&mut stdout)["error"]["code"], "ownership_mismatch");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":5,"method":"memory/confirm","params":{{"session_ref":{},"candidate_id":"candidate-own","body_sha256":"wrong-hash","idempotency_key":"memory-confirm-wrong"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    assert_eq!(read_rpc(&mut stdout)["error"]["code"], "memory_not_found");

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":6,"method":"memory/confirm","params":{{"session_ref":{},"candidate_id":"candidate-own","body_sha256":{},"idempotency_key":"memory-confirm-own"}}}}"#, serde_json::to_string(&session_ref).unwrap(), serde_json::to_string(&candidate_hash).unwrap()).unwrap();
    let confirmed = read_rpc(&mut stdout);
    assert_eq!(confirmed["result"]["confirmed"], true);
    assert_eq!(confirmed["result"]["memory"]["memory_id"], "candidate-own");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":7,"method":"memory/reject","params":{{"session_ref":{},"candidate_id":"candidate-reject","idempotency_key":"memory-reject"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    assert_eq!(read_rpc(&mut stdout)["result"]["rejected"], true);
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":8,"method":"memory/delete","params":{{"session_ref":{},"memory_id":"candidate-own","idempotency_key":"memory-delete-own"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    assert_eq!(read_rpc(&mut stdout)["result"]["deleted"], true);
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":9,"method":"memory/delete","params":{{"session_ref":{},"memory_id":"memory-other","idempotency_key":"memory-delete-other"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
    assert_eq!(read_rpc(&mut stdout)["error"]["code"], "memory_not_found");

    drop(stdin);
    drop(stdout);
    assert!(child.wait().unwrap().success());
    let mut diagnostics = String::new();
    stderr.read_to_string(&mut diagnostics).unwrap();
    assert!(diagnostics.is_empty());
}

#[test]
fn e2e_app_server_interrupt_cancels_a_stalled_provider_turn_once() {
    let _guard = lock_app_server_test();
    for expire_lease in [false, true] {
        let environment = schema3_environment();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok(_) => {
                        thread::sleep(Duration::from_secs(2));
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });
        let base_url = format!("http://{address}/v1");
        let workspace = environment.workspace().display().to_string();
        configure_fixture(&environment, &base_url);
        run(&environment, &["config", "set", "workspace", &workspace]).assert_success();
        let mut child = start_app_server(&environment);
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut stderr = child.stderr.take().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"fixture","version":"1"}}}}}}"#).unwrap();
        assert_eq!(read_rpc(&mut stdout)["id"], 1);
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{}},"idempotency_key":"start"}}}}"#, serde_json::to_string(&workspace).unwrap()).unwrap();
        let session_ref = read_rpc(&mut stdout)["result"]["session_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"session_ref":{},"prompt":"stall","runtime_settings":{{}},"idempotency_key":"turn"}}}}"#, serde_json::to_string(&session_ref).unwrap()).unwrap();
        let mut turn_id = None;
        while turn_id.is_none() {
            let message = read_rpc(&mut stdout);
            if message["id"] == 3 {
                turn_id = message["result"]["turn_id"].as_str().map(ToOwned::to_owned);
            }
        }
        let turn_id = turn_id.unwrap();
        if expire_lease {
            let database = environment.data_dir().join("rivo/rivo.db");
            let connection = rusqlite::Connection::open(database).unwrap();
            connection
                .execute(
                    "UPDATE sidecar_leases SET expires_at='2000-01-01T00:00:00Z'",
                    [],
                )
                .unwrap();
        } else {
            writeln!(stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"turn/interrupt","params":{{"session_ref":{},"turn_id":{},"idempotency_key":"interrupt"}}}}"#, serde_json::to_string(&session_ref).unwrap(), serde_json::to_string(&turn_id).unwrap()).unwrap();
        }
        let mut interrupt_accepted = expire_lease;
        let mut terminal = Vec::new();
        while !interrupt_accepted || terminal.is_empty() {
            let message = read_rpc(&mut stdout);
            if message["id"] == 4 {
                interrupt_accepted = message["result"]["accepted"] == true;
            }
            if message.pointer("/params/type")
                == Some(&serde_json::Value::String("turn_complete".into()))
            {
                terminal.push(message);
            }
        }
        assert!(interrupt_accepted);
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0]["params"]["payload"]["outcome"], "interrupted");
        if expire_lease {
            writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":5,"method":"sidecar/task/read","params":{"session_ref":session_ref,"turn_id":turn_id}})).unwrap();
            let task = loop {
                let message = read_rpc(&mut stdout);
                if message["id"] == 5 {
                    break message;
                }
            };
            assert_eq!(task["result"]["task"]["state"], "expired", "{task}");
            assert_eq!(
                task["result"]["task"]["terminal_reason"], "lease_expired",
                "{task}"
            );
        }
        drop(stdin);
        drop(stdout);
        assert!(child.wait().unwrap().success());
        let mut diagnostics = String::new();
        stderr.read_to_string(&mut diagnostics).unwrap();
        assert!(diagnostics.is_empty());
        server.join().unwrap();
    }
}

fn chat_stream(text: &str) -> String {
    format!(
        concat!(
            "data: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture-model\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":{text:?}}},\"finish_reason\":null}}]}}\n\n",
            "data: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture-model\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}}}\n\n",
            "data: [DONE]\n\n"
        ),
        text = text
    )
}

fn write_file_tool_stream(path: &str, content: &str) -> String {
    tool_call_stream(
        "call-write",
        "write_file",
        serde_json::json!({
            "path": path,
            "content": content,
        }),
    )
}

fn tool_call_stream(id: &str, name: &str, arguments: serde_json::Value) -> String {
    let arguments = arguments.to_string();
    let requested = serde_json::json!({
        "id": "fixture",
        "object": "chat.completion.chunk",
        "model": "fixture-model",
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]},
            "finish_reason": null
        }]
    });
    let completed = serde_json::json!({
        "id": "fixture",
        "object": "chat.completion.chunk",
        "model": "fixture-model",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    format!("data: {requested}\n\ndata: {completed}\n\ndata: [DONE]\n\n")
}

fn canonical_write_file_hash(path: &str, content: &str) -> String {
    let canonical = format!(
        r#"{{"content":{},"path":{}}}"#,
        serde_json::to_string(content).unwrap(),
        serde_json::to_string(path).unwrap(),
    );
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

#[test]
fn e2e_cli_001_help_version_and_usage_have_stable_exit_codes() {
    let environment = schema3_environment();

    let help = run(&environment, &["--help"]);
    help.assert_success();
    assert!(help.stdout.contains("Rivo"));
    assert!(help.stdout.contains("--provider"));
    assert!(!help.stdout.contains("supervision"));

    let version = run(&environment, &["--version"]);
    version.assert_success();
    assert!(version.stdout.contains(env!("RIVO_RELEASE_VERSION")));

    let missing = run(&environment, &[]);
    missing.assert_success();
    assert!(missing.stdout.contains("Usage:"));
    assert!(missing.stderr.is_empty());

    let run_help = run(&environment, &["run"]);
    run_help.assert_success();
    assert!(run_help.stdout.contains("Usage: rivo run"));
    assert!(run_help.stdout.contains("--prompt <PROMPT>"));
    assert!(run_help.stderr.is_empty());

    let positional = run(&environment, &["legacy task"]);
    assert_eq!(positional.status, 2);
    assert!(positional.stderr.contains("未知命令"));

    let run_positional = run(&environment, &["run", "legacy task"]);
    assert_eq!(run_positional.status, 2);
    assert!(run_positional.stderr.contains("unexpected argument"));

    let invalid = run(&environment, &["--provider", "unknown", "--prompt", "task"]);
    assert_eq!(invalid.status, 2);
    assert!(invalid.stderr.contains("unexpected argument"));

    for (command, expected_help) in [
        (vec!["config", "--help"], "Usage: rivo config"),
        (vec!["language", "--help"], "Usage: rivo language"),
        (vec!["mcp", "--help"], "Usage: rivo mcp"),
        (vec!["sessions", "--help"], "rivo sessions"),
        (vec!["diagnostics", "--help"], "Usage: rivo diagnostics"),
        (vec!["upgrade", "--help"], "Usage: rivo upgrade"),
        (vec!["tui", "--help"], "Usage: rivo tui"),
        (vec!["instance", "--help"], "Usage: rivo instance"),
        (vec!["app-server", "--help"], "Usage: rivo app-server"),
    ] {
        let output = run(&environment, &command);
        output.assert_success();
        assert!(
            output.stdout.contains(expected_help),
            "{command:?}: {}",
            output.stdout
        );
    }

    let tui_extra = run(&environment, &["tui", "unexpected"]);
    assert_eq!(tui_extra.status, 2);
    assert!(tui_extra.stderr.contains("Usage: rivo tui"));

    let tui_help = run(&environment, &["tui", "--help"]);
    tui_help.assert_success();
    assert!(tui_help.stdout.contains("--workspace <PATH>"));

    let tui_non_tty = run(&environment, &["tui"]);
    assert_eq!(tui_non_tty.status, 2);
    assert!(tui_non_tty
        .stderr
        .contains("rivo tui requires an interactive terminal"));

    let tui_workspace_non_tty = run(&environment, &["tui", "--workspace", "."]);
    assert_eq!(tui_workspace_non_tty.status, 2);
    assert!(tui_workspace_non_tty
        .stderr
        .contains("rivo tui requires an interactive terminal"));
}

#[test]
fn e2e_language_files_are_validated_installed_backed_up_and_removed() {
    let environment = schema3_environment();
    let help = run(&environment, &["language", "--help"]);
    help.assert_success();
    for command in [
        "path",
        "list",
        "show",
        "validate",
        "install",
        "remove",
        "remote-list",
        "download",
        "update",
        "rollback",
    ] {
        assert!(help.stdout.contains(command));
    }

    let source = environment.root().join("source.ftl");
    let language = |value: &str| {
        format!(
            "### rivo-language-schema: 1\n### locale: fr-FR\n### display-name: Français\n### revision: test.1\n### min-rivo-version: 0.1.0\n\ncommon-ok = {value}\n"
        )
    };
    std::fs::write(&source, language("D'accord")).unwrap();

    let path = run(&environment, &["language", "path"]);
    path.assert_success();
    let language_dir = environment.data_dir().join("rivo").join("languages");
    assert_eq!(path.stdout.trim(), language_dir.display().to_string());

    let initial = run(&environment, &["language", "list"]);
    initial.assert_success();
    assert!(initial.stdout.contains("en-US\tEnglish (United States)"));
    assert!(initial.stdout.contains("zh-CN\t简体中文"));

    let source_path = source.to_string_lossy().to_string();
    run(&environment, &["language", "install", &source_path]).assert_success();
    let installed_path = language_dir.join("fr-FR.ftl");
    assert!(installed_path.is_file());

    let shown = run(&environment, &["language", "show", "fr-FR"]);
    shown.assert_success();
    assert!(shown.stdout.contains("fr-FR\tFrançais"));
    assert!(shown.stdout.contains("source=user"));

    let installed_path_text = installed_path.to_string_lossy().to_string();
    run(
        &environment,
        &["language", "validate", &installed_path_text],
    )
    .assert_success();

    let rejected = run(&environment, &["language", "install", &source_path]);
    rejected.assert_failure();
    assert!(rejected.stderr.contains("--yes"));

    std::fs::write(&source, language("Oui")).unwrap();
    run(
        &environment,
        &["language", "install", &source_path, "--yes"],
    )
    .assert_success();
    let backup = language_dir.join("fr-FR.ftl.bak");
    assert!(std::fs::read_to_string(&backup)
        .unwrap()
        .contains("D'accord"));
    assert!(std::fs::read_to_string(&installed_path)
        .unwrap()
        .contains("Oui"));

    let rejected_remove = run(&environment, &["language", "remove", "fr-FR"]);
    rejected_remove.assert_failure();
    assert!(rejected_remove.stderr.contains("--yes"));
    run(&environment, &["language", "remove", "fr-FR", "--yes"]).assert_success();
    assert!(!installed_path.exists());
    assert!(backup.exists());
}

#[test]
fn e2e_cli_command_help_and_usage_boundaries_are_stable() {
    let environment = schema3_environment();
    for (args, marker) in [
        (&["a2a", "--help"][..], "rivo a2a"),
        (&["a2a", "delegate", "--help"][..], "--message-stdin"),
        (&["a2a", "pairings", "approve", "--help"][..], "REQUEST_ID"),
        (&["config", "--help"][..], "config"),
        (&["mcp", "--help"][..], "mcp"),
        (&["mcp", "install", "http", "--help"][..], "--header-env"),
        (&["sessions", "--help"][..], "sessions"),
        (&["sessions", "delete", "--help"][..], "--yes"),
        (&["diagnostics", "--help"][..], "diagnostics"),
        (&["diagnostics", "create", "--help"][..], "<PATH>"),
    ] {
        let output = run(&environment, args);
        output.assert_success();
        assert!(output.stderr.is_empty(), "{args:?}: {}", output.stderr);
        assert!(
            output.stdout.contains(marker),
            "{args:?}: {}",
            output.stdout
        );
    }

    for args in [
        &["config", "unknown"][..],
        &["sessions", "unknown"][..],
        &["diagnostics", "unknown"][..],
    ] {
        let output = run(&environment, args);
        assert_eq!(output.status, 2, "{args:?}: {output:?}");
        assert!(!output.stderr.trim().is_empty(), "{args:?}");
    }
}

#[test]
fn e2e_config_help_is_hierarchical_and_side_effect_free() {
    let environment = schema3_environment();
    for (args, markers) in [
        (
            &["config", "--help"][..],
            &["Usage: rivo config", "Commands:", "provider"][..],
        ),
        (
            &["config", "provider", "--help"][..],
            &["Usage: rivo config provider <list|show|set|remove> [ARGS]"][..],
        ),
        (
            &["config", "provider", "set", "--help"][..],
            &["Usage: rivo config provider set <ID>"][..],
        ),
        (
            &["config", "vendor", "--help"][..],
            &["Usage: rivo config vendor"][..],
        ),
        (
            &["config", "model", "--help"][..],
            &["Usage: rivo config model"][..],
        ),
        (
            &["config", "credentials", "set", "--help"][..],
            &["Usage: rivo config credentials set", "--from-env <ENV>"][..],
        ),
    ] {
        let output = run(&environment, args);
        output.assert_success();
        assert!(output.stderr.is_empty(), "{args:?}: {}", output.stderr);
        for marker in markers {
            assert!(
                output.stdout.contains(marker),
                "{args:?} missing {marker:?}: {}",
                output.stdout
            );
        }
        assert!(
            output.stdout.contains("Show help"),
            "{args:?}: {}",
            output.stdout
        );
        assert!(
            !output.stdout.contains("Print help"),
            "{args:?}: {}",
            output.stdout
        );
        assert!(
            !output.stdout.contains("\n  help "),
            "{args:?}: {}",
            output.stdout
        );
    }

    let missing = run(&environment, &["config", "provider", "set", "local"]);
    assert_eq!(missing.status, 2);
    assert!(missing.stdout.is_empty());
    assert!(missing.stderr.contains("缺少 --base-url"));
    let legacy = run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "local",
            "--protocol",
            "openai_responses",
            "--base-url",
            "http://127.0.0.1:18080/v1",
        ],
    );
    assert_eq!(legacy.status, 2);
    assert!(legacy.stderr.contains("Provider 不再携带协议"));
    assert!(legacy.stderr.contains("--enable-protocol"));
}

#[test]
fn e2e_command_short_help_aliases_are_stable() {
    let environment = schema3_environment();
    for command in [
        "config",
        "mcp",
        "a2a",
        "instance",
        "sessions",
        "diagnostics",
        "upgrade",
        "tui",
        "app-server",
    ] {
        let output = run(&environment, &[command, "-h"]);
        output.assert_success();
        assert!(
            output.stdout.contains("Usage:"),
            "{command}: {}",
            output.stdout
        );
        assert!(output.stderr.is_empty(), "{command}: {}", output.stderr);
    }
}

#[test]
fn e2e_config_default_subcommands_remain_compatible() {
    let environment = schema3_environment();
    for (implicit, explicit) in [
        (&["config"][..], &["config", "show"][..]),
        (&["config", "vendor"][..], &["config", "vendor", "list"][..]),
        (
            &["config", "credentials"][..],
            &["config", "credentials", "show"][..],
        ),
        (
            &["config", "provider"][..],
            &["config", "provider", "list"][..],
        ),
    ] {
        let implicit_output = run(&environment, implicit);
        let explicit_output = run(&environment, explicit);
        implicit_output.assert_success();
        explicit_output.assert_success();
        assert_eq!(
            implicit_output.stdout, explicit_output.stdout,
            "{implicit:?}"
        );
        assert_eq!(
            implicit_output.stderr, explicit_output.stderr,
            "{implicit:?}"
        );
    }
}

#[test]
fn e2e_cli_002_config_provider_and_credentials_round_trip() {
    let environment = schema3_environment();
    let workspace = environment.workspace().display().to_string();
    set_test_credential(&environment, "fixture-key");

    for args in [
        vec!["config", "set", "profile", "research"],
        vec!["config", "set", "policy", "strict"],
        vec!["config", "set", "workspace", workspace.as_str()],
        vec![
            "config",
            "provider",
            "set",
            "fixture",
            "--base-url",
            "http://127.0.0.1:18080/v1",
            "--credential-name",
            "fixture-key",
        ],
        vec!["config", "set", "provider_id", "fixture"],
    ] {
        run(&environment, &args).assert_success();
    }

    let show = environment
        .command(binary())
        .args(["config", "show"])
        .env("RIVO_FIXTURE_KEY", "fixture-secret-value")
        .run()
        .unwrap();
    show.assert_success();
    assert!(show.stdout.contains("provider_id = \"fixture\""));
    assert!(show.stdout.contains("capability = \"research\""));
    assert!(show.stdout.contains("policy = \"strict\""));
    assert_redacted(&show.stdout, &["fixture-secret-value"]);

    let profile = run(&environment, &["config", "provider", "show", "fixture"]);
    profile.assert_success();
    assert!(!profile.stdout.contains("protocol ="));
    assert!(profile
        .stdout
        .contains("base_url = \"http://127.0.0.1:18080/v1\""));

    let credential = environment
        .command(binary())
        .args([
            "config",
            "credentials",
            "set",
            "release-key",
            "--from-env",
            "RIVO_CREDENTIAL_SOURCE",
        ])
        .env("RIVO_CREDENTIAL_SOURCE", "credential-secret-value")
        .run()
        .unwrap();
    credential.assert_success();
    assert_redacted(
        &format!("{}{}", credential.stdout, credential.stderr),
        &["credential-secret-value"],
    );

    let credentials = run(&environment, &["config", "credentials", "show"]);
    credentials.assert_success();
    assert!(credentials.stdout.contains("release-key=encrypted"));
    assert_redacted(&credentials.stdout, &["credential-secret-value"]);

    let rejected = run(&environment, &["config", "provider", "remove", "fixture"]);
    rejected.assert_failure();
    assert!(!rejected.stdout.contains("confirmation_token"));
    assert!(rejected.stderr.contains("--yes"));
    assert!(environment.config_path().is_file());
}

#[test]
fn e2e_cli_007_config_lifecycle_covers_mutable_defaults_and_cleanup() {
    let environment = schema3_environment();
    let workspace = environment.workspace().display().to_string();

    for args in [
        vec!["config", "set", "profile", "coding"],
        vec!["config", "set", "policy", "medium"],
        vec!["config", "set", "workspace", workspace.as_str()],
        vec!["config", "set", "budget.max_steps", "12"],
        vec!["config", "set", "budget.max_retries", "2"],
        vec!["config", "set", "budget.max_duration_seconds", "90"],
        vec!["config", "set", "budget.context_limit_tokens", "4096"],
        vec!["config", "set", "budget.max_observation_bytes", "8192"],
        vec!["config", "set", "logging.required", "true"],
        vec!["config", "set", "logging.retention_days", "3"],
        vec![
            "config",
            "provider",
            "set",
            "temporary",
            "--base-url",
            "https://example.test/v1",
        ],
    ] {
        let output = run(&environment, &args);
        assert_eq!(output.status, 0, "{args:?}: {output:?}");
    }

    let listed = run(&environment, &["config", "provider", "list"]);
    listed.assert_success();
    assert!(listed.stdout.contains("https://example.test/v1"));

    remove_provider(&environment, "temporary");

    let credential = environment
        .command(binary())
        .args([
            "config",
            "credentials",
            "set",
            "temporary-key",
            "--from-env",
            "RIVO_TEMPORARY_CREDENTIAL",
        ])
        .env("RIVO_TEMPORARY_CREDENTIAL", "temporary-secret-value")
        .run()
        .unwrap();
    credential.assert_success();
    run(&environment, &["config", "credentials", "migrate"]).assert_success();
    run(
        &environment,
        &["config", "credentials", "unset", "temporary-key"],
    )
    .assert_success();

    for args in [
        vec!["config", "unset", "logging.required"],
        vec!["config", "unset", "logging.retention_days"],
        vec!["config", "unset", "budget.max_steps"],
        vec!["config", "unset", "budget.max_retries"],
        vec!["config", "unset", "budget.max_duration_seconds"],
        vec!["config", "unset", "budget.context_limit_tokens"],
        vec!["config", "unset", "budget.max_observation_bytes"],
        vec!["config", "unset", "workspace"],
        vec!["config", "unset", "policy"],
        vec!["config", "unset", "profile"],
    ] {
        run(&environment, &args).assert_success();
    }
    let unknown = run(&environment, &["config", "set", "unknown.option", "value"]);
    unknown.assert_failure();
    assert!(unknown.stderr.contains("Unsupported configuration field"));
}

#[test]
fn e2e_cli_013_provider_management_preserves_independent_connections_and_errors() {
    let environment = schema3_environment();
    set_test_credential(&environment, "named-key");
    set_test_credential(&environment, "generated-key");
    let before_missing = std::fs::read(environment.config_path()).unwrap();
    let missing = run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "missing",
            "--base-url",
            "https://gateway.example/v1",
            "--credential-name",
            "missing-key",
        ],
    );
    missing.assert_failure();
    assert!(missing.stderr.contains("Credential missing-key 不存在"));
    assert_eq!(
        std::fs::read(environment.config_path()).unwrap(),
        before_missing
    );
    run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "named",
            "--base-url",
            "https://gateway.example/v1",
            "--credential-name",
            "named-key",
        ],
    )
    .assert_success();
    let blocked_unset = run(
        &environment,
        &["config", "credentials", "unset", "named-key"],
    );
    blocked_unset.assert_failure();
    assert!(blocked_unset.stderr.contains("named"));
    run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "generated",
            "--base-url",
            "https://gateway.example/v1",
            "--credential-name",
            "generated-key",
        ],
    )
    .assert_success();

    let listed = run(&environment, &["config", "provider", "list"]);
    listed.assert_success();
    assert!(listed.stdout.contains("named"));
    assert!(listed.stdout.contains("generated"));
    let generated = run(&environment, &["config", "provider", "show", "generated"]);
    generated.assert_success();
    assert!(generated
        .stdout
        .contains("base_url = \"https://gateway.example/v1\""));

    let referenced = run(&environment, &["config", "provider", "remove", "named"]);
    referenced.assert_failure();
    assert!(!referenced.stdout.contains("confirmation_token"));
    assert!(referenced.stderr.contains("--yes"));
    let malformed = run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "broken",
            "--base-url",
            "https://a.example/v1",
            "--unknown",
            "value",
        ],
    );
    malformed.assert_failure();
    assert!(malformed.stderr.contains("未知参数: --unknown"));
    remove_provider(&environment, "named");
    assert!(run(&environment, &["config", "provider", "list"])
        .stdout
        .contains("generated"));
    remove_provider(&environment, "generated");
    assert!(run(&environment, &["config", "provider", "list"])
        .stdout
        .is_empty());
}

#[test]
fn e2e_cli_008_mcp_registry_commands_use_configured_toolsets() {
    let environment = schema3_environment();
    let server = r#"{"name":"docs","enabled":false,"transport":{"transport":"stdio","program":"fixture-mcp","args":[],"cwd":null},"allowed_capabilities":["tools","resources"],"toolsets":{"readonly":{"tools":["search_docs"],"risk":"read_only"}}}"#;
    let set = run(&environment, &["config", "set", "mcp.server", server]);
    set.assert_success();

    let list = run(&environment, &["mcp", "list"]);
    list.assert_success();
    assert!(list.stdout.contains("docs"));
    assert!(list.stdout.contains("toolsets=readonly"));

    let show = run(&environment, &["mcp", "show", "docs"]);
    show.assert_success();
    assert!(show.stdout.contains("enabled=false"));
    assert!(show
        .stdout
        .contains("toolset.readonly=risk:read_only,tools:search_docs"));

    let disabled = run(&environment, &["mcp", "test", "docs"]);
    disabled.assert_failure();
    assert!(disabled.stderr.contains("MCP server is disabled"));

    run(&environment, &["config", "unset", "mcp.server", "docs"]).assert_success();
    let missing = run(&environment, &["mcp", "show", "docs"]);
    missing.assert_failure();
    assert!(missing.stderr.contains("MCP server not found"));
}

#[test]
fn e2e_cli_010_mcp_lifecycle_and_validation_are_bounded() {
    let environment = schema3_environment();
    let server = r#"{"name":"docs","enabled":false,"transport":{"transport":"stdio","program":"fixture-mcp","args":[],"cwd":null},"allowed_capabilities":["tools"],"toolsets":{"readonly":{"tools":["search_docs"],"risk":"read_only"}}}"#;
    run(&environment, &["config", "set", "mcp.server", server]).assert_success();

    run(&environment, &["mcp", "enable", "docs"]).assert_success();
    let enabled = run(&environment, &["mcp", "show", "docs"]);
    enabled.assert_success();
    assert!(enabled.stdout.contains("enabled=true"));
    assert!(enabled.stdout.contains("transport=stdio"));

    run(&environment, &["mcp", "disable", "docs"]).assert_success();
    let disabled = run(&environment, &["mcp", "show", "docs"]);
    disabled.assert_success();
    assert!(disabled.stdout.contains("enabled=false"));

    let inspect_disabled = run(&environment, &["mcp", "inspect", "docs"]);
    assert_eq!(inspect_disabled.status, 2);
    assert!(inspect_disabled.stderr.contains("MCP server is disabled"));

    let unknown = run(&environment, &["mcp", "enable", "missing"]);
    assert_eq!(unknown.status, 2);
    assert!(unknown.stderr.contains("MCP server not found"));

    let invalid = run(&environment, &["mcp", "install", "stdio", "docs"]);
    assert_eq!(invalid.status, 2);
    assert!(invalid.stderr.contains("Usage:"));

    run(&environment, &["mcp", "remove", "docs"]).assert_success();
    let removed = run(&environment, &["mcp", "list"]);
    removed.assert_success();
    assert!(!removed.stdout.contains("docs"));
}

#[test]
fn e2e_cli_mcp_inspect_discovers_all_declared_capabilities_over_loopback() {
    // Exercise the actual CLI command against a deterministic Streamable HTTP
    // fixture.  This proves that inspection is bounded to the configured
    // capabilities and that an explicitly allowed local endpoint works
    // without a browser, keychain, or external network.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = thread::spawn(move || {
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            let (status, body) = if request.contains("notifications/initialized") {
                ("202 Accepted", String::new())
            } else if request.contains("tools/list") {
                (
                    "200 OK",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"fixture tool","inputSchema":{"type":"object"}}]}}"#.into(),
                )
            } else if request.contains("resources/list") {
                (
                    "200 OK",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resources":[{"uri":"memory://fixture","name":"fixture resource"}]}}"#.into(),
                )
            } else if request.contains("prompts/list") {
                (
                    "200 OK",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"prompts":[{"name":"summarize","description":"fixture prompt"}]}}"#.into(),
                )
            } else {
                (
                    "200 OK",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}"#.into(),
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    let environment = schema3_environment();
    let server = serde_json::json!({
        "name": "fixture",
        "enabled": true,
        "transport": {
            "transport": "streamable_http",
            "url": format!("http://{address}/mcp"),
            "header_env": {},
            "auth": {"kind": "none"},
            "allow_loopback": true,
        },
        "allowed_capabilities": ["tools", "resources", "prompts"],
        "toolsets": {"read": {"tools": ["echo"], "risk": "read_only"}},
        "allow_sampling": false,
    })
    .to_string();
    run(&environment, &["config", "set", "mcp.server", &server]).assert_success();

    let inspected = run(&environment, &["mcp", "inspect", "fixture"]);
    inspected.assert_success();
    assert!(inspected
        .stdout
        .contains("server=fixture status=ready tools=1 resources=1 prompts=1"));
    assert!(inspected.stdout.contains("tool=echo\tfixture tool"));
    assert!(inspected
        .stdout
        .contains("resource=memory://fixture\tfixture resource"));
    assert!(inspected
        .stdout
        .contains("prompt=summarize\tfixture prompt"));
    fixture.join().unwrap();
}

#[test]
fn e2e_cli_mcp_install_discovers_and_persists_declared_toolsets() {
    let (url, fixture) = start_loopback_mcp_fixture();
    let environment = schema3_environment();
    let installed = run(
        &environment,
        &[
            "mcp",
            "install",
            "http",
            "fixture",
            &url,
            "--capability",
            "tools,resources,prompts",
            "--toolset",
            "read:read_only:echo",
            "--allow-loopback",
        ],
    );
    installed.assert_success();
    assert!(installed
        .stdout
        .contains("installed MCP server fixture; status=ready tools=1 resources=1 prompts=1"));
    let shown = run(&environment, &["mcp", "show", "fixture"]);
    shown.assert_success();
    assert!(shown
        .stdout
        .contains("toolset.read=risk:read_only,tools:echo"));
    assert!(shown.stdout.contains("allow_loopback=true"));
    fixture.join().unwrap();
}

#[test]
fn e2e_cli_diagnostics_create_is_explicit_and_excludes_database_bytes() {
    let environment = schema3_environment();
    let destination = environment.root().join("diagnostics.json");
    let destination_text = destination.to_string_lossy().into_owned();

    let created = run(
        &environment,
        &["diagnostics", "create", &destination_text, "--yes"],
    );
    created.assert_success();
    assert!(created.stdout.contains("created"));
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&destination).unwrap()).unwrap();
    assert_eq!(bundle["database_included"], false);
    assert_eq!(bundle["manifest"]["redaction"], "known_api_keys_masked_v1");
    assert!(bundle["journal"]["sessions"].is_array());
}

#[test]
fn e2e_cli_diagnostics_exports_health_when_schema_is_incompatible() {
    let environment = schema3_environment();
    let database_directory = environment.data_dir().join("rivo");
    std::fs::create_dir_all(&database_directory).unwrap();
    let database = database_directory.join("rivo.db");
    std::fs::write(&database, []).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("CREATE TABLE schema_version(version INTEGER NOT NULL); INSERT INTO schema_version VALUES(14);")
        .unwrap();
    drop(connection);

    let destination = environment.root().join("incompatible-diagnostics.json");
    let destination_text = destination.to_string_lossy().into_owned();
    let created = run(
        &environment,
        &["diagnostics", "create", &destination_text, "--yes"],
    );
    created.assert_success();
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(destination).unwrap()).unwrap();
    assert_eq!(bundle["storage_health"]["schema_version"], 14);
    assert_eq!(bundle["journal"]["omitted"], true);
    assert_eq!(bundle["database_included"], false);
}

#[test]
fn e2e_cli_011_management_failures_are_explicit_and_non_destructive() {
    let environment = schema3_environment();

    for args in [
        vec!["config", "set", "budget.max_steps", "0"],
        vec!["config", "set", "logging.retention_days", "0"],
        vec![
            "config",
            "credentials",
            "set",
            "test-key",
            "--from-env",
            "MISSING_KEY",
        ],
        vec!["config", "provider", "set", "broken"],
        vec!["sessions", "show"],
        vec!["sessions", "delete", "missing"],
        vec!["diagnostics", "create"],
        vec!["mcp", "show"],
    ] {
        let output = run(&environment, &args);
        assert_eq!(output.status, 2, "{args:?}: {}", output.stderr);
        assert!(!output.stderr.is_empty(), "{args:?}");
    }

    let path = run(&environment, &["sessions", "path"]);
    path.assert_success();
    assert!(path.stdout.contains("rivo.db"));

    let list = run(&environment, &["sessions", "list"]);
    list.assert_success();
    assert!(list.stdout.is_empty());
}

#[test]
fn e2e_cli_012_model_discovery_failure_is_cached_without_leaking_credentials() {
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![HttpResponse {
        status: 404,
        content_type: "application/json".into(),
        body: r#"{"error":{"message":"model endpoint unavailable"}}"#.into(),
    }])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());
    let configured = environment
        .command(binary())
        .args([
            "config",
            "provider",
            "set",
            "fixture",
            "--base-url",
            &base_url,
        ])
        .run()
        .unwrap();
    configured.assert_success();

    let refresh = environment
        .command(binary())
        .args(["config", "provider", "models", "refresh", "fixture"])
        .env("RIVO_FIXTURE_KEY", "discovery-secret-value")
        .run()
        .unwrap();
    assert_eq!(refresh.status, 2);
    assert!(refresh.stderr.contains("Model discovery failed"));
    assert_redacted(
        &format!("{}{}", refresh.stdout, refresh.stderr),
        &["discovery-secret-value"],
    );

    let shown = run(&environment, &["config", "provider", "show", "fixture"]);
    shown.assert_success();
    assert!(shown.stdout.contains("supported = false"));
    assert!(shown.stdout.contains("last_error ="));
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn e2e_cli_009_provider_lifecycle_confirms_default_deletion() {
    let environment = schema3_environment();
    set_test_credential(&environment, "local-fixture-credential");
    let created = run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "managed",
            "--base-url",
            "https://gateway.example/v1",
            "--credential-name",
            "local-fixture-credential",
        ],
    );
    created.assert_success();
    run(&environment, &["config", "set", "provider_id", "managed"]).assert_success();

    let listed = run(&environment, &["config", "provider", "list"]);
    listed.assert_success();
    assert!(listed.stdout.contains("managed"));

    let shown = run(&environment, &["config", "provider", "show", "managed"]);
    shown.assert_success();
    assert!(shown
        .stdout
        .contains("base_url = \"https://gateway.example/v1\""));
    assert!(shown
        .stdout
        .contains("credential = \"local-fixture-credential\""));

    let rejected = run(&environment, &["config", "provider", "remove", "managed"]);
    rejected.assert_failure();
    assert!(rejected.stdout.contains("\"clears_default_route\": true"));
    remove_provider(&environment, "managed");
    assert!(rivo_session::read_file_config(environment.config_path())
        .unwrap()
        .defaults
        .provider_id
        .is_none());
}

#[test]
fn e2e_cli_003_model_discovery_uses_local_provider_and_persists_catalog() {
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![HttpResponse::json(
        r#"{"data":[{"id":"fixture-a"},{"id":"unknown-catalog-model"},{"id":"fixture-b"}]}"#,
    )])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());

    let credential = environment
        .command(binary())
        .args([
            "config",
            "credentials",
            "set",
            "fixture-key",
            "--from-env",
            "RIVO_FIXTURE_KEY",
        ])
        .env("RIVO_FIXTURE_KEY", "model-secret-value")
        .run()
        .unwrap();
    credential.assert_success();

    for model in ["fixture-a", "fixture-b"] {
        run(
            &environment,
            &[
                "config",
                "model",
                "set",
                model,
                "--vendor",
                "openai",
                "--display-name",
                model,
                "--protocols",
                "openai_chat_completions",
            ],
        )
        .assert_success();
    }

    let configured = environment
        .command(binary())
        .args([
            "config",
            "provider",
            "set",
            "fixture",
            "--base-url",
            &base_url,
            "--credential-name",
            "fixture-key",
        ])
        .env("RIVO_FIXTURE_KEY", "model-secret-value")
        .run()
        .unwrap();
    configured.assert_success();

    let refresh = environment
        .command(binary())
        .args(["config", "provider", "models", "refresh", "fixture"])
        .env("RIVO_FIXTURE_KEY", "model-secret-value")
        .run()
        .unwrap();
    refresh.assert_success();
    assert!(refresh.stdout.contains("supported=true"));
    assert!(refresh.stdout.contains("fixture-a"));
    assert!(refresh.stdout.contains("fixture-b"));
    assert!(!refresh.stdout.contains("unknown-catalog-model"));
    assert_redacted(
        &format!("{}{}", refresh.stdout, refresh.stderr),
        &["model-secret-value"],
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].request_line.starts_with("GET /v1/models "));
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer model-secret-value")
    );

    let content = std::fs::read_to_string(environment.config_path()).unwrap();
    assert!(content.contains("[providers.fixture.discovery]"));
    assert!(content.contains("supported = true"));
    assert_eq!(
        rivo_session::read_file_config(environment.config_path())
            .unwrap()
            .providers["fixture"]
            .discovery
            .models,
        vec!["fixture-a", "fixture-b"]
    );
    assert!(!content.contains("unknown-catalog-model"));
}

#[test]
fn e2e_cli_provider_models_remote_lists_all_without_persisting() {
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![HttpResponse::json(
        r#"{"data":[{"id":"remote-unknown"},{"id":"claude-opus-5"},{"id":"remote-unknown"}]}"#,
    )])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());
    set_test_credential(&environment, "remote-key");
    let configured = run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "remote-fixture",
            "--base-url",
            &base_url,
            "--credential-name",
            "remote-key",
        ],
    );
    configured.assert_success();
    let before = std::fs::read(environment.config_path()).unwrap();

    let remote = run(
        &environment,
        &["config", "provider", "models", "remote", "remote-fixture"],
    );

    remote.assert_success();
    assert_eq!(remote.stdout, "remote-unknown\nclaude-opus-5\n");
    assert!(!remote.stdout.contains("supported=true"));
    assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
    assert_redacted(
        &format!("{}{}", remote.stdout, remote.stderr),
        &["test-only-credential-value"],
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].request_line.starts_with("GET /v1/models "));
    // Provider 不携带协议：discovery 先以 Bearer 认证，成功时不发送 Anthropic 头。
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-only-credential-value")
    );
    assert!(!requests[0].headers.contains_key("x-api-key"));
}

#[test]
fn e2e_cli_provider_set_refresh_is_explicit_and_persists_discovery() {
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![HttpResponse::json(
        r#"{"data":[{"id":"fixture-refresh-model"},{"id":"unknown-catalog-model"}]}"#,
    )])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());
    set_test_credential(&environment, "refresh-key");
    run(
        &environment,
        &[
            "config",
            "model",
            "set",
            "fixture-refresh-model",
            "--vendor",
            "openai",
            "--display-name",
            "Fixture refresh model",
            "--protocols",
            "openai_chat_completions",
        ],
    )
    .assert_success();

    let configured = environment
        .command(binary())
        .args([
            "config",
            "provider",
            "set",
            "refresh-fixture",
            "--base-url",
            &base_url,
            "--credential-name",
            "refresh-key",
            "--refresh",
        ])
        .run()
        .unwrap();
    configured.assert_success();
    assert!(configured
        .stdout
        .contains("Model discovery refreshed: refresh-fixture"));
    assert!(configured.stdout.contains("fixture-refresh-model"));
    assert!(!configured.stdout.contains("unknown-catalog-model"));
    assert_eq!(
        rivo_session::read_file_config(environment.config_path())
            .unwrap()
            .providers["refresh-fixture"]
            .discovery
            .models,
        vec!["fixture-refresh-model"]
    );
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn e2e_cli_refresh_with_no_compatible_models_succeeds_and_explains_empty_result() {
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![HttpResponse::json(
        r#"{"data":[{"id":"unknown-catalog-model"},{"id":"claude-opus-5"}]}"#,
    )])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());
    set_test_credential(&environment, "empty-discovery-key");
    run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "empty-discovery",
            "--base-url",
            &base_url,
            "--credential-name",
            "empty-discovery-key",
        ],
    )
    .assert_success();

    let refresh = run(
        &environment,
        &["config", "provider", "models", "refresh", "empty-discovery"],
    );
    refresh.assert_success();
    assert!(refresh.stdout.contains("supported=true"));
    assert!(refresh.stdout.contains(
        "Model discovery succeeded but no discovered model is in the local model catalog"
    ));
    let discovery = &rivo_session::read_file_config(environment.config_path())
        .unwrap()
        .providers["empty-discovery"]
        .discovery;
    assert_eq!(discovery.supported, Some(true));
    assert!(discovery.last_error.is_none());
    assert!(discovery.models.is_empty());
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn e2e_cli_014_shared_model_routing_maintenance_commands_round_trip() {
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![HttpResponse::json(
        r#"{"data":[{"id":"custom-model-029"},{"id":"other-model"},{"id":"custom-model-029"}]}"#,
    )])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());

    let missing_protocols = run(
        &environment,
        &[
            "config",
            "model",
            "set",
            "missing-protocols",
            "--vendor",
            "openai",
            "--display-name",
            "Missing Protocols",
        ],
    );
    missing_protocols.assert_failure();

    for args in [
        vec![
            "config",
            "vendor",
            "set",
            "fixture-vendor",
            "--display-name",
            "Fixture Vendor",
        ],
        vec![
            "config",
            "model",
            "set",
            "custom-model-029",
            "--vendor",
            "fixture-vendor",
            "--display-name",
            "Custom Model",
            "--protocols",
            "openai_chat_completions,openai_responses",
            "--priority",
            "7",
        ],
        vec![
            "config",
            "provider",
            "set",
            "fixture-provider",
            "--base-url",
            base_url.as_str(),
            "--priority",
            "9",
        ],
    ] {
        let output = run(&environment, &args);
        assert_eq!(output.status, 0, "{args:?}: {output:?}");
    }

    let listed = run(&environment, &["config", "provider", "list"]);
    listed.assert_success();
    assert!(listed.stdout.contains("fixture-provider"));
    let shown = run(
        &environment,
        &["config", "model", "show", "custom-model-029"],
    );
    shown.assert_success();
    assert!(shown.stdout.contains("fixture-vendor"));
    let providers_before_refresh = run(
        &environment,
        &["config", "provider", "models", "list", "fixture-provider"],
    );
    providers_before_refresh.assert_success();
    assert!(providers_before_refresh
        .stdout
        .contains("supported=unknown"));

    let rejected = run(
        &environment,
        &[
            "config",
            "route",
            "--vendor",
            "fixture-vendor",
            "--model",
            "custom-model-029",
        ],
    );
    rejected.assert_failure();
    assert!(rejected.stderr.contains("没有已验证且精确发现模型"));

    let refreshed = environment
        .command(binary())
        .args([
            "config",
            "provider",
            "models",
            "refresh",
            "fixture-provider",
        ])
        .env("RIVO_FIXTURE_KEY", "fixture-secret-value")
        .run()
        .unwrap();
    refreshed.assert_success();
    assert_redacted(
        &format!("{}{}", refreshed.stdout, refreshed.stderr),
        &["fixture-secret-value"],
    );
    assert!(refreshed.stdout.contains("custom-model-029"));
    assert_eq!(server.finish().len(), 1);

    let providers = run(
        &environment,
        &["config", "model", "providers", "custom-model-029"],
    );
    providers.assert_success();
    assert!(providers
        .stdout
        .contains("fixture-provider\tavailable\tpriority=9"));

    let routed = run(
        &environment,
        &[
            "config",
            "route",
            "--vendor",
            "fixture-vendor",
            "--model",
            "custom-model-029",
            "--provider-id",
            "fixture-provider",
        ],
    );
    assert_eq!(
        routed.status, 0,
        "explicit route failed\nstdout:\n{}\nstderr:\n{}",
        routed.stdout, routed.stderr
    );
    assert!(routed.stdout.contains("provider=fixture-provider"));
    assert!(routed.stdout.contains("model=custom-model-029"));
    assert!(routed.stdout.contains("priority=9"));
    assert!(!routed.stdout.contains("request_model_id"));
    let reverse = run(
        &environment,
        &["config", "provider", "models", "list", "fixture-provider"],
    );
    reverse.assert_success();
    assert!(reverse.stdout.contains("custom-model-029\tfixture-vendor"));
    assert!(!reverse.stdout.contains("other-model"));

    let removed = run(
        &environment,
        &["config", "vendor", "remove", "fixture-vendor", "--yes"],
    );
    removed.assert_failure();
    assert!(removed.stderr.contains("仍有 Model 引用"));
    for (kind, id) in [("model", "custom-model-029"), ("vendor", "fixture-vendor")] {
        run(&environment, &["config", kind, "remove", id, "--yes"]).assert_success();
    }
    assert!(run(&environment, &["config", "vendor", "list"])
        .stdout
        .lines()
        .all(|line| !line.starts_with("fixture-vendor\t")));
}

#[test]
fn e2e_cli_004_headless_stream_runs_through_real_binary_and_records_history() {
    let environment = schema3_environment();
    let server =
        ScriptedHttpServer::start(vec![HttpResponse::sse(chat_stream("fixture-ok"))]).unwrap();
    let base_url = format!("{}/v1", server.base_url());
    configure_fixture(&environment, &base_url);
    let workspace = environment.workspace().display().to_string();

    let output = environment
        .command(binary())
        .args([
            "--provider-id",
            "fixture",
            "--workspace",
            &workspace,
            "--trace",
            "--prompt",
            "return fixture result",
        ])
        .env("RIVO_FIXTURE_KEY", "headless-secret-value")
        .run()
        .unwrap();
    output.assert_success();
    assert!(output.stdout.contains("fixture-ok"));
    assert!(output.stderr.contains("budget tokens="));
    assert_redacted(
        &format!("{}{}", output.stdout, output.stderr),
        &["headless-secret-value"],
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .request_line
        .starts_with("POST /v1/chat/completions "));
    assert!(requests[0].body.contains("return fixture result"));

    let sessions = run(&environment, &["sessions", "list"]);
    sessions.assert_success();
    assert!(sessions.stdout.contains("events="));
    assert!(sessions.stdout.contains("summary=return fixture result"));

    let diagnostic = environment.root().join("diagnostic.json");
    let diagnostic_path = diagnostic.display().to_string();
    let created = run(
        &environment,
        &["diagnostics", "create", &diagnostic_path, "--yes"],
    );
    created.assert_success();
    assert!(diagnostic.is_file());
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(diagnostic).unwrap()).unwrap();
    assert_eq!(
        bundle.pointer("/manifest/database_included"),
        None,
        "database inclusion is represented at the bundle root"
    );
    assert_eq!(
        bundle.pointer("/database_included"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn e2e_cli_run_stdin_jsonl_has_ordered_public_events() {
    let environment = schema3_environment();
    let server =
        ScriptedHttpServer::start(vec![HttpResponse::sse(chat_stream("jsonl-answer"))]).unwrap();
    let base_url = format!("{}/v1", server.base_url());
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, &base_url);
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let mut child = Command::new(binary())
        .args(["run", "--stdin", "--format", "jsonl"])
        .env("RIVO_CONFIG", environment.config_path())
        .env("RIVO_DATA_DIR", environment.data_dir())
        .env("HOME", environment.root())
        .env("USERPROFILE", environment.root())
        .env("RIVO_FIXTURE_KEY", "jsonl-secret-value")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"stream this task from stdin")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let events = stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events.len() >= 2);
    assert!(events.iter().all(|event| event["schema_version"] == 1));
    assert!(events
        .windows(2)
        .all(|pair| pair[0]["sequence"].as_u64() < pair[1]["sequence"].as_u64()));
    assert_eq!(events.first().unwrap()["event"]["type"], "started");
    assert_eq!(events.last().unwrap()["event"]["type"], "final_answer");
    assert!(!stdout.contains("jsonl-secret-value"));

    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("stream this task from stdin"));
}

#[cfg(windows)]
#[test]
fn e2e_app_server_pipe_keeps_delivering_events_past_the_stdio_queue_capacity() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let mut body = String::new();
    for index in 0..600 {
        body.push_str(&format!(
            "data: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture-model\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"d{index} \"}},\"finish_reason\":null}}]}}\n\n"
        ));
    }
    body.push_str("data: {\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\ndata: [DONE]\n\n");
    let server = ScriptedHttpServer::start(vec![HttpResponse::sse(body)]).unwrap();
    let base_url = format!("{}/v1", server.base_url());
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, &base_url);
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let mut child = Command::new(binary())
        .args(["app-server", "--pipe"])
        .env("RIVO_CONFIG", environment.config_path())
        .env("RIVO_DATA_DIR", environment.data_dir())
        .env("HOME", environment.root())
        .env("USERPROFILE", environment.root())
        .env("RIVO_FIXTURE_KEY", "pipe-secret")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let endpoint = rivo_instance_control::local_endpoint(environment.data_dir());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Kill the server before reporting a failure; a stalled bridge would
    // otherwise keep the scripted provider connection open forever.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
        let deadline = Instant::now() + app_server_test_timeout();
        let client = loop {
            match tokio::net::windows::named_pipe::ClientOptions::new().open(&endpoint) {
                Ok(client) => break client,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(error) => panic!("pipe did not open: {error}"),
            }
        };
        let (reader, mut writer) = tokio::io::split(client);
        let mut lines = tokio::io::BufReader::new(reader).lines();
        async fn next<R: tokio::io::AsyncBufRead + Unpin>(
            lines: &mut tokio::io::Lines<R>,
        ) -> serde_json::Value {
            let line = tokio::time::timeout(app_server_test_timeout(), lines.next_line())
                .await
                .expect("pipe stopped delivering messages")
                .unwrap()
                .expect("pipe closed");
            serde_json::from_str(&line).unwrap()
        }
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocol_version\":\"rivo-app-server-v2\",\"client\":{\"name\":\"pipe-fixture\",\"version\":\"1\"}}}\n").await.unwrap();
        assert_eq!(next(&mut lines).await["id"], 1);
        let start = format!("{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/start\",\"params\":{{\"working_directory\":{},\"runtime_settings\":{{\"collaboration_mode\":\"code\",\"permission_preset\":\"workspace_write\"}},\"idempotency_key\":\"pipe-start\"}}}}\n", serde_json::to_string(&workspace).unwrap());
        writer.write_all(start.as_bytes()).await.unwrap();
        let session = next(&mut lines).await["result"]["session_ref"].as_str().unwrap().to_owned();
        let turn = format!("{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"turn/start\",\"params\":{{\"session_ref\":{},\"prompt\":\"stream many deltas\",\"runtime_settings\":{{}},\"idempotency_key\":\"pipe-turn\"}}}}\n", serde_json::to_string(&session).unwrap());
        writer.write_all(turn.as_bytes()).await.unwrap();
        let mut deltas = 0;
        loop {
            let message = next(&mut lines).await;
            match message["params"]["type"].as_str() {
                Some("message_delta") => deltas += 1,
                Some("turn_complete") => break (deltas, message["params"]["payload"]["outcome"].clone()),
                _ => {}
            }
        }
    })
    }));
    let _ = child.kill();
    let _ = child.wait();
    let outcome = outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert!(outcome.0 > 512, "deltas={}", outcome.0);
    assert_eq!(outcome.1, "completed");
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn e2e_app_server_subagent_projects_lifecycle_child_tools_and_approval() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(tool_call_stream(
            "call-delegate",
            "delegate_task",
            serde_json::json!({"task":"创建 child-note.txt，内容为 child","label":"child note"}),
        )),
        HttpResponse::sse(write_file_tool_stream("child-note.txt", "child")),
        HttpResponse::sse(chat_stream("child-done")),
        HttpResponse::sse(chat_stream("root-done")),
        HttpResponse::sse(chat_stream("no-subagents")),
    ])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, &base_url);
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let target = environment.workspace().join("child-note.txt");
    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let (messages_tx, messages_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in stdout.lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if messages_tx.send(message).is_err() {
                break;
            }
        }
    });
    let next_message = || {
        messages_rx
            .recv_timeout(app_server_test_timeout())
            .expect("app-server did not produce the expected response/event")
    };

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"subagent-fixture","version":"1"}}}}}}"#).unwrap();
    let initialized = next_message();
    assert_eq!(
        initialized["result"]["capabilities"]["supports_subagents"],
        true
    );
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"workspace_write"}},"idempotency_key":"subagent-start"}}}}"#, serde_json::to_string(&workspace).unwrap()).unwrap();
    let started = next_message();
    assert_eq!(
        started["result"]["accepted_runtime_settings"]["subagents_enabled"],
        true
    );
    let session = started["result"]["session_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"session_ref":{},"prompt":"委派一个子任务写入笔记，然后简要回答。","runtime_settings":{{}},"idempotency_key":"subagent-turn"}}}}"#, serde_json::to_string(&session).unwrap()).unwrap();

    let mut events = Vec::<serde_json::Value>::new();
    loop {
        let message = next_message();
        if message["method"] != "event" {
            continue;
        }
        let params = message["params"].clone();
        if params["type"] == "interaction_request" {
            let interaction_id = params["payload"]["interaction_id"].as_str().unwrap();
            writeln!(stdin, r#"{{"jsonrpc":"2.0","id":8,"method":"interaction/respond","params":{{"session_ref":{},"interaction_id":{},"decision":"allow","idempotency_key":"subagent-approval"}}}}"#, serde_json::to_string(&session).unwrap(), serde_json::to_string(interaction_id).unwrap()).unwrap();
        }
        let done = params["type"] == "turn_complete";
        events.push(params);
        if done {
            break;
        }
    }
    let kinds = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "child",
        "events={kinds:?}"
    );
    let subagent_statuses = events
        .iter()
        .filter(|event| event["type"] == "subagent")
        .map(|event| {
            assert_eq!(event["payload"]["tool_call_id"], "call-delegate");
            event["payload"]["status"].as_str().unwrap_or_default()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        subagent_statuses,
        ["queued", "running", "completed"],
        "events={kinds:?}"
    );
    let request = events
        .iter()
        .find(|event| event["type"] == "interaction_request")
        .expect("subagent approval must reach the host");
    let child_call = request["payload"]["tool_call_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(child_call.ends_with(":call-write"), "{child_call}");
    assert!(request["payload"]["agent_id"].as_str().is_some());
    assert!(request["payload"]["task_id"].as_str().is_some());
    let child_statuses = events
        .iter()
        .filter(|event| {
            event["type"] == "tool_call" && event["payload"]["tool_call_id"] == child_call.as_str()
        })
        .map(|event| event["payload"]["status"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        child_statuses.last(),
        Some(&"completed"),
        "events={kinds:?} child={child_statuses:?}"
    );
    assert!(child_statuses.contains(&"waiting_interaction"));
    assert!(
        !events.iter().any(|event| event["type"] == "status"
            && event["payload"]["status"] == "waiting_interaction"),
        "a waiting subagent must not pause the root turn: {kinds:?}"
    );
    let delegate_statuses = events
        .iter()
        .filter(|event| {
            event["type"] == "tool_call" && event["payload"]["tool_call_id"] == "call-delegate"
        })
        .map(|event| event["payload"]["status"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(delegate_statuses.last(), Some(&"completed"));
    assert!(!serde_json::to_string(&events)
        .unwrap()
        .contains("child-done"));

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{{"session_ref":{},"prompt":"直接回答。","runtime_settings":{{"subagents_enabled":false}},"idempotency_key":"no-subagent-turn"}}}}"#, serde_json::to_string(&session).unwrap()).unwrap();
    loop {
        let message = next_message();
        if message["method"] == "event" && message["params"]["type"] == "turn_complete" {
            break;
        }
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
    let requests = server.finish();
    assert_eq!(requests.len(), 5);
    let offers_delegate = |index: usize| {
        let body: serde_json::Value = serde_json::from_str(&requests[index].body).unwrap();
        body["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["function"]["name"] == "delegate_task")
        })
    };
    assert!(offers_delegate(0));
    assert!(!offers_delegate(1), "subagents cannot delegate");
    assert!(
        !offers_delegate(4),
        "subagents_enabled=false must hide delegate_task"
    );
}

#[test]
fn e2e_app_server_read_only_write_is_denied_and_returned_to_the_model() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(write_file_tool_stream("ro.txt", "RO")),
        HttpResponse::sse(chat_stream("read-only mode cannot write")),
    ])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, &base_url);
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"read-only-fixture","version":"1"}}}}}}"#).unwrap();
    assert_eq!(read_rpc(&mut stdout)["id"], 1);
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"read_only"}},"idempotency_key":"ro-start"}}}}"#, serde_json::to_string(&workspace).unwrap()).unwrap();
    let session = read_rpc(&mut stdout)["result"]["session_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"session_ref":{},"prompt":"create ro.txt containing RO","runtime_settings":{{}},"idempotency_key":"ro-turn"}}}}"#, serde_json::to_string(&session).unwrap()).unwrap();
    let mut events = Vec::new();
    loop {
        let message = read_rpc(&mut stdout);
        if message["method"] != "event" {
            continue;
        }
        let done = message["params"]["type"] == "turn_complete";
        events.push(message["params"].clone());
        if done {
            break;
        }
    }
    let kinds = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    let write_terminal = events
        .iter()
        .rev()
        .find(|event| {
            event["type"] == "tool_call" && event["payload"]["tool_call_id"] == "call-write"
        })
        .expect("write_file tool call must be projected");
    assert_eq!(
        write_terminal["payload"]["status"], "failed",
        "events={kinds:?}"
    );
    assert_eq!(write_terminal["payload"]["error_code"], "policy_denied");
    assert!(
        !events.iter().any(|event| event["type"] == "error"),
        "a policy denial is not a runtime failure: {kinds:?}"
    );
    assert_eq!(events.last().unwrap()["payload"]["outcome"], "completed");
    assert!(!environment.workspace().join("ro.txt").exists());
    drop(stdin);
    assert!(child.wait().unwrap().success());
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn e2e_app_server_session_messaging_holds_approves_and_delivers_between_sessions() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let (responses, script) = std::sync::mpsc::channel();
    let server = ScriptedHttpServer::start_channel(script).unwrap();
    let base_url = format!("{}/v1", server.base_url());
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, &base_url);
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let (messages_tx, messages_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in stdout.lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if messages_tx.send(message).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::<serde_json::Value>::new();
    let mut wait_for = |predicate: &dyn Fn(&serde_json::Value) -> bool| loop {
        let message = messages_rx
            .recv_timeout(app_server_test_timeout())
            .unwrap_or_else(|_| panic!("app-server stalled; seen={seen:?}"));
        seen.push(message.clone());
        if predicate(&message) {
            return message;
        }
    };
    let response_to = |id: u64| move |message: &serde_json::Value| message["id"] == id;
    let event_of = |session: String, kind: &'static str| {
        move |message: &serde_json::Value| {
            message["method"] == "event"
                && message["params"]["session_ref"] == session.as_str()
                && message["params"]["type"] == kind
        }
    };
    let quoted = |value: &str| serde_json::to_string(value).unwrap();

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"messaging-fixture","version":"1"}}}}}}"#).unwrap();
    wait_for(&response_to(1));
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"workspace_write","session_messaging":true}},"idempotency_key":"messaging-start-a"}}}}"#, quoted(&workspace)).unwrap();
    let started = wait_for(&response_to(2));
    assert_eq!(
        started["result"]["accepted_runtime_settings"]["session_messaging"],
        true
    );
    let sender = started["result"]["session_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"workspace_write"}},"idempotency_key":"messaging-start-b"}}}}"#, quoted(&workspace)).unwrap();
    let receiver = wait_for(&response_to(3))["result"]["session_ref"]
        .as_str()
        .unwrap()
        .to_owned();

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":4,"method":"session/peers","params":{{}}}}"#
    )
    .unwrap();
    let peers = wait_for(&response_to(4));
    let peers = peers["result"]["peers"].as_array().unwrap();
    assert_eq!(peers.len(), 2);
    assert!(peers
        .iter()
        .all(|peer| peer["inbound"] == "held" && peer["status"] == "idle"));

    // Sender turn: one confirmed session_send, then a final answer.
    responses
        .send(HttpResponse::sse(tool_call_stream(
            "call-send",
            "session_send",
            serde_json::json!({"to": receiver, "message": "hello from the sender session"}),
        )))
        .unwrap();
    responses
        .send(HttpResponse::sse(chat_stream("sent")))
        .unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"session_ref":{},"prompt":"通知另一个会话。","runtime_settings":{{}},"idempotency_key":"messaging-turn-a"}}}}"#, quoted(&sender)).unwrap();
    let request = wait_for(&event_of(sender.clone(), "interaction_request"));
    let interaction_id = request["params"]["payload"]["interaction_id"]
        .as_str()
        .unwrap()
        .to_owned();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":6,"method":"interaction/respond","params":{{"session_ref":{},"interaction_id":{},"decision":"allow","idempotency_key":"messaging-allow-send"}}}}"#, quoted(&sender), quoted(&interaction_id)).unwrap();
    let held = wait_for(&event_of(receiver.clone(), "session_message_held"));
    let message_id = held["params"]["payload"]["message_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        held["params"]["payload"]["from_session_id"],
        sender.as_str()
    );
    assert!(held["params"]["payload"].get("body").is_none());
    let tool_done = wait_for(&|message: &serde_json::Value| {
        message["method"] == "event"
            && message["params"]["type"] == "tool_call"
            && message["params"]["payload"]["tool_call_id"] == "call-send"
            && message["params"]["payload"]["status"] == "completed"
    });
    assert!(!tool_done
        .to_string()
        .contains("hello from the sender session"));
    wait_for(&event_of(sender.clone(), "turn_complete"));

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":7,"method":"session/message/list","params":{{"session_ref":{},"state":"held"}}}}"#, quoted(&receiver)).unwrap();
    let listed = wait_for(&response_to(7));
    assert_eq!(
        listed["result"]["messages"][0]["message_id"],
        message_id.as_str()
    );
    assert_eq!(
        listed["result"]["messages"][0]["body"]["text"],
        "hello from the sender session"
    );

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":8,"method":"session/message/respond","params":{{"session_ref":{},"message_id":{},"approve":true,"idempotency_key":"messaging-approve-1"}}}}"#, quoted(&receiver), quoted(&message_id)).unwrap();
    assert_eq!(
        wait_for(&response_to(8))["error"]["code"],
        "confirmation_required"
    );
    responses
        .send(HttpResponse::sse(chat_stream("received")))
        .unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":9,"method":"session/message/respond","params":{{"session_ref":{},"message_id":{},"approve":true,"confirmation":true,"idempotency_key":"messaging-approve-2"}}}}"#, quoted(&receiver), quoted(&message_id)).unwrap();
    assert_eq!(wait_for(&response_to(9))["result"]["state"], "delivered");
    wait_for(&event_of(receiver.clone(), "turn_complete"));

    drop(stdin);
    drop(responses);
    assert!(child.wait().unwrap().success());
    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    let tool_names = |index: usize| {
        let body: serde_json::Value = serde_json::from_str(&requests[index].body).unwrap();
        body["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert!(tool_names(0).contains(&"session_send".to_owned()));
    assert!(
        !tool_names(2).contains(&"session_send".to_owned()),
        "messaging tools are opt-in per session"
    );
    let delivered: serde_json::Value = serde_json::from_str(&requests[2].body).unwrap();
    let delivered = delivered.to_string();
    assert!(delivered.contains("hello from the sender session"));
    assert!(
        delivered.contains(&format!(r#"<session-message from=\"{sender}\""#)),
        "{delivered}"
    );
}

#[test]
fn e2e_app_server_approval_continues_same_tool_call_and_writes_file() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(write_file_tool_stream("approved-bridge.txt", "yes")),
        HttpResponse::sse(chat_stream("write-approved")),
    ])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());
    let workspace = environment.workspace().display().to_string();
    configure_fixture(&environment, &base_url);
    run(&environment, &["config", "set", "workspace", &workspace]).assert_success();

    let target = environment.workspace().join("approved-bridge.txt");
    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let (messages_tx, messages_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in stdout.lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if messages_tx.send(message).is_err() {
                break;
            }
        }
    });
    let next_message = || {
        messages_rx
            .recv_timeout(app_server_test_timeout())
            .expect("app-server did not produce the expected response/event")
    };

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"rivo-app-server-v2","client":{{"name":"approval-fixture","version":"1"}}}}}}"#).unwrap();
    assert_eq!(next_message()["id"], 1);
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"working_directory":{},"runtime_settings":{{"collaboration_mode":"code","permission_preset":"workspace_write"}},"idempotency_key":"approval-start"}}}}"#, serde_json::to_string(&workspace).unwrap()).unwrap();
    let session = next_message()["result"]["session_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"session_ref":{},"prompt":"Use write_file to create approved-bridge.txt with exactly yes, then answer briefly.","runtime_settings":{{}},"idempotency_key":"approval-turn"}}}}"#, serde_json::to_string(&session).unwrap()).unwrap();

    let mut turn_started = false;
    let mut interaction_id = None;
    let mut turn_complete = false;
    let mut event_types = Vec::new();
    let mut events = Vec::<serde_json::Value>::new();
    let mut response_accepted = false;
    while !turn_complete {
        let message = next_message();
        if message["id"] == 8 {
            response_accepted = message["result"]["accepted"] == true;
        }
        if message["id"] == 3 {
            turn_started = true;
        }
        if message["method"] == "event" {
            event_types.push(
                message["params"]["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            events.push(message["params"].clone());
            if message["params"]["type"] == "interaction_request" {
                interaction_id = message["params"]["payload"]["interaction_id"]
                    .as_str()
                    .map(str::to_owned);
                let interaction_id = interaction_id.as_ref().unwrap();
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":8,"method":"interaction/respond","params":{{"session_ref":{},"interaction_id":{},"decision":"allow","idempotency_key":"approval-response-{}"}}}}"#, serde_json::to_string(&session).unwrap(), serde_json::to_string(interaction_id).unwrap(), interaction_id).unwrap();
            }
            if message["params"]["type"] == "turn_complete" {
                turn_complete = true;
            }
        }
    }
    assert!(turn_started);
    assert!(interaction_id.is_some());
    assert!(response_accepted, "events={event_types:?}");
    assert!(target.exists(), "events={event_types:?}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "yes",
        "events={event_types:?}"
    );
    let requests = events
        .iter()
        .filter(|event| event["type"] == "interaction_request")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1, "events={event_types:?}");
    let call_id = requests[0]["payload"]["tool_call_id"]
        .as_str()
        .expect("approval request must name its tool call");
    let after_request = events
        .iter()
        .skip_while(|event| event["type"] != "interaction_request")
        .skip(1)
        .collect::<Vec<_>>();
    let tool_statuses = after_request
        .iter()
        .filter(|event| event["type"] == "tool_call" && event["payload"]["tool_call_id"] == call_id)
        .map(|event| event["payload"]["status"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        tool_statuses,
        ["running", "completed"],
        "events={event_types:?}"
    );
    // The approval batch is tool_call, interaction_request and then the
    // waiting status; the runtime must leave that state exactly once.
    let statuses = after_request
        .iter()
        .filter(|event| event["type"] == "status")
        .map(|event| event["payload"]["status"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        statuses.get(..2),
        Some(&["waiting_interaction", "running"][..]),
        "events={event_types:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == "waiting_interaction")
            .count(),
        1,
        "events={event_types:?}"
    );

    drop(stdin);
    assert!(child.wait().unwrap().success());
    let mut diagnostics = String::new();
    stderr.read_to_string(&mut diagnostics).unwrap();
    assert!(diagnostics.is_empty(), "{diagnostics}");
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn e2e_cli_non_tty_denies_mutation_and_manifest_allows_only_exact_call() {
    let environment = schema3_environment();
    let workspace = environment.workspace().display().to_string();
    let denied_server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(write_file_tool_stream("denied.txt", "no")),
        // A denial is handed back to the model, which then explains it.
        HttpResponse::sse(chat_stream("write was denied")),
    ])
    .unwrap();
    let denied_base_url = format!("{}/v1", denied_server.base_url());
    configure_fixture(&environment, &denied_base_url);
    std::fs::write(environment.workspace().join("denied.txt"), "before").unwrap();
    let denied = environment
        .command(binary())
        .args([
            "run",
            "--auto-provider",
            "--workspace",
            &workspace,
            "--profile",
            "coding",
            "--policy",
            "medium",
            "--format",
            "jsonl",
            "--prompt",
            "request a write",
        ])
        .env("RIVO_FIXTURE_KEY", "approval-secret")
        .run()
        .unwrap();
    assert_eq!(denied.status, 3, "{}", denied.stderr);
    assert!(denied.stderr.contains("approval denied"));
    assert_eq!(
        std::fs::read_to_string(environment.workspace().join("denied.txt")).unwrap(),
        "before"
    );
    let denied_events = denied
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(denied_events
        .iter()
        .any(|event| event["event"]["type"] == "confirmation_required"));
    assert!(matches!(
        denied_events
            .last()
            .and_then(|event| event["event"]["type"].as_str()),
        Some("stopped" | "final_answer")
    ));
    assert_eq!(
        denied_events
            .iter()
            .filter(|event| {
                matches!(
                    event["event"]["type"].as_str(),
                    Some("stopped" | "final_answer")
                )
            })
            .count(),
        1,
        "JSONL run must emit exactly one terminal event"
    );
    assert_eq!(denied_server.finish().len(), 2);

    let allowed_server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(write_file_tool_stream("approved.txt", "yes")),
        // The Session replays the pending model turn with the exact approval
        // token; a provider therefore emits the pending tool call once more.
        HttpResponse::sse(write_file_tool_stream("approved.txt", "yes")),
        HttpResponse::sse(chat_stream("write-approved")),
    ])
    .unwrap();
    let allowed_base_url = format!("{}/v1", allowed_server.base_url());
    configure_fixture(&environment, &allowed_base_url);
    std::fs::write(environment.workspace().join("approved.txt"), "before").unwrap();
    let manifest = environment.root().join("approvals.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "workspace": workspace,
            "rules": [{
                "tool_name": "write_file",
                "arguments_sha256": canonical_write_file_hash("approved.txt", "yes"),
                "scope": "once",
                "max_calls": 1,
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let allowed = environment
        .command(binary())
        .args([
            "run",
            "--auto-provider",
            "--workspace",
            &workspace,
            "--profile",
            "coding",
            "--policy",
            "medium",
            "--format",
            "jsonl",
            "--approval-mode",
            "manifest",
            "--approval-manifest",
            manifest.to_str().unwrap(),
            "--prompt",
            "request an approved write",
        ])
        .env("RIVO_FIXTURE_KEY", "approval-secret")
        .run()
        .unwrap();
    allowed.assert_success();
    assert_eq!(
        std::fs::read_to_string(environment.workspace().join("approved.txt")).unwrap(),
        "yes",
        "stdout:\n{}\nstderr:\n{}",
        allowed.stdout,
        allowed.stderr
    );
    assert!(allowed.stdout.contains("final_answer"));
    assert_redacted(
        &format!("{}{}", allowed.stdout, allowed.stderr),
        &["approval-secret"],
    );
    assert_eq!(allowed_server.finish().len(), 3);

    let mismatch_server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(write_file_tool_stream("mismatch.txt", "blocked")),
        HttpResponse::sse(chat_stream("write was denied")),
    ])
    .unwrap();
    let mismatch_base_url = format!("{}/v1", mismatch_server.base_url());
    configure_fixture(&environment, &mismatch_base_url);
    std::fs::write(environment.workspace().join("mismatch.txt"), "before").unwrap();
    let mismatch_manifest = environment.root().join("mismatched-approvals.json");
    std::fs::write(
        &mismatch_manifest,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "workspace": workspace,
            "rules": [{
                "tool_name": "write_file",
                "arguments_sha256": canonical_write_file_hash("mismatch.txt", "different"),
                "scope": "once",
                "max_calls": 1,
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let mismatch = environment
        .command(binary())
        .args([
            "run",
            "--auto-provider",
            "--workspace",
            &workspace,
            "--profile",
            "coding",
            "--policy",
            "medium",
            "--approval-mode",
            "manifest",
            "--approval-manifest",
            mismatch_manifest.to_str().unwrap(),
            "--prompt",
            "request a mismatched write",
        ])
        .env("RIVO_FIXTURE_KEY", "approval-secret")
        .run()
        .unwrap();
    assert_eq!(mismatch.status, 3, "{}", mismatch.stderr);
    assert!(mismatch.stderr.contains("approval denied"));
    assert_eq!(
        std::fs::read_to_string(environment.workspace().join("mismatch.txt")).unwrap(),
        "before"
    );
    assert_eq!(mismatch_server.finish().len(), 2);

    let prompt_server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(write_file_tool_stream("prompt.txt", "blocked")),
        HttpResponse::sse(chat_stream("write was denied")),
    ])
    .unwrap();
    let prompt_base_url = format!("{}/v1", prompt_server.base_url());
    configure_fixture(&environment, &prompt_base_url);
    std::fs::write(environment.workspace().join("prompt.txt"), "before").unwrap();
    let explicit_prompt = environment
        .command(binary())
        .args([
            "run",
            "--auto-provider",
            "--workspace",
            &workspace,
            "--profile",
            "coding",
            "--policy",
            "medium",
            "--approval-mode",
            "prompt",
            "--prompt",
            "request a prompt write",
        ])
        .env("RIVO_FIXTURE_KEY", "approval-secret")
        .run()
        .unwrap();
    assert_eq!(explicit_prompt.status, 3, "{}", explicit_prompt.stderr);
    assert!(explicit_prompt.stderr.contains("unavailable without a TTY"));
    assert!(explicit_prompt.duration < Duration::from_secs(5));
    assert_eq!(
        std::fs::read_to_string(environment.workspace().join("prompt.txt")).unwrap(),
        "before"
    );
    assert_eq!(prompt_server.finish().len(), 2);
}

#[test]
fn e2e_cli_005_sessions_resume_show_delete_and_clear_use_isolated_storage() {
    let environment = schema3_environment();
    let server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(chat_stream("first-answer")),
        HttpResponse::sse(chat_stream("second-answer")),
    ])
    .unwrap();
    let base_url = format!("{}/v1", server.base_url());

    configure_fixture(&environment, &base_url);

    let first = environment
        .command(binary())
        .args(["--prompt", "first turn"])
        .env("RIVO_FIXTURE_KEY", "session-secret")
        .run()
        .unwrap();
    first.assert_success();
    assert!(first.stdout.contains("first-answer"));

    let list = run(&environment, &["sessions", "list"]);
    list.assert_success();
    let session_id = list
        .stdout
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .filter(|value| !value.is_empty())
        .unwrap()
        .to_owned();

    let resumed = environment
        .command(binary())
        .args(["sessions", "resume", &session_id, "--prompt", "second turn"])
        .env("RIVO_FIXTURE_KEY", "session-secret")
        .run()
        .unwrap();
    resumed.assert_success();
    assert!(resumed.stdout.contains("second-answer"));

    let show = run(&environment, &["sessions", "show", &session_id]);
    show.assert_success();
    assert!(show.stdout.contains("events:"));
    assert!(show.stdout.contains("messages:"));
    assert!(show.stdout.contains("first turn"));
    assert!(show.stdout.contains("second turn"));
    assert_redacted(&show.stdout, &["session-secret"]);

    let rejected_delete = run(&environment, &["sessions", "delete", &session_id]);
    rejected_delete.assert_failure();
    assert!(rejected_delete.stderr.contains("--yes"));
    run(&environment, &["sessions", "delete", &session_id, "--yes"]).assert_success();
    let after_delete = run(&environment, &["sessions", "list"]);
    after_delete.assert_success();
    assert!(after_delete.stdout.trim().is_empty());
    let rejected_clear = run(&environment, &["sessions", "clear"]);
    rejected_clear.assert_failure();
    assert!(rejected_clear.stderr.contains("--yes"));
    run(&environment, &["sessions", "clear", "--yes"]).assert_success();

    assert_eq!(server.finish().len(), 2);
}

#[test]
fn e2e_cli_sessions_migrate_yes_creates_backup_and_preserves_history() {
    let environment = schema3_environment();
    let database = environment.data_dir().join("rivo").join("rivo.db");
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let workspace = environment.workspace();
    {
        let store = rivo_storage::Store::open(&database).unwrap();
        store.ensure_session("legacy-session").unwrap();
        store
            .set_workspace_root("legacy-session", workspace.to_string_lossy().as_ref())
            .unwrap();
        store
            .append_event(&rivo_storage::StoredEvent {
                session_id: "legacy-session".into(),
                turn_id: "legacy-turn".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        store
            .append_message(&rivo_storage::StoredMessage {
                session_id: "legacy-session".into(),
                turn_id: "legacy-turn".into(),
                sequence: 1,
                role: "user".into(),
                body: "preserve this".into(),
            })
            .unwrap();
    }
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute("UPDATE schema_version SET version=17", [])
        .unwrap();
    drop(connection);

    let migrated = run(&environment, &["sessions", "migrate", "--yes"]);
    migrated.assert_success();
    assert!(migrated.stdout.contains("schema 17 -> schema 18"));
    assert!(migrated.stdout.contains("bound_sessions=1"));
    assert!(migrated.stdout.contains("unscoped_sessions=0"));
    assert_eq!(
        rivo_storage::inspect_database(&database).schema_version,
        Some(18)
    );

    let backup = std::fs::read_dir(database.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rivo-schema-17-backup-")
        })
        .expect("automatic migration backup");
    assert_eq!(
        rivo_storage::inspect_database(&backup).schema_version,
        Some(17)
    );
    let store = rivo_storage::Store::open(&database).unwrap();
    assert_eq!(store.list_sessions().unwrap().len(), 1);
    assert_eq!(store.replay("legacy-session").unwrap().len(), 1);
    assert_eq!(store.replay_messages("legacy-session").unwrap().len(), 1);
}

#[test]
fn e2e_cli_006_safe_legacy_config_is_upgraded_by_a_write_command() {
    let environment = schema3_environment();
    std::fs::write(
        environment.config_path(),
        r#"
provider = "openai_chat_completions"
model = "gpt-5"
base_url = "http://127.0.0.1:18081/v1"
credential_name = "legacy-key"
capability_profile = "general"
policy_profile = "workspace"
"#,
    )
    .unwrap();

    run(
        &environment,
        &["config", "set", "workspace", "C:\\legacy-workspace"],
    )
    .assert_success();
    let content = std::fs::read_to_string(environment.config_path()).unwrap();
    assert!(content.contains("version = 6"));
    assert!(content.contains("model = \"gpt-5\""));
    assert!(content.contains("credential = \"legacy-key\""));
}

#[test]
fn e2e_cli_006_legacy_config_queries_are_compatible_without_writing() {
    let environment = schema3_environment();
    let content = "version = 3\n[supervision]\nenabled = false\n";
    std::fs::write(environment.config_path(), content).unwrap();

    for args in [
        ["config", "vendor", "list"],
        ["config", "model", "list"],
        ["config", "provider", "list"],
    ] {
        run(&environment, &args).assert_success();
        assert_eq!(
            std::fs::read_to_string(environment.config_path()).unwrap(),
            content
        );
    }
}

#[test]
fn e2e_cli_006_unsafe_legacy_config_is_rejected_without_overwrite() {
    let environment = schema3_environment();
    let content = r#"
provider = "openai_chat_completions"
model = "gpt-4.1"
base_url = "http://127.0.0.1:18081/v1"
api_key_env = "RIVO_LEGACY_KEY"
"#;
    std::fs::write(environment.config_path(), content).unwrap();

    let rejected = run(
        &environment,
        &["config", "set", "workspace", "C:\\legacy-workspace"],
    );
    rejected.assert_failure();
    assert_eq!(
        content,
        std::fs::read_to_string(environment.config_path()).unwrap()
    );
}

#[test]
fn schema3_catalog_queries_are_offline_and_provider_updates_require_current_confirmation() {
    let environment = schema3_environment();
    configure_fixture(&environment, "http://127.0.0.1:9/v1");
    let before = std::fs::read(environment.config_path()).unwrap();
    for args in [
        vec!["config", "show"],
        vec!["config", "validate"],
        vec!["config", "vendor", "list"],
        vec!["config", "model", "list"],
        vec!["config", "model", "providers", "fixture-model"],
        vec!["config", "provider", "models", "list", "fixture"],
        vec![
            "config",
            "route",
            "--vendor",
            "fixture",
            "--model",
            "fixture-model",
        ],
    ] {
        run(&environment, &args).assert_success();
        assert_eq!(before, std::fs::read(environment.config_path()).unwrap());
    }
    let args = [
        "config",
        "provider",
        "set",
        "fixture",
        "--base-url",
        "http://127.0.0.1:10/v1",
    ];
    let planned = run(&environment, &args);
    planned.assert_failure();
    assert_eq!(before, std::fs::read(environment.config_path()).unwrap());
    let plan: serde_json::Value = serde_json::from_str(&planned.stdout).unwrap();
    assert_eq!(
        plan["provider_impacts"][0]["affected_models"][0]["model"],
        "fixture-model"
    );
    let mut confirmed = args.to_vec();
    confirmed.extend(["--confirm", plan["confirmation_token"].as_str().unwrap()]);
    run(&environment, &confirmed).assert_success();
    let file = rivo_session::read_file_config(environment.config_path()).unwrap();
    assert!(file.providers["fixture"].discovery.models.is_empty());
    run(&environment, &confirmed).assert_failure();
    run(
        &environment,
        &[
            "config",
            "route",
            "--vendor",
            "fixture",
            "--model",
            "fixture-model",
        ],
    )
    .assert_failure();
}

/// 远端 MCP OAuth 真机冒烟骨架（默认 `#[ignore]`，不进 CI）。
///
/// 验证对官方远端 MCP server（OAuth 2.1 授权码 + PKCE）的只读接入：安装 → 交互式登录后 →
/// 发现/连接只读工具，并核对 `mcp show` 不泄漏任何 token。因需真实浏览器完成 Entra/Atlassian
/// 授权，本用例无法自动化，故标记为 ignore，由操作者按下述步骤手动触发。
///
/// 前置（一次性，写入真实 OS keychain，命名空间 `rivo-mcp-oauth`）：
/// 1. 用真实的 `rivo` 二进制安装并登录目标 server，例如：
///    `rivo mcp install http ado https://mcp.dev.azure.com/<org> --capability tools \`
///    `  --toolset work:read_only:wit_list_work_items --oauth --auto-login`
///    `rivo mcp login ado`（浏览器完成授权，token 存入 keychain）。
///
/// 运行：
/// ```powershell
/// $env:RIVO_REMOTE_MCP_SMOKE = "1"
/// $env:RIVO_REMOTE_MCP_NAME = "ado"
/// $env:RIVO_REMOTE_MCP_URL = "https://mcp.dev.azure.com/<org>"
/// $env:RIVO_REMOTE_MCP_TOOLSET = "work:read_only:wit_list_work_items"
/// cargo test -p rivo-cli --test cli_black_box -- --ignored e2e_remote_mcp_oauth_readonly_smoke
/// ```
///
/// 注意：keychain 为 OS 全局，独立于本用例的临时配置目录；因此隔离环境安装同名 server 后，
/// connect 仍能从 keychain 载入前置步骤写入的 token。
#[test]
#[ignore = "需真实浏览器 OAuth 与已登录的 keychain 凭据，手动触发"]
fn e2e_remote_mcp_oauth_readonly_smoke() {
    // 未显式启用则视为通过（保护 `--include-ignored` 的批量运行不至于误失败）。
    if std::env::var("RIVO_REMOTE_MCP_SMOKE").as_deref() != Ok("1") {
        eprintln!("跳过：未设置 RIVO_REMOTE_MCP_SMOKE=1");
        return;
    }
    let name = std::env::var("RIVO_REMOTE_MCP_NAME")
        .expect("启用冒烟需设置 RIVO_REMOTE_MCP_NAME（如 ado）");
    let url = std::env::var("RIVO_REMOTE_MCP_URL")
        .expect("启用冒烟需设置 RIVO_REMOTE_MCP_URL（如 https://mcp.dev.azure.com/<org>）");
    let toolset = std::env::var("RIVO_REMOTE_MCP_TOOLSET")
        .expect("启用冒烟需设置 RIVO_REMOTE_MCP_TOOLSET（如 work:read_only:wit_list_work_items）");

    let environment = schema3_environment();

    // 安装 OAuth server；首次未登录时实时发现会被跳过并落配置，install 仍应成功。
    let install = run(
        &environment,
        &[
            "mcp",
            "install",
            "http",
            &name,
            &url,
            "--capability",
            "tools",
            "--toolset",
            &toolset,
            "--oauth",
            "--auto-login",
        ],
    );
    install.assert_success();

    // show 只暴露变量名与鉴权类型，绝不出现 token 明文。
    let show = run(&environment, &["mcp", "show", &name]);
    show.assert_success();
    assert!(show.stdout.contains("auth=oauth"));
    assert!(show.stdout.contains("transport=http"));

    // inspect 触发 connect：从 keychain 载入 token（过期则静默刷新），列出只读工具。
    let inspect = run(&environment, &["mcp", "inspect", &name]);
    inspect.assert_success();
    assert!(
        inspect.stdout.contains("status=ready"),
        "inspect 未就绪：stdout={} stderr={}",
        inspect.stdout,
        inspect.stderr
    );
}

#[test]
fn e2e_app_server_config_management_lists_credentials_catalog_and_typed_errors() {
    let _guard = lock_app_server_test();
    let environment = schema3_environment();
    configure_fixture(&environment, "http://127.0.0.1:1/v1");
    let mut file: rivo_session::FileConfig =
        toml::from_str(&std::fs::read_to_string(environment.config_path()).unwrap()).unwrap();
    file.providers.get_mut("fixture").unwrap().auth.credential = Some("used-key".into());
    let mut orphan = file.providers["fixture"].clone();
    orphan.auth.credential = Some("ghost-key".into());
    file.providers.insert("orphan".into(), orphan);
    std::fs::write(
        environment.config_path(),
        toml::to_string_pretty(&file).unwrap(),
    )
    .unwrap();
    set_test_credential(&environment, "used-key");
    set_test_credential(&environment, "spare-key");

    let mut child = start_app_server(&environment);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut call = |id: u64, method: &str, params: serde_json::Value| {
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        let response = read_rpc(&mut stdout);
        assert_eq!(response["id"], id);
        response
    };

    let init = call(
        1,
        "initialize",
        serde_json::json!({"protocol_version":"rivo-app-server-v2","client":{"name":"config-fixture","version":"1"}}),
    );
    let config_caps = &init["result"]["capabilities"]["model_routing_config"];
    assert_eq!(config_caps["credential_list"], true);
    assert_eq!(config_caps["catalog"], true);

    let read = call(2, "model_routing/read", serde_json::json!({}));
    let revision = read["result"]["revision"].as_str().unwrap().to_owned();
    let raw = read["result"]["raw_toml"].as_str().unwrap();
    assert!(raw.contains("used-key"));
    assert!(!raw.contains("test-only-credential-value"));

    let list = call(3, "credential/list", serde_json::json!({}));
    let credentials = list["result"]["credentials"].as_array().unwrap();
    let used = credentials
        .iter()
        .find(|item| item["name"] == "used-key")
        .unwrap();
    assert_eq!(used["referenced_by"], serde_json::json!(["fixture"]));
    assert_eq!(used["encrypted"], true);
    let spare = credentials
        .iter()
        .find(|item| item["name"] == "spare-key")
        .unwrap();
    assert_eq!(spare["referenced_by"], serde_json::json!([]));
    assert_eq!(
        list["result"]["missing"],
        serde_json::json!([{"name":"ghost-key","referenced_by":["orphan"]}])
    );
    assert!(!list.to_string().contains("test-only-credential-value"));

    let in_use = call(
        4,
        "credential/delete",
        serde_json::json!({"name":"used-key"}),
    );
    assert_eq!(in_use["error"]["code"], "credential_in_use");
    assert_eq!(in_use["error"]["detail"], "fixture");

    let deleted = call(
        5,
        "credential/delete",
        serde_json::json!({"name":"spare-key"}),
    );
    assert_eq!(deleted["result"]["deleted"], true);
    let again = call(
        6,
        "credential/delete",
        serde_json::json!({"name":"spare-key"}),
    );
    assert_eq!(again["error"]["code"], "credential_not_found");

    let stale = call(
        7,
        "model_routing/commit",
        serde_json::json!({"revision":"stale","config":read["result"]["config"]}),
    );
    assert_eq!(stale["error"]["code"], "revision_conflict");

    let mut invalid = read["result"]["config"].clone();
    invalid["providers"]["fixture"]["base_url"] =
        serde_json::json!("https://example.test/v1/responses");
    let rejected = call(
        8,
        "model_routing/validate",
        serde_json::json!({"revision":revision,"config":invalid}),
    );
    assert_eq!(rejected["error"]["code"], "configuration_invalid");
    assert!(!rejected["error"]["detail"].as_str().unwrap().is_empty());

    let catalog = call(9, "catalog/read", serde_json::json!({}));
    let vendors = catalog["result"]["vendors"].as_array().unwrap();
    assert!(vendors
        .iter()
        .any(|vendor| vendor["id"] == "fixture" && vendor["source"] == "local"));
    assert!(vendors
        .iter()
        .any(|vendor| vendor["id"] == "openai" && vendor["source"] == "builtin"));
    let models = catalog["result"]["models"].as_array().unwrap();
    assert!(models
        .iter()
        .any(|model| model["id"] == "fixture-model" && model["builtin"].is_null()));
    assert!(models.iter().any(|model| model["id"] == "gpt-6-luna"
        && model["source"] == "builtin"
        && model["status"] == "active"
        && model["replaced_by"].is_null()));
    assert_eq!(catalog["result"]["protocols"].as_array().unwrap().len(), 3);
    assert_eq!(
        catalog["result"]["catalog_revision"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(catalog["result"]["warnings"].is_array());

    child.kill().ok();
    child.wait().ok();
}
