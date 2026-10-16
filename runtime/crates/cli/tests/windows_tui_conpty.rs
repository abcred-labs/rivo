// Explicit PTY smoke tests for the real `rivo tui` process.
// These tests are intentionally ignored: run with
// `cargo test -p rivo --test windows_tui_conpty -- --ignored --nocapture`.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
#[cfg(windows)]
use rivo_test_support::assert_redacted;
use rivo_test_support::{HttpResponse, ScriptedHttpServer, TestEnvironment};
use serde_json::json;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod support;

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_rivo"))
}

fn chat_stream(text: &str) -> String {
    format!(
        "data: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture-model\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":{text:?}}},\"finish_reason\":null}}]}}\n\ndata: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture-model\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
        text = text
    )
}

fn configure_fixture(environment: &TestEnvironment, base_url: &str) {
    support::write_routing_fixture(
        environment.config_path(),
        environment.workspace(),
        base_url,
        "fixture-model",
        "RIVO_FIXTURE_KEY",
    );
}

struct PtySession {
    pair: portable_pty::PtyPair,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    screen: Arc<Mutex<vt100::Parser>>,
}

impl PtySession {
    fn start(environment: &TestEnvironment) -> Self {
        let mut command = CommandBuilder::new(binary());
        command.arg("tui");
        Self::start_command(environment, command)
    }

    fn start_command(environment: &TestEnvironment, mut command: CommandBuilder) -> Self {
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open ConPTY");
        command.cwd(environment.workspace());
        for key in [
            "RIVO_CONFIG_CONTENT",
            "RIVO_PROVIDER_PROFILE",
            "RIVO_PROVIDER",
            "RIVO_VENDOR",
            "RIVO_MODEL",
            "RIVO_PROVIDER_ID",
            "RIVO_BASE_URL",
            "RIVO_API_KEY_ENV",
            "RIVO_CREDENTIAL_NAME",
            "RIVO_PROFILE",
            "RIVO_POLICY",
            "RIVO_LIVE_TEST",
            "RIVO_LIVE_MODEL",
            "RIVO_LIVE_API_KEY",
            "RIVO_LIVE_BASE_URL",
        ] {
            command.env_remove(key);
        }
        command.env("RIVO_CONFIG", environment.config_path());
        command.env("RIVO_DATA_DIR", environment.data_dir());
        command.env("RIVO_WORKSPACE", environment.workspace());
        command.env("HOME", environment.root());
        command.env("USERPROFILE", environment.root());
        command.env("LOCALAPPDATA", environment.data_dir());
        command.env("APPDATA", environment.data_dir());
        command.env("XDG_CONFIG_HOME", environment.root());
        command.env("XDG_DATA_HOME", environment.data_dir());
        command.env("RIVO_FIXTURE_KEY", "fixture-key");
        let child = pair.slave.spawn_command(command).expect("spawn tui");
        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("PTY writer")));
        let response_writer = Arc::clone(&writer);
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        let screen = Arc::new(Mutex::new(vt100::Parser::new(30, 120, 0)));
        let rendered = Arc::clone(&screen);
        thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            while let Ok(size) = reader.read(&mut chunk) {
                if size == 0 {
                    break;
                }
                captured
                    .lock()
                    .expect("output lock")
                    .extend_from_slice(&chunk[..size]);
                rendered
                    .lock()
                    .expect("screen lock")
                    .process(&chunk[..size]);
                // Windows crossterm asks the host for the cursor position when
                // querying the ConPTY size. A PTY test driver must answer it.
                if chunk[..size].windows(4).any(|window| window == b"\x1b[6n") {
                    let _ = response_writer
                        .lock()
                        .expect("writer lock")
                        .write_all(b"\x1b[1;1R");
                }
            }
        });
        Self {
            pair,
            writer,
            child,
            output,
            screen,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer
            .lock()
            .expect("writer lock")
            .write_all(bytes)
            .expect("write PTY input");
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            // ConPTY emits screen diffs, including cursor moves over unchanged
            // characters. Raw output (even ANSI-stripped) is not visible text.
            let found = self.text().contains(needle);
            if found {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    #[cfg(windows)]
    fn wait_until_missing(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.text().contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn text(&self) -> String {
        self.screen.lock().expect("screen lock").screen().contents()
    }

    fn resize(&self, rows: u16, cols: u16) {
        self.screen
            .lock()
            .expect("screen lock")
            .screen_mut()
            .set_size(rows, cols);
        self.pair
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize PTY");
    }

    fn wait_for_answer(&self, answer: &str) -> bool {
        self.wait_for(answer, Duration::from_secs(10))
            && self.wait_for("● Complete", Duration::from_secs(10))
    }

    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().expect("output lock")).into_owned()
    }

    fn finish(mut self) -> (i32, String) {
        let status = self.child.wait().expect("wait tui");
        (status.exit_code() as i32, self.text())
    }

    fn finish_with_timeout(mut self, timeout: Duration) -> (i32, String, bool) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return (status.exit_code() as i32, self.raw_text(), true);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let status = self.child.wait().expect("wait terminated tui");
                return (status.exit_code() as i32, self.raw_text(), false);
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn terminate(mut self) -> (i32, String) {
        let _ = self.child.kill();
        self.finish()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Assertion failures must not leave a fixture process behind.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
        // Dropping this session's PtyPair closes its ConPTY, including attached
        // descendants (the PowerShell launch variant). The reader keeps draining
        // the output pipe during ClosePseudoConsole. Never kill by process name.
    }
}

