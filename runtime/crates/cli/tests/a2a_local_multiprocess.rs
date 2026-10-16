//! Cross-platform local A2A smoke. It is ignored because it writes disposable
//! test identities to the host keychain, but every A2A endpoint is a real
//! child process using loopback HTTPS and mTLS.

use rivo_test_support::{HttpResponse, ScriptedHttpServer};
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod support;

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_rivo"))
}

fn run(config: &Path, data: &Path, workspace: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .args(args)
        .env("RIVO_CONFIG", config)
        .env("RIVO_DATA_DIR", data)
        .env("RIVO_WORKSPACE", workspace)
        .env("RIVO_A2A_LOCAL_TEST_KEY", "redacted-test-key")
        .env("RIVO_A2A_LOCAL_TEST", "1")
        .env("RIVO_A2A_LOCAL_TEST_CONFIRM", "1")
        .output()
        .expect("start local A2A command")
}

fn assert_success(output: &std::process::Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn field(output: &std::process::Output, name: &str) -> String {
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .find_map(|value| value.strip_prefix(name))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("missing {name} in {text}"))
}

fn pending_pairing_id(output: &std::process::Output) -> Option<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.contains("state=Pending"))
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|value| value.strip_prefix("request_id="))
        })
        .map(str::to_owned)
}

