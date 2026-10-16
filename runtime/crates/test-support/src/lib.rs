use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CLI_PROCESS_TIMEOUT: Duration = Duration::from_secs(120);

/// 为 CLI subprocess 隔离配置、数据、日志和 workspace。
pub struct TestEnvironment {
    root: tempfile::TempDir,
    config_path: PathBuf,
    data_dir: PathBuf,
    workspace: PathBuf,
}

impl TestEnvironment {
    pub fn new() -> std::io::Result<Self> {
        let root = tempfile::tempdir()?;
        let config_path = root.path().join("config").join("config.toml");
        let data_dir = root.path().join("data");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(config_path.parent().expect("config has parent"))?;
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&workspace)?;
        std::fs::write(&config_path, "version = 6\n")?;
        Ok(Self {
            root,
            config_path,
            data_dir,
            workspace,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn command(&self, binary: impl AsRef<Path>) -> CliCommand {
        let mut environment = BTreeMap::new();
        let root = self.root().as_os_str().to_owned();
        let data = self.data_dir().as_os_str().to_owned();
        environment.insert(
            "RIVO_CONFIG".into(),
            self.config_path.as_os_str().to_owned(),
        );
        environment.insert("RIVO_DATA_DIR".into(), data.clone());
        environment.insert("HOME".into(), root.clone());
        environment.insert("USERPROFILE".into(), root.clone());
        environment.insert("XDG_CONFIG_HOME".into(), root.clone());
        environment.insert("XDG_DATA_HOME".into(), data.clone());
        environment.insert("LOCALAPPDATA".into(), data.clone());
        environment.insert("APPDATA".into(), data);
        CliCommand {
            binary: binary.as_ref().to_path_buf(),
            args: Vec::new(),
            environment,
            removed_environment: default_removed_environment(),
            timeout: CLI_PROCESS_TIMEOUT,
        }
    }
}

fn default_removed_environment() -> Vec<String> {
    [
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
        "RIVO_WORKSPACE",
        "RIVO_DATA_DIR",
        "RIVO_LANGUAGE_DIR",
        "RIVO_LIVE_TEST",
        "RIVO_LIVE_MODEL",
        "RIVO_LIVE_API_KEY",
        "RIVO_LIVE_BASE_URL",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub struct CliCommand {
    binary: PathBuf,
    args: Vec<String>,
    environment: BTreeMap<String, std::ffi::OsString>,
    removed_environment: Vec<String>,
    timeout: Duration,
}

impl CliCommand {
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<std::ffi::OsString>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn run(self) -> std::io::Result<CliOutput> {
        let mut command = Command::new(&self.binary);
        command.args(&self.args);
        for key in &self.removed_environment {
            command.env_remove(key);
        }
        command.envs(&self.environment);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let started = Instant::now();
        let mut child = command.spawn()?;
        let mut timed_out = false;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if started.elapsed() >= self.timeout {
                timed_out = true;
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output()?;
        Ok(CliOutput::new(output, started.elapsed(), timed_out))
    }
}

#[derive(Debug)]
pub struct CliOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

impl CliOutput {
    fn new(output: Output, duration: Duration, timed_out: bool) -> Self {
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if timed_out {
            stderr.push_str("\n[test-support] CLI process timed out and was terminated\n");
        }
        Self {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr,
            duration,
        }
    }

    pub fn assert_success(&self) {
        assert_eq!(
            self.status, 0,
            "CLI failed\nstdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
    }

    pub fn assert_failure(&self) {
        assert_ne!(
            self.status, 0,
            "CLI unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

impl HttpResponse {
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            body: body.into(),
        }
    }

    pub fn sse(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".into(),
            body: body.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedRequest {
    pub request_line: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// 按顺序返回预设 HTTP 响应，并保留收到的请求用于断言。
pub struct ScriptedHttpServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    worker: Option<JoinHandle<()>>,
}

impl ScriptedHttpServer {
    pub fn start(responses: Vec<HttpResponse>) -> std::io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::channel();
        for response in responses {
            let _ = sender.send(response);
        }
        drop(sender);
        Self::start_channel(receiver)
    }

    /// 与 `start` 相同，但响应可在服务器启动后按需追加；发送端关闭后结束。
    /// 用于响应内容依赖运行时才知道的值（例如会话 ID）的测试。
    pub fn start_channel(
        responses: std::sync::mpsc::Receiver<HttpResponse>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = std::thread::spawn(move || {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut socket).unwrap_or_else(|error| CapturedRequest {
                    request_line: format!("fixture read error: {error}"),
                    headers: BTreeMap::new(),
                    body: String::new(),
                });
                captured
                    .lock()
                    .expect("request lock poisoned")
                    .push(request);
                let reason = status_reason(response.status);
                let wire = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    reason,
                    response.content_type,
                    response.body.len(),
                    response.body
                );
                let _ = socket.write_all(wire.as_bytes());
                let _ = socket.flush();
            }
        });
        Ok(Self {
            address,
            requests,
            worker: Some(worker),
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("request lock poisoned").clone()
    }

    pub fn finish(mut self) -> Vec<CapturedRequest> {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("fixture server panicked");
        }
        self.requests()
    }
}

impl Drop for ScriptedHttpServer {
    fn drop(&mut self) {
        if self.worker.is_some() {
            // 测试失败时不阻塞等待尚未发生的请求。
            self.worker.take();
        }
    }
}

fn read_request(stream: &mut impl Read) -> std::io::Result<CapturedRequest> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let size = stream.read(&mut buffer)?;
        if size == 0 {
            break bytes.len();
        }
        bytes.extend_from_slice(&buffer[..size]);
        if let Some(index) = find_bytes(&bytes, b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 2 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "fixture request headers too large",
            ));
        }
    };
    let header_text = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or_default().trim().to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    while bytes.len().saturating_sub(header_end) < content_length {
        let size = stream.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..size]);
    }
    let body_end = header_end.saturating_add(content_length).min(bytes.len());
    Ok(CapturedRequest {
        request_line,
        headers,
        body: String::from_utf8_lossy(&bytes[header_end..body_end]).into_owned(),
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Fixture",
    }
}

pub fn assert_redacted(text: &str, secrets: &[&str]) {
    for secret in secrets {
        assert!(
            !text.contains(secret),
            "secret was present in output: {secret}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_creates_isolated_paths() {
        let environment = TestEnvironment::new().unwrap();
        assert!(environment.workspace().is_dir());
        assert!(environment.data_dir().is_dir());
        assert!(environment.config_path().is_file());
    }

    #[test]
    fn cli_command_uses_the_120_second_default_timeout() {
        let environment = TestEnvironment::new().unwrap();
        assert_eq!(environment.command("rivo").timeout, CLI_PROCESS_TIMEOUT);
        assert_eq!(CLI_PROCESS_TIMEOUT, Duration::from_secs(120));
    }

    #[test]
    fn redaction_assertion_accepts_clean_text() {
        assert_redacted("token=[REDACTED]", &["super-secret"]);
    }
}