// No report files: --nocapture keeps each explicit run's evidence separate from
// the historical PTY reports. All expectations are reconstructed visible text.
#[cfg(windows)]
mod windows_input {
    use super::*;

    const PROMPT_TIMEOUT: Duration = Duration::from_secs(10);
    const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
    const REMOVE_PROMPT: &str = "确认删除 provider fixture？此操作不可恢复。 [y/N]";
    const FIRST_ROW: &str = "> Provider ID: default";
    const BASE_URL_ROW: &str = "> Base URL: https://api.openai.com/v1";
    const NO_ROW: &str = "> Save and discover models now (yes/no): no";
    const CANCEL_FORM: &str = "First-launch setup cancelled; configuration was not saved";

    #[derive(Clone, Copy, Debug)]
    enum Launch {
        Direct,
        PowerShell,
    }

    fn start(environment: &TestEnvironment, args: &[&str], launch: Launch) -> PtySession {
        let mut command = match launch {
            Launch::Direct => {
                let mut command = CommandBuilder::new(binary());
                command.args(args);
                command
            }
            Launch::PowerShell => {
                // Only the Cargo-provided executable and literal test arguments
                // enter this shell command. No user configuration or shell profile.
                let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
                let mut script = format!("& {}", quote(&binary().to_string_lossy()));
                for arg in args {
                    script.push(' ');
                    script.push_str(&quote(arg));
                }
                script.push_str("; exit $LASTEXITCODE");
                let shell = Path::new(&std::env::var_os("SystemRoot").expect("SystemRoot"))
                    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
                let mut command = CommandBuilder::new(shell);
                command.args(["-NoLogo", "-NoProfile", "-Command", &script]);
                command
            }
        };
        // Do not inherit API keys, routing overrides, proxies, or user startup
        // hooks. Keep only Windows process-loading essentials; the shared helper
        // sets HOME/config/data/workspace and a fake fixture key afterwards.
        command.env_clear();
        for key in ["SystemRoot", "WINDIR", "COMSPEC", "PATH", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.env("TEMP", environment.root());
        command.env("TMP", environment.root());
        println!("launch={launch:?}, args={args:?}");
        PtySession::start_command(environment, command)
    }

    fn expect_screen(session: &mut PtySession, prompt: &str) {
        expect_screen_with_timeout(session, prompt, PROMPT_TIMEOUT);
    }

    fn expect_screen_with_timeout(session: &mut PtySession, prompt: &str, timeout: Duration) {
        let found = session.wait_for(prompt, timeout);
        let screen = session.text();
        if !found {
            let status = session.child.try_wait().expect("query fixture status");
            panic!(
                "visible prompt timeout after {timeout:?}: {prompt:?}; \
                 child_status={status:?}\n{screen}"
            );
        }
        println!("visible prompt: {prompt}");
    }

    fn expect_exit(mut session: PtySession, message: Option<&str>, success: bool) {
        // Keep the last visible frame as evidence if Ctrl+C does not exit. Do
        // not call the legacy finish() helper, which waits without a deadline.
        let before = session.text();
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let status = loop {
            if let Some(status) = session.child.try_wait().expect("query fixture exit") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "exit timeout after {EXIT_TIMEOUT:?}; child still running\n{}",
                session.text()
            );
            thread::sleep(Duration::from_millis(25));
        };
        // Process exit can precede delivery of the final ConPTY screen diff.
        if let Some(message) = message {
            expect_screen(&mut session, message);
        }
        println!(
            "exit_code={}, success={}, visible before input/exit:\n{before}\nvisible final:\n{}",
            status.exit_code(),
            status.success(),
            session.text()
        );
        assert_eq!(
            status.success(),
            success,
            "unexpected fixture exit: {status:?}"
        );
    }