fn write_evidence_report() {
    let Ok(path) = std::env::var("RIVO_A2A_LOCAL_REPORT_PATH") else {
        return;
    };
    let commit =
        std::env::var("RIVO_A2A_LOCAL_CANDIDATE_COMMIT").expect("A2A report candidate commit");
    assert!(
        commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "A2A report candidate commit must be a full SHA-1"
    );
    let report_path = Path::new(&path);
    assert_eq!(
        report_path.extension().and_then(|value| value.to_str()),
        Some("md"),
        "A2A evidence report must be Markdown"
    );
    let report = format!(
        "# Rivo A2A 本机多进程测试报告\n\n- Candidate commit: `{commit}`\n- Result: `passed`\n- OS/arch: `{}/{}`\n- Transport: `loopback mTLS`\n- Bind address category: `loopback only`\n- Process roles: `Server`, `Client`, `independent third Client`\n- Child-process, data-directory and SQLite isolation: `passed`\n- Terminal-state uniqueness and recovery assertions: `passed`\n- Identity cleanup: `passed`\n\n## 覆盖场景\n\n- Card 查询、SAS 配对、owner 批准、PeerGrant 与 allowlist 重启。\n- active task 的 GetTask/SSE revision recovery、Server 中断恢复与终态唯一性。\n- 流式完成、取消、TLS/SPKI 轮换和 Card 轮换挂起。\n- 独立第三 Client 的跨 peer recover 在联网前拒绝；不复制任务映射。\n\n本报告不包含 workspace 路径、监听端口、task/peer/request ID、SPKI、任务正文、凭据或 identity 引用。\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    std::fs::write(report_path, report).expect("write redacted A2A evidence report");
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

fn wait_for_listener(port: u16, server: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            drop(stream);
            return;
        }
        if let Some(status) = server.try_wait().expect("poll A2A server") {
            panic!("A2A server exited early: {status}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("A2A server did not bind loopback before timeout");
}

struct ServerChild(Child);

impl ServerChild {
    fn child_mut(&mut self) -> &mut Child {
        &mut self.0
    }

    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    fn replace(&mut self, child: Child) {
        self.stop();
        self.0 = child;
    }
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        self.stop();
    }
}

struct IdentityCleanup {
    config: std::path::PathBuf,
    data: std::path::PathBuf,
    workspace: std::path::PathBuf,
    pairs: Vec<(String, String)>,
    armed: bool,
}

impl IdentityCleanup {
    fn new(config: &Path, data: &Path, workspace: &Path, server: String, issuer: String) -> Self {
        Self {
            config: config.to_path_buf(),
            data: data.to_path_buf(),
            workspace: workspace.to_path_buf(),
            pairs: vec![(server, issuer)],
            armed: true,
        }
    }

    fn add(&mut self, server: String, issuer: String) {
        self.pairs.push((server, issuer));
    }

    fn cleanup(&mut self) -> bool {
        if !self.armed {
            return true;
        }
        self.armed = false;
        self.pairs.iter().all(|(server, issuer)| {
            run(
                &self.config,
                &self.data,
                &self.workspace,
                &[
                    "a2a",
                    "provision",
                    "cleanup",
                    "--yes",
                    "--server-identity-ref",
                    server,
                    "--issuer-ref",
                    issuer,
                ],
            )
            .status
            .success()
        })
    }
}

impl Drop for IdentityCleanup {
    fn drop(&mut self) {
        // A failed assertion must not leave disposable a2a-test-* entries in
        // the host keychain. Drop cannot surface an error, so the successful
        // path below still asserts cleanup explicitly.
        let _ = self.cleanup();
    }
}

#[test]
#[ignore = "requires a host OS keychain and creates disposable a2a-test-* identities"]
fn local_multiprocess_mtls_gate() {
    let root = tempfile::tempdir().expect("temporary root");
    let server_root = root.path().join("server");
    let client_root = root.path().join("client");
    let workspace = server_root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&client_root).expect("client data directory");
    std::fs::create_dir_all(client_root.join("rivo")).expect("client Rivo data directory");
    let server_config = server_root.join("config.toml");
    std::fs::create_dir_all(server_config.parent().expect("config parent"))
        .expect("config directory");
    support::write_routing_fixture(
        &server_config,
        &workspace,
        "http://127.0.0.1:9/v1",
        "a2a-local-fixture",
        "RIVO_A2A_LOCAL_TEST_KEY",
    );

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve port")
        .local_addr()
        .expect("reserved address")
        .port();
    let bind = format!("127.0.0.1:{port}");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let server_identity = format!("a2a-test-local-server-{}-{stamp}", std::process::id());
    let issuer = format!("a2a-test-local-issuer-{}-{stamp}", std::process::id());
    let mut identity_cleanup = IdentityCleanup::new(
        &server_config,
        &server_root,
        &workspace,
        server_identity.clone(),
        issuer.clone(),
    );
    let provision = run(
        &server_config,
        &server_root,
        &workspace,
        &[
            "a2a",
            "provision",
            "--yes",
            "--bind",
            &bind,
            "--server-identity-ref",
            &server_identity,
            "--issuer-ref",
            &issuer,
        ],
    );
    assert_success(&provision, "provision");
    let spki = field(&provision, "spki_sha256=");

    let owner_config = server_root.join("owner-a2a-server.toml");
    let workspace_toml = workspace.to_string_lossy().replace('\\', "/");
    std::fs::write(
        &owner_config,
        format!(
            "schema_version = 1\nstate_db = \"state.db\"\n\n[workspace_aliases]\nresearch = \"{workspace_toml}\"\n\n[[profiles]]\nidentity_ref = \"{server_identity}\"\n\n[profiles.profile]\nname = \"research-lan\"\nbind = \"{bind}\"\nskill_id = \"research\"\nworkspace_alias = \"research\"\ncapability_profile = \"research\"\npolicy_ceiling = \"strict\"\nnetwork_enabled = false\nbrowser_enabled = false\nmcp_servers = []\nmcp_toolsets = {{}}\nper_peer_max_active_tasks = 1\nmax_active_tasks = 3\nmax_message_bytes = 65536\nmax_output_bytes = 262144\nmax_duration_seconds = 1800\napproval_timeout_seconds = 300\nallowed_peers = []\nissuer_ref = \"{issuer}\"\nidempotency_retention_seconds = 86400\naudit_retention_days = 30\n"
        ),
    )
    .expect("owner config");

    let mut server = ServerChild(
        Command::new(binary())
            .args(["a2a", "serve", "research-lan", "--config"])
            .arg(&owner_config)
            .env("RIVO_CONFIG", &server_config)
            .env("RIVO_DATA_DIR", &server_root)
            .env("RIVO_WORKSPACE", &workspace)
            .env("RIVO_A2A_LOCAL_TEST_KEY", "redacted-test-key")
            .env("RIVO_A2A_LOCAL_TEST", "1")
            .env("RIVO_A2A_SMOKE_HOLD_MS", "30000")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start A2A server child"),
    );
    wait_for_listener(port, server.child_mut());

    let client_config = client_root.join("config.toml");
    std::fs::write(&client_config, "version = 6\n").expect("client config");
    let endpoint = format!("https://{bind}/a2a");
    let pair = Command::new(binary())
        .args([
            "a2a",
            "pair",
            "a2a-test-local-client",
            "--endpoint",
            &endpoint,
            "--spki-sha256",
            &spki,
        ])
        .env("RIVO_CONFIG", &client_config)
        .env("RIVO_DATA_DIR", &client_root)
        .env("RIVO_A2A_LOCAL_TEST", "1")
        .env("RIVO_A2A_LOCAL_TEST_CONFIRM", "1")
        .env("RIVO_A2A_LOCAL_TEST_CONFIRM_DELAY_MS", "1500")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start paired client");
    // The client deliberately waits in the test-only SAS confirmation window.
    // This owner decision remains a separate process and state-store operation.
    let mut request_id = None;
    for _ in 0..20 {
        let list = run(
            &server_config,
            &server_root,
            &workspace,
            &[
                "a2a",
                "pairings",
                "list",
                "--config",
                owner_config.to_str().expect("utf8 config"),
            ],
        );
        if list.status.success() {
            request_id = pending_pairing_id(&list);
            if request_id.is_some() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    let request_id = match request_id {
        Some(request_id) => request_id,
        None => {
            let output = pair.wait_with_output().expect("wait failed pairing client");
            panic!(
                "pending pairing request was not persisted\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };
    let approve = run(
        &server_config,
        &server_root,
        &workspace,
        &[
            "a2a",
            "pairings",
            "approve",
            &request_id,
            "--config",
            owner_config.to_str().expect("utf8 config"),
        ],
    );
    assert_success(&approve, "owner pairing approval");
    let paired = pair.wait_with_output().expect("wait pairing client");
    assert_success(&paired, "mTLS pairing");
    let peer_id = field(&paired, "peer_id=");

    // `allowed_peers` is a frozen owner profile field. Pairing creates a grant,
    // then the owner deliberately adds that opaque peer ID and restarts before
    // it may submit work; the test must not bypass this authorization boundary.
    let owner_text = std::fs::read_to_string(&owner_config).expect("read owner config");
    std::fs::write(
        &owner_config,
        owner_text.replace(
            "allowed_peers = []",
            &format!("allowed_peers = [\"{peer_id}\"]"),
        ),
    )
    .expect("authorize paired peer");
    server.replace(
        Command::new(binary())
            .args(["a2a", "serve", "research-lan", "--config"])
            .arg(&owner_config)
            .env("RIVO_CONFIG", &server_config)
            .env("RIVO_DATA_DIR", &server_root)
            .env("RIVO_WORKSPACE", &workspace)
            .env("RIVO_A2A_LOCAL_TEST_KEY", "redacted-test-key")
            .env("RIVO_A2A_LOCAL_TEST", "1")
            .env("RIVO_A2A_SMOKE_HOLD_MS", "30000")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("restart A2A server child"),
    );
    wait_for_listener(port, server.child_mut());

    let mut delegate = Command::new(binary())
        .args([
            "a2a",
            "delegate",
            &peer_id,
            "research",
            "--message-stdin",
            "--yes",
        ])
        .env("RIVO_CONFIG", &client_config)
        .env("RIVO_DATA_DIR", &client_root)
        .env("RIVO_A2A_LOCAL_TEST", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start delegation client");
    delegate
        .stdin
        .take()
        .expect("delegate stdin")
        .write_all(b"local cancellation smoke")
        .expect("delegate body");
    let delegated = delegate.wait_with_output().expect("wait delegation");
    assert_success(&delegated, "delegation");
    let recovery_task_id = field(&delegated, "task_id=");
    // While the task is genuinely active, recovery must consume its GetTask
    // snapshot and the SSE snapshot with a durable revision. A duplicate
    // snapshot is intentionally deduplicated; it must never resend the body.
    let active_recovered = run(
        &client_config,
        &client_root,
        &workspace,
        &["a2a", "recover", &peer_id, &recovery_task_id],
    );
    assert_success(&active_recovered, "recover active task through SSE");
    let active_recovery_output = String::from_utf8_lossy(&active_recovered.stdout);
    assert!(
        active_recovery_output.contains("state=Working")
            && active_recovery_output.contains("revision=Some("),
        "active recovery must retain the SSE snapshot revision"
    );
    // An interrupted task is never replayed. Restart recovery fixes it to a
    // terminal FAILED state, and the client must obtain that state via its
    // persisted mapping rather than submit the business message a second time.
    server.replace(
        Command::new(binary())
            .args(["a2a", "serve", "research-lan", "--config"])
            .arg(&owner_config)
            .env("RIVO_CONFIG", &server_config)
            .env("RIVO_DATA_DIR", &server_root)
            .env("RIVO_WORKSPACE", &workspace)
            .env("RIVO_A2A_LOCAL_TEST_KEY", "redacted-test-key")
            .env("RIVO_A2A_LOCAL_TEST", "1")
            .env("RIVO_A2A_SMOKE_HOLD_MS", "30000")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("restart after interrupted task"),
    );
    wait_for_listener(port, server.child_mut());
    let recovered = run(
        &client_config,
        &client_root,
        &workspace,
        &["a2a", "recover", &peer_id, &recovery_task_id],
    );
    assert_success(&recovered, "recover interrupted task");
    assert!(
        String::from_utf8_lossy(&recovered.stdout).contains("state=Failed"),
        "recover must observe the Server terminal state"
    );

    let mut cancel_delegate = Command::new(binary())
        .args([
            "a2a",
            "delegate",
            &peer_id,
            "research",
            "--message-stdin",
            "--yes",
        ])
        .env("RIVO_CONFIG", &client_config)
        .env("RIVO_DATA_DIR", &client_root)
        .env("RIVO_A2A_LOCAL_TEST", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start cancellation delegation client");
    cancel_delegate
        .stdin
        .take()
        .expect("cancellation delegation stdin")
        .write_all(b"local cancellation smoke")
        .expect("cancellation delegation body");
    let cancel_delegated = cancel_delegate
        .wait_with_output()
        .expect("wait cancellation delegation");
    assert_success(&cancel_delegated, "cancellation delegation");
    let task_id = field(&cancel_delegated, "task_id=");
    let cancel = run(
        &client_config,
        &client_root,
        &workspace,
        &["a2a", "cancel", &peer_id, &task_id, "--yes"],
    );
    assert_success(&cancel, "cancel");

    let provider = ScriptedHttpServer::start(vec![HttpResponse::sse(chat_stream("local result"))])
        .expect("local provider fixture");
    let provider_url = format!("{}/v1", provider.base_url());
    server.stop();
    support::write_routing_fixture(
        &server_config,
        &workspace,
        &provider_url,
        "a2a-local-fixture",
        "RIVO_A2A_LOCAL_TEST_KEY",
    );
    server.replace(
        Command::new(binary())
            .args(["a2a", "serve", "research-lan", "--config"])
            .arg(&owner_config)
            .env("RIVO_CONFIG", &server_config)
            .env("RIVO_DATA_DIR", &server_root)
            .env("RIVO_WORKSPACE", &workspace)
            .env("RIVO_A2A_LOCAL_TEST_KEY", "redacted-test-key")
            .env("RIVO_A2A_LOCAL_TEST", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("restart completed-task server"),
    );
    wait_for_listener(port, server.child_mut());
    let mut completed_delegate = Command::new(binary())
        .args([
            "a2a",
            "delegate",
            &peer_id,
            "research",
            "--message-stdin",
            "--yes",
        ])
        .env("RIVO_CONFIG", &client_config)
        .env("RIVO_DATA_DIR", &client_root)
        .env("RIVO_A2A_LOCAL_TEST", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start completed delegation client");
    completed_delegate
        .stdin
        .take()
        .expect("completed delegation stdin")
        .write_all(b"local completed smoke")
        .expect("completed delegation body");
    let completed_delegated = completed_delegate
        .wait_with_output()
        .expect("wait completed delegation");
    assert_success(&completed_delegated, "completed delegation");
    let completed_task_id = field(&completed_delegated, "task_id=");
    thread::sleep(Duration::from_millis(100));
    let completed = run(
        &client_config,
        &client_root,
        &workspace,
        &["a2a", "recover", &peer_id, &completed_task_id],
    );
    assert_success(&completed, "recover completed task");
    assert!(
        String::from_utf8_lossy(&completed.stdout).contains("state=Completed"),
        "recover must observe the completed task state"
    );
    assert_eq!(
        provider.finish().len(),
        1,
        "completed task must call fixture once"
    );

    // TLS/SPKI rotation is an independent trust boundary. Rotate the Server
    // identity first, then make the original Client perform a real request
    // with its persisted pin. It must suspend before the body is delegated.
    let rotated_server_identity = format!(
        "a2a-test-local-server-rotated-{}-{stamp}",
        std::process::id()
    );
    let rotated_issuer = format!(
        "a2a-test-local-issuer-rotated-{}-{stamp}",
        std::process::id()
    );
    identity_cleanup.add(rotated_server_identity.clone(), rotated_issuer.clone());
    server.stop();
    support::write_routing_fixture(
        &server_config,
        &workspace,
        "http://127.0.0.1:9/v1",
        "a2a-local-fixture",
        "RIVO_A2A_LOCAL_TEST_KEY",
    );
    let rotated_provision = run(
        &server_config,
        &server_root,
        &workspace,
        &[
            "a2a",
            "provision",
            "--yes",
            "--bind",
            &bind,
            "--server-identity-ref",
            &rotated_server_identity,
            "--issuer-ref",
            &rotated_issuer,
        ],
    );
    assert_success(&rotated_provision, "provision rotated TLS identity");
    let rotated_spki = field(&rotated_provision, "spki_sha256=");
    let owner_text = std::fs::read_to_string(&owner_config).expect("read TLS-rotated owner config");
    std::fs::write(
        &owner_config,
        owner_text
            .replace(
                &format!("identity_ref = \"{server_identity}\""),
                &format!("identity_ref = \"{rotated_server_identity}\""),
            )
            .replace(
                &format!("issuer_ref = \"{issuer}\""),
                &format!("issuer_ref = \"{rotated_issuer}\""),
            ),
    )
    .expect("rotate Server TLS identity");
    server.replace(
        Command::new(binary())
            .args(["a2a", "serve", "research-lan", "--config"])
            .arg(&owner_config)
            .env("RIVO_CONFIG", &server_config)
            .env("RIVO_DATA_DIR", &server_root)
            .env("RIVO_WORKSPACE", &workspace)
            .env("RIVO_A2A_LOCAL_TEST_KEY", "redacted-test-key")
            .env("RIVO_A2A_LOCAL_TEST", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("restart TLS-rotated server"),
    );
    wait_for_listener(port, server.child_mut());
    let mut tls_rotated = Command::new(binary())
        .args([
            "a2a",
            "delegate",
            &peer_id,
            "research",
            "--message-stdin",
            "--yes",
        ])
        .env("RIVO_CONFIG", &client_config)
        .env("RIVO_DATA_DIR", &client_root)
        .env("RIVO_A2A_LOCAL_TEST", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start TLS-rotated delegation client");
    tls_rotated
        .stdin
        .take()
        .expect("TLS-rotated delegate stdin")
        .write_all(b"must not be delegated after TLS rotation")
        .expect("TLS-rotated delegate body");
    let tls_rotated = tls_rotated
        .wait_with_output()
        .expect("wait TLS-rotated delegation");
    assert!(
        !tls_rotated.status.success()
            && String::from_utf8_lossy(&tls_rotated.stderr).contains("a2a_peer_suspended"),
        "TLS/SPKI rotation must suspend the pinned peer before delegation"
    );
    let inspection = run(
        &client_config,
        &client_root,
        &workspace,
        &["a2a", "inspect", &peer_id],
    );
    assert_success(&inspection, "inspect TLS-suspended peer");
    assert!(
        String::from_utf8_lossy(&inspection.stdout).contains("status=Suspended"),
        "TLS/SPKI rotation must persist suspended trust state"
    );

    // Establish a fresh trust snapshot after the TLS rotation. The following
    // Card test must use this peer so its failure cannot be attributed to the
    // prior TLS/SPKI mismatch.
    let card_client_root = root.path().join("card-client");
    std::fs::create_dir_all(card_client_root.join("rivo")).expect("Card client data directory");
    let card_client_config = card_client_root.join("config.toml");
    std::fs::write(&card_client_config, "version = 6\n").expect("Card client config");
    let card_pair = Command::new(binary())
        .args([
            "a2a",
            "pair",
            "a2a-test-local-card-client",
            "--endpoint",
            &endpoint,
            "--spki-sha256",
            &rotated_spki,
        ])
        .env("RIVO_CONFIG", &card_client_config)
        .env("RIVO_DATA_DIR", &card_client_root)
        .env("RIVO_A2A_LOCAL_TEST", "1")
        .env("RIVO_A2A_LOCAL_TEST_CONFIRM", "1")
        .env("RIVO_A2A_LOCAL_TEST_CONFIRM_DELAY_MS", "1500")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Card test pairing client");
    let mut card_request_id = None;
    for _ in 0..20 {
        let list = run(
            &server_config,
            &server_root,
            &workspace,
            &[
                "a2a",
                "pairings",
                "list",
                "--config",
                owner_config.to_str().expect("utf8 config"),
            ],
        );
        if list.status.success() {
            card_request_id = pending_pairing_id(&list);
            if card_request_id.is_some() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    let card_request_id = card_request_id.expect("pending Card test pairing request");
    let approve = run(
        &server_config,
        &server_root,
        &workspace,
        &[
            "a2a",
            "pairings",
            "approve",
            &card_request_id,
            "--config",
            owner_config.to_str().expect("utf8 config"),
        ],
    );
    assert_success(&approve, "Card test owner pairing approval");
    let card_paired = card_pair
        .wait_with_output()
        .expect("wait Card test pairing client");
    assert_success(&card_paired, "Card test mTLS pairing");
    let card_peer_id = field(&card_paired, "peer_id=");
    let owner_text = std::fs::read_to_string(&owner_config).expect("read Card peer owner config");
    std::fs::write(
        &owner_config,
        owner_text.replace(
            &format!("allowed_peers = [\"{peer_id}\"]"),
            &format!("allowed_peers = [\"{peer_id}\", \"{card_peer_id}\"]"),
        ),
    )
    .expect("authorize Card test peer");
    server.replace(
        Command::new(binary())
            .args(["a2a", "serve", "research-lan", "--config"])
            .arg(&owner_config)
            .env("RIVO_CONFIG", &server_config)
            .env("RIVO_DATA_DIR", &server_root)
            .env("RIVO_WORKSPACE", &workspace)
            .env("RIVO_A2A_LOCAL_TEST_KEY", "redacted-test-key")
            .env("RIVO_A2A_LOCAL_TEST", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("restart Card test server"),
    );
    wait_for_listener(port, server.child_mut());

    // A separately paired third Client deliberately has no task mapping for
    // the original peer. It must fail locally before opening a recovery
    // request; copying mappings or cross-peer task recovery would break mTLS
    // ownership isolation.
    let third_client_recover = run(
        &card_client_config,
        &card_client_root,
        &workspace,
        &["a2a", "recover", &card_peer_id, &recovery_task_id],
    );
    assert!(
        !third_client_recover.status.success()
            && String::from_utf8_lossy(&third_client_recover.stderr)
                .contains("No persisted task mapping exists locally"),
        "a separately paired Client must not recover another peer's task"
    );

    // A changed Card is a trust boundary, not a live capability refresh. The
    // old client snapshot must suspend before its next business request.
    let owner_text = std::fs::read_to_string(&owner_config).expect("read rotated owner config");
    std::fs::write(
        &owner_config,
        owner_text.replace("skill_id = \"research\"", "skill_id = \"research-rotated\""),
    )
    .expect("rotate Card skill");
    server.replace(
        Command::new(binary())
            .args(["a2a", "serve", "research-lan", "--config"])
            .arg(&owner_config)
            .env("RIVO_CONFIG", &server_config)
            .env("RIVO_DATA_DIR", &server_root)
            .env("RIVO_WORKSPACE", &workspace)
            .env("RIVO_A2A_LOCAL_TEST_KEY", "redacted-test-key")
            .env("RIVO_A2A_LOCAL_TEST", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("restart Card-rotated server"),
    );
    wait_for_listener(port, server.child_mut());
    let mut rotated = Command::new(binary())
        .args([
            "a2a",
            "delegate",
            &card_peer_id,
            "research",
            "--message-stdin",
            "--yes",
        ])
        .env("RIVO_CONFIG", &card_client_config)
        .env("RIVO_DATA_DIR", &card_client_root)
        .env("RIVO_A2A_LOCAL_TEST", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Card-rotated delegation client");
    rotated
        .stdin
        .take()
        .expect("Card-rotated delegation stdin")
        .write_all(b"must not be delegated after Card rotation")
        .expect("Card-rotated delegation body");
    let rotated = rotated
        .wait_with_output()
        .expect("wait Card-rotated delegation");
    assert!(
        !rotated.status.success()
            && String::from_utf8_lossy(&rotated.stderr).contains("a2a_agent_card_changed"),
        "Card rotation must reject delegation without refreshing trust"
    );
    let inspection = run(
        &card_client_config,
        &card_client_root,
        &workspace,
        &["a2a", "inspect", &card_peer_id],
    );
    assert_success(&inspection, "inspect suspended peer");
    assert!(
        String::from_utf8_lossy(&inspection.stdout).contains("status=Suspended"),
        "Card rotation must persist suspended trust state"
    );

    server.stop();
    assert!(identity_cleanup.cleanup(), "identity cleanup");
    write_evidence_report();
}