    fn empty_environment() -> TestEnvironment {
        let environment = TestEnvironment::new().expect("test environment");
        // TestEnvironment's generic default is legacy version 2. First launch
        // must reach the schema 3 form, not the legacy-config rejection branch.
        std::fs::write(
            environment.config_path(),
            toml::to_string_pretty(&rivo_session::FileConfig::default()).unwrap(),
        )
        .expect("write isolated empty schema 3 config");
        environment
    }

    fn config_bytes(environment: &TestEnvironment) -> Vec<u8> {
        std::fs::read(environment.config_path()).expect("read isolated config")
    }

    fn remove(launch: Launch, input: &[u8], affirmative: bool, interrupted: bool) {
        let environment = TestEnvironment::new().expect("test environment");
        // This path never discovers models or runs a prompt. The only configured
        // endpoint is loopback, with a fake credential reference and cached model.
        configure_fixture(&environment, "http://127.0.0.1:9/v1");
        let before = config_bytes(&environment);
        let mut session = start(
            &environment,
            &["config", "provider", "remove", "fixture"],
            launch,
        );
        // Matching the complete prompt also proves the stdin/stderr TTY gate
        // passed, rather than mistaking a non-TTY rejection for cancellation.
        expect_screen(&mut session, REMOVE_PROMPT);
        session.send(input);
        let message = if affirmative {
            Some("removed revision=")
        } else if interrupted {
            None // Windows can terminate with STATUS_CONTROL_C_EXIT, without text.
        } else {
            Some("已取消删除 provider fixture")
        };
        expect_exit(session, message, affirmative);
        if affirmative {
            let file: rivo_session::FileConfig =
                toml::from_str(&String::from_utf8(config_bytes(&environment)).unwrap()).unwrap();
            assert!(
                !file.providers.contains_key("fixture"),
                "y must remove provider"
            );
            println!("config evidence: fixture provider removed");
        } else {
            assert_eq!(
                config_bytes(&environment),
                before,
                "cancel must not write config"
            );
            println!("config evidence: byte-identical after cancellation");
        }
    }

    fn form_cancel(launch: Launch, key: &[u8]) {
        let environment = empty_environment();
        let before = config_bytes(&environment);
        let mut session = start(&environment, &["tui"], launch);
        expect_screen(&mut session, FIRST_ROW);
        session.send(key);
        expect_exit(session, Some(CANCEL_FORM), false);
        assert_eq!(config_bytes(&environment), before);
    }

    fn form_characters_tab_submit_no(launch: Launch) {
        let environment = empty_environment();
        let before = config_bytes(&environment);
        let mut session = start(&environment, &["tui"], launch);
        expect_screen(&mut session, FIRST_ROW);
        session.send(b"_typed");
        expect_screen(&mut session, "> Provider ID: default_typed");
        for prompt in [
            BASE_URL_ROW,
            "> Existing Credential name: openai-api-key",
            NO_ROW,
        ] {
            session.send(b"\t");
            expect_screen(&mut session, prompt);
        }
        // Never type yes: this must cancel before any save, discovery or API call.
        session.send(b"\r");
        expect_exit(
            session,
            Some("Save and discovery were not confirmed; configuration was not saved"),
            false,
        );
        assert_eq!(config_bytes(&environment), before);
    }

    fn form_enter_advances(launch: Launch) {
        let environment = empty_environment();
        let before = config_bytes(&environment);
        let mut session = start(&environment, &["tui"], launch);
        expect_screen(&mut session, FIRST_ROW);
        session.send(b"\r");
        // Desired regression contract, not an assertion that today's no-op is OK.
        // The process is already ready: a local focus change has no I/O to wait for.
        expect_screen_with_timeout(&mut session, BASE_URL_ROW, EXIT_TIMEOUT);
        session.send(b"\x1b");
        expect_exit(session, Some(CANCEL_FORM), false);
        assert_eq!(config_bytes(&environment), before);
    }

    fn startup_cancel(launch: Launch, key: &[u8]) {
        let environment = TestEnvironment::new().expect("test environment");
        configure_fixture(&environment, "http://127.0.0.1:9/v1");
        let before = config_bytes(&environment);
        let mut session = start(&environment, &["tui"], launch);
        expect_screen(&mut session, "> 能力模式:");
        expect_screen(&mut session, "Provider 选择:");
        session.send(key);
        expect_exit(session, Some("TUI startup cancelled"), false);
        assert_eq!(config_bytes(&environment), before);
    }

    macro_rules! cases {
        ($($name:ident: $launch:ident => $body:expr;)*) => {$ (
            #[test]
            #[ignore = "explicit Windows ConPTY input regression; no external API"]
            fn $name() {
                ($body)(Launch::$launch);
            }
        )*};
    }

    cases! {
        provider_remove_y_direct: Direct => |mode| remove(mode, b"y\r", true, false);
        provider_remove_y_powershell: PowerShell => |mode| remove(mode, b"y\r", true, false);
        provider_remove_n_direct: Direct => |mode| remove(mode, b"n\r", false, false);
        provider_remove_n_powershell: PowerShell => |mode| remove(mode, b"n\r", false, false);
        provider_remove_enter_direct: Direct => |mode| remove(mode, b"\r", false, false);
        provider_remove_enter_powershell: PowerShell => |mode| remove(mode, b"\r", false, false);
        provider_remove_ctrl_c_direct: Direct => |mode| remove(mode, b"\x03", false, true);
        provider_remove_ctrl_c_powershell: PowerShell => |mode| remove(mode, b"\x03", false, true);
        first_launch_characters_tab_submit_no_direct: Direct => form_characters_tab_submit_no;
        first_launch_characters_tab_submit_no_powershell: PowerShell => form_characters_tab_submit_no;
        first_launch_enter_advances_direct: Direct => form_enter_advances;
        first_launch_enter_advances_powershell: PowerShell => form_enter_advances;
        first_launch_esc_direct: Direct => |mode| form_cancel(mode, b"\x1b");
        first_launch_esc_powershell: PowerShell => |mode| form_cancel(mode, b"\x1b");
        first_launch_ctrl_c_direct: Direct => |mode| form_cancel(mode, b"\x03");
        first_launch_ctrl_c_powershell: PowerShell => |mode| form_cancel(mode, b"\x03");
        startup_ctrl_c_direct: Direct => |mode| startup_cancel(mode, b"\x03");
        startup_ctrl_c_powershell: PowerShell => |mode| startup_cancel(mode, b"\x03");
        startup_esc_direct: Direct => |mode| startup_cancel(mode, b"\x1b");
        startup_esc_powershell: PowerShell => |mode| startup_cancel(mode, b"\x1b");
    }
}

#[test]
fn pty_screen_reconstructs_diffs_and_does_not_match_erased_text() {
    let mut parser = vt100::Parser::new(30, 120, 0);
    parser.process(b"write-approved\rwrite-app\x1b[1Cov\x1b[1Cd");
    assert!(parser.screen().contents().contains("write-approved"));
    parser.process(b"\r\x1b[2K");
    parser.process("已确认路由\r\n需要确认 · Approval".as_bytes());
    assert!(!parser.screen().contents().contains("write-approved"));
    assert!(parser.screen().contents().contains("需要确认 · Approval"));
}

fn write_report(name: &str, passed: bool, history_resumed: bool, details: &str) {
    static REPORT_LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    let _lock = REPORT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("report lock");
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("artifacts")
        .join("test-reports");
    std::fs::create_dir_all(&report_dir).expect("create report directory");
    let platform = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let report = json!({
        "kind": "tui-pty",
        "platform": platform,
        "candidate_commit": commit,
        "case": name,
        "passed": passed,
        "exit_code": if passed { 0 } else { -1 },
        "terminal_restored": passed,
        "history_resumed": history_resumed,
        "single_terminal_event": passed,
        "details": details,
        "sensitive_data": "not recorded"
    });
    let stem = format!("tui-pty-{platform}-{}", &commit[..commit.len().min(12)]);
    let json_path = report_dir.join(format!("{stem}.json"));
    static REPORT_RESET: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    REPORT_RESET.get_or_init(|| {
        let _ = std::fs::remove_file(&json_path);
        let _ = std::fs::remove_file(report_dir.join(format!("{stem}.md")));
    });
    let mut cases = std::fs::read_to_string(&json_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<serde_json::Value>>(&text).ok())
        .unwrap_or_default();
    cases.push(report);
    std::fs::write(
        json_path,
        serde_json::to_vec_pretty(&cases).expect("serialize report"),
    )
    .expect("write report");
    let markdown_path = report_dir.join(format!("{stem}.md"));
    let mut markdown = std::fs::read_to_string(&markdown_path).unwrap_or_else(|_| {
        format!("# TUI PTY ({platform})\n\n- candidate_commit: `{commit}`\n- sensitive_data: not recorded\n")
    });
    if !markdown.contains("- candidate_commit:") || !markdown.contains("## ") {
        markdown = format!("# TUI PTY ({platform})\n\n- candidate_commit: `{commit}`\n- sensitive_data: not recorded\n");
    }
    markdown.push_str(&format!("\n## {name}\n\n- passed: {passed}\n- exit_code: {}\n- terminal_restored: {passed}\n- history_resumed: {history_resumed}\n- single_terminal_event: {passed}\n\nDetails: {details}\n", if passed { 0 } else { -1 }));
    std::fs::write(markdown_path, markdown).expect("write markdown report");
}

fn enter_workspace(tui: &mut PtySession) {
    assert!(
        tui.wait_for("Rivo", Duration::from_secs(10)),
        "TUI startup timed out"
    );
    for _ in 0..rivo_tui::StartupConfigState::FIXED_ROWS {
        tui.send(b"\x1b[B");
    }
    tui.send(b"\r");
    assert!(
        tui.wait_for("Input", Duration::from_secs(10)),
        "TUI workspace startup timed out"
    );
}

fn wait_for_file(path: &Path, expected: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::fs::read_to_string(path).ok().as_deref() == Some(expected) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn enter_coding_workspace(tui: &mut PtySession) {
    assert!(tui.wait_for("Rivo", Duration::from_secs(10)));
    tui.send(b"\r\r\r");
    for _ in 0..rivo_tui::StartupConfigState::FIXED_ROWS {
        tui.send(b"\x1b[B");
    }
    tui.send(b"\r");
    assert!(tui.wait_for("Input", Duration::from_secs(10)));
}

#[test]
#[cfg(windows)]
#[ignore = "optional: requires Windows ConPTY host"]
fn tui_pty_f6_opens_session_settings() {
    let environment = TestEnvironment::new().expect("test environment");
    let server = ScriptedHttpServer::start(vec![HttpResponse::sse(chat_stream("f6-fixture"))])
        .expect("fixture server");
    configure_fixture(&environment, &format!("{}/v1", server.base_url()));
    let mut tui = PtySession::start(&environment);
    enter_workspace(&mut tui);

    // xterm/ConPTY encodes F6 as CSI 17~.
    tui.send(b"\x1b[17~");
    let opened = tui.wait_for("Session Settings", Duration::from_secs(10));
    tui.send(b"\x1b");
    let closed = tui.wait_until_missing("Session Settings", Duration::from_secs(5));
    tui.send(b"q");
    let (status, output, exited) = tui.finish_with_timeout(Duration::from_secs(5));
    let restored = output.contains("\x1b[?1049l") && output.contains("\x1b[?25h");
    let passed = opened && closed && exited && status == 0 && restored;
    write_report(
        "IT-TUI-SETTINGS-F6-001",
        passed,
        false,
        "ConPTY F6 opens Session Settings and terminal restores",
    );
    assert!(
        passed,
        "F6 settings status={status}, opened={opened}, closed={closed}, restored={restored}"
    );
}

fn write_file_tool_stream(path: &str, content: &str) -> String {
    let arguments = serde_json::json!({"path": path, "content": content}).to_string();
    let requested = serde_json::json!({"id":"fixture","object":"chat.completion.chunk","model":"fixture-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-write","type":"function","function":{"name":"write_file","arguments":arguments}}]},"finish_reason":null}]});
    let completed = serde_json::json!({"id":"fixture","object":"chat.completion.chunk","model":"fixture-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
    format!("data: {requested}\n\ndata: {completed}\n\ndata: [DONE]\n\n")
}

#[test]
#[ignore = "optional: requires Windows ConPTY host"]
fn tui_lifecycle_resize_cjk_and_terminal_restore() {
    let environment = TestEnvironment::new().expect("test environment");
    let server = ScriptedHttpServer::start(vec![HttpResponse::sse(chat_stream("中文响应"))])
        .expect("fixture server");
    configure_fixture(&environment, &format!("{}/v1", server.base_url()));
    let mut tui = PtySession::start(&environment);
    enter_workspace(&mut tui);
    tui.send("中文输入测试\r".as_bytes());
    if !tui.wait_for_answer("中文响应") {
        let (status, _output) = tui.terminate();
        write_report("IT-TUI-LIFE-001", false, false, "中文流式响应超时");
        panic!("CJK streaming response timed out (exit code {status})");
    }
    tui.resize(18, 52);
    if !tui.wait_for("Terminal is too small", Duration::from_secs(5)) {
        let (status, _output) = tui.terminate();
        write_report(
            "IT-TUI-LIFE-001",
            false,
            false,
            "narrow resize redraw timed out",
        );
        panic!("narrow resize timed out (exit code {status})");
    }
    tui.resize(30, 120);
    assert!(tui.wait_for("Input", Duration::from_secs(5)));
    // `q` is the idle compatibility shortcut and avoids command-palette
    // parsing differences between terminal hosts.
    tui.send(b"q");
    let (status, output, exited) = tui.finish_with_timeout(Duration::from_secs(5));
    let restored = output.contains("\x1b[?1049l") && output.contains("\x1b[?25h");
    let passed = exited && status == 0 && restored;
    write_report(
        "IT-TUI-LIFE-001",
        passed,
        false,
        "startup, resize, CJK, exit and terminal restore",
    );
    assert!(passed, "status={status}, restored={restored}");
}

#[test]
#[ignore = "optional: requires Windows ConPTY host"]
fn tui_pty_provider_failure_restores_terminal() {
    let environment = TestEnvironment::new().expect("test environment");
    let server = ScriptedHttpServer::start(vec![HttpResponse {
        status: 500,
        content_type: "application/json".into(),
        body: "{\"error\":\"fixture failure\"}".into(),
    }])
    .expect("fixture server");
    configure_fixture(&environment, &format!("{}/v1", server.base_url()));
    let mut tui = PtySession::start(&environment);
    enter_workspace(&mut tui);
    tui.send("触发失败\r".as_bytes());
    let failed = tui.wait_for("提供方失败", Duration::from_secs(10))
        && tui.wait_for("● Complete", Duration::from_secs(10));
    let screen = tui.text();
    tui.send(b"q");
    let (status, output, exited) = tui.finish_with_timeout(Duration::from_secs(5));
    let restored = output.contains("\x1b[?1049l") && output.contains("\x1b[?25h");
    let passed = failed && exited && status == 0 && restored;
    write_report(
        "IT-TUI-RESTORE-001",
        passed,
        false,
        "provider failure and terminal restore",
    );
    assert!(
        passed,
        "provider failure restore status={status}, restored={restored}, failed={failed}\n{screen}"
    );
}

#[test]
#[ignore = "optional: requires Windows ConPTY host"]
fn tui_pty_cancel_restores_terminal() {
    let environment = TestEnvironment::new().expect("test environment");
    let server = ScriptedHttpServer::start(vec![HttpResponse::sse(write_file_tool_stream(
        "cancelled.txt",
        "must not be written",
    ))])
    .expect("fixture server");
    configure_fixture(&environment, &format!("{}/v1", server.base_url()));
    let mut tui = PtySession::start(&environment);
    enter_coding_workspace(&mut tui);
    tui.send("取消测试\r".as_bytes());
    // A pending approval is an observable active turn, unlike a race with an
    // immediately completed HTTP response or a key sent before Started.
    let active = tui.wait_for("Approval required", Duration::from_secs(10));
    tui.send(b"\x03");
    let cancelled = tui.wait_for("用户取消等待确认的 turn", Duration::from_secs(10))
        && tui.wait_for("● Complete", Duration::from_secs(10));
    let screen = tui.text();
    tui.send(b"q");
    let (status, output, exited) = tui.finish_with_timeout(Duration::from_secs(5));
    let restored = output.contains("\x1b[?1049l") && output.contains("\x1b[?25h");
    let not_written = !environment.workspace().join("cancelled.txt").exists();
    let passed = active && cancelled && not_written && exited && status == 0 && restored;
    write_report(
        "IT-TUI-LIFE-001-CANCEL",
        passed,
        false,
        "active cancellation and terminal restore",
    );
    assert!(
        passed,
        "cancel status={status}, restored={restored}, active={active}, cancelled={cancelled}, not_written={not_written}\n{screen}"
    );
}

#[test]
#[ignore = "optional: requires Windows ConPTY host"]
fn tui_pty_confirmation_deny_restores_terminal() {
    let environment = TestEnvironment::new().expect("test environment");
    // A user refusal is returned to the model as a bounded `policy_denied`
    // tool result, so the fixture must provide the model's follow-up answer.
    let server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(write_file_tool_stream("denied.txt", "no")),
        HttpResponse::sse(chat_stream("拒绝已处理")),
    ])
    .expect("fixture server");
    configure_fixture(&environment, &format!("{}/v1", server.base_url()));
    let mut tui = PtySession::start(&environment);
    enter_coding_workspace(&mut tui);
    tui.send("写入文件\r".as_bytes());
    // schema 3 also renders “已确认路由”; only the approval overlay is actionable.
    let requested = tui.wait_for("Approval required", Duration::from_secs(10));
    tui.send(b"n");
    let denied = tui.wait_for("拒绝已处理", Duration::from_secs(10))
        && tui.wait_for("● Complete", Duration::from_secs(10));
    let screen = tui.text();
    tui.send(b"q");
    let (status, output, exited) = tui.finish_with_timeout(Duration::from_secs(5));
    let restored = output.contains("\x1b[?1049l") && output.contains("\x1b[?25h");
    let not_written = !environment.workspace().join("denied.txt").exists();
    let passed = requested && denied && not_written && exited && status == 0 && restored;
    write_report(
        "IT-TUI-CONFIRM-001-DENY",
        passed,
        false,
        "confirmation deny and terminal restore",
    );
    assert!(
        passed,
        "confirmation deny status={status}, restored={restored}, requested={requested}, denied={denied}, not_written={not_written}\n{screen}"
    );
}

#[test]
#[ignore = "optional: requires Windows ConPTY host"]
fn tui_pty_confirmation_allow_restores_terminal() {
    let environment = TestEnvironment::new().expect("test environment");
    let target = environment.workspace().join("approved.txt");
    let server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(write_file_tool_stream("approved.txt", "yes")),
        HttpResponse::sse(write_file_tool_stream("approved.txt", "yes")),
        HttpResponse::sse(chat_stream("write-approved")),
    ])
    .expect("fixture server");
    configure_fixture(&environment, &format!("{}/v1", server.base_url()));
    let mut tui = PtySession::start(&environment);
    enter_coding_workspace(&mut tui);
    tui.send("写入文件\r".as_bytes());
    let requested = tui.wait_for("Approval required", Duration::from_secs(10));
    tui.send(b"y");
    let completed = wait_for_file(&target, "yes", Duration::from_secs(10));
    let answered = tui.wait_for_answer("write-approved");
    let screen = tui.text();
    tui.send(b"q");
    let (status, output, exited) = tui.finish_with_timeout(Duration::from_secs(5));
    let restored = output.contains("\x1b[?1049l") && output.contains("\x1b[?25h");
    let written = std::fs::read_to_string(target).ok().as_deref() == Some("yes");
    let passed = requested && completed && written && answered && exited && status == 0 && restored;
    write_report(
        "IT-TUI-CONFIRM-001-ALLOW",
        passed,
        false,
        "confirmation allow and terminal restore",
    );
    assert!(
        passed,
        "confirmation allow status={status}, restored={restored}, requested={requested}, completed={completed}, written={written}, answered={answered}\n{screen}"
    );
}

#[test]
#[cfg(windows)]
#[ignore = "optional: requires Windows ConPTY host"]
fn tui_pty_history_resume_preserves_session_and_context() {
    let environment = TestEnvironment::new().expect("test environment");
    let server = ScriptedHttpServer::start(vec![
        HttpResponse::sse(chat_stream("first-history-answer")),
        HttpResponse::sse(chat_stream("second-history-answer")),
    ])
    .expect("fixture server");
    configure_fixture(&environment, &format!("{}/v1", server.base_url()));

    let mut first = PtySession::start(&environment);
    enter_workspace(&mut first);
    first.send("first history turn\r".as_bytes());
    assert!(
        first.wait_for_answer("first-history-answer"),
        "first history turn timed out\n{}",
        first.text()
    );
    first.send(b"q");
    let (first_status, _, first_exited) = first.finish_with_timeout(Duration::from_secs(5));
    assert!(
        first_exited && first_status == 0,
        "first TUI did not exit cleanly: status={first_status}"
    );

    let listed = environment
        .command(binary())
        .args(["sessions", "list"])
        .run()
        .expect("list sessions");
    listed.assert_success();
    let first_session_id = listed
        .stdout
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .filter(|value| !value.is_empty())
        .expect("first session id")
        .to_owned();

    let mut second = PtySession::start(&environment);
    enter_workspace(&mut second);
    second.send(b"/sessions\r");
    assert!(
        second.wait_for("[history]", Duration::from_secs(10)),
        "history drawer did not open\n{}",
        second.text()
    );
    // The newly created session is the first entry; move to the prior session.
    second.send(b"\x1b[B\r");
    let history_resumed = second.wait_for(&first_session_id, Duration::from_secs(10))
        && second.wait_for("first history turn", Duration::from_secs(10));
    assert!(
        history_resumed,
        "history session was not restored\n{}",
        second.text()
    );
    second.send("second history turn\r".as_bytes());
    let answered = second.wait_for_answer("second-history-answer");
    second.send(b"q");
    let (second_status, output, second_exited) = second.finish_with_timeout(Duration::from_secs(5));
    let restored = output.contains("\x1b[?1049l") && output.contains("\x1b[?25h");
    let passed = answered && second_exited && second_status == 0 && restored;
    assert!(
        passed,
        "history resume status={second_status}, answered={answered}, restored={restored}"
    );

    let show = environment
        .command(binary())
        .args(["sessions", "show", &first_session_id])
        .run()
        .expect("show resumed session");
    show.assert_success();
    assert!(show.stdout.contains("first history turn"));
    assert!(show.stdout.contains("second history turn"));
    assert_redacted(&show.stdout, &["fixture-key"]);
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    let resumed: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let messages = resumed["messages"].as_array().expect("resumed messages");
    let context_preserved = [
        ("user", "first history turn"),
        ("assistant", "first-history-answer"),
        ("user", "second history turn"),
    ]
    .iter()
    .all(|(role, content)| {
        messages.iter().any(|message| {
            message["role"] == *role
                && message["content"].as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|part| part["type"] == "text" && part["text"] == *content)
                })
        })
    });
    write_report(
        "IT-TUI-HISTORY-001",
        passed && context_preserved,
        history_resumed,
        "restart, history selection, same-session second turn, request context and terminal restore",
    );
    assert!(context_preserved, "resumed request lost prior turn context");
}
