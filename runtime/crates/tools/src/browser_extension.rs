//! Chrome/Edge Manifest V3 extension transport.
//!
//! The extension backend deliberately has no browser-specific policy logic.  It
//! transports a bounded, versioned request to the extension and returns the
//! structured result.  The agent/policy layer remains responsible for deciding
//! whether a request may run.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{timeout, Duration};

pub const PROTOCOL: &str = "rivo-browser-extension-v1";
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_REQUEST_DEADLINE_MS: u64 = 35_000;

#[derive(Debug, Error)]
pub enum ExtensionBrowserError {
    #[error("扩展协议错误: {0}")]
    Protocol(String),
    #[error("扩展连接不可用: {0}")]
    Connection(String),
    #[error("扩展实例不存在: {0}")]
    MissingInstance(String),
    #[error("扩展请求超时")]
    Timeout,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BrowserInstanceRef {
    pub browser_family: String,
    pub extension_id: String,
    pub installation_id: String,
    pub profile_key: String,
    pub boot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionHello {
    pub protocol: String,
    pub browser_family: String,
    pub extension_id: String,
    pub installation_id: String,
    pub profile_key: String,
    pub boot_id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl ExtensionHello {
    pub fn instance(&self) -> BrowserInstanceRef {
        BrowserInstanceRef {
            browser_family: self.browser_family.clone(),
            extension_id: self.extension_id.clone(),
            installation_id: self.installation_id.clone(),
            profile_key: self.profile_key.clone(),
            boot_id: self.boot_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionRequest {
    pub protocol: String,
    pub id: String,
    pub method: String,
    pub browser_instance: BrowserInstanceRef,
    #[serde(default)]
    pub params: Value,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionResponse {
    pub protocol: String,
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub browser_instance: Option<BrowserInstanceRef>,
}

/// Native Messaging framing: a little-endian u32 length followed by UTF-8 JSON.
pub fn encode_native_message(value: &impl Serialize) -> Result<Vec<u8>, ExtensionBrowserError> {
    let body =
        serde_json::to_vec(value).map_err(|e| ExtensionBrowserError::Protocol(e.to_string()))?;
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(ExtensionBrowserError::Protocol(
            "消息超过 1 MiB 限制".into(),
        ));
    }
    let len = u32::try_from(body.len())
        .map_err(|_| ExtensionBrowserError::Protocol("消息长度无效".into()))?;
    let mut framed = Vec::with_capacity(body.len() + 4);
    framed.extend_from_slice(&len.to_le_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

pub fn decode_native_message<T: for<'de> Deserialize<'de>>(
    frame: &[u8],
) -> Result<T, ExtensionBrowserError> {
    if frame.len() < 4 {
        return Err(ExtensionBrowserError::Protocol(
            "Native Messaging 帧不完整".into(),
        ));
    }
    let length = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
    if length > MAX_MESSAGE_BYTES || frame.len() != length + 4 {
        return Err(ExtensionBrowserError::Protocol(
            "Native Messaging 帧长度无效".into(),
        ));
    }
    serde_json::from_slice(&frame[4..]).map_err(|e| ExtensionBrowserError::Protocol(e.to_string()))
}

#[async_trait]
pub trait ExtensionTransport: Send + Sync {
    async fn request(
        &self,
        request: ExtensionRequest,
    ) -> Result<ExtensionResponse, ExtensionBrowserError>;
}

/// Sequential Native Messaging transport. Native Messaging hosts are launched
/// per browser by Chrome/Edge, so a transport instance is intentionally bound
/// to one stdin/stdout pair and cannot accidentally multiplex profiles.
pub struct NativeMessagingTransport<R, W> {
    reader: AsyncMutex<R>,
    writer: AsyncMutex<W>,
    exchange: AsyncMutex<()>,
    broken: AtomicBool,
}

impl<R, W> NativeMessagingTransport<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: AsyncMutex::new(reader),
            writer: AsyncMutex::new(writer),
            exchange: AsyncMutex::new(()),
            broken: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl<R, W> ExtensionTransport for NativeMessagingTransport<R, W>
where
    R: AsyncRead + Unpin + Send + Sync,
    W: AsyncWrite + Unpin + Send + Sync,
{
    async fn request(
        &self,
        request: ExtensionRequest,
    ) -> Result<ExtensionResponse, ExtensionBrowserError> {
        if self.broken.load(Ordering::Acquire) {
            return Err(ExtensionBrowserError::Connection(
                "扩展连接已失效，需重新连接".into(),
            ));
        }
        let deadline_ms = request.deadline_ms;
        if deadline_ms == 0 || deadline_ms > MAX_REQUEST_DEADLINE_MS {
            return Err(ExtensionBrowserError::Protocol(
                "请求 deadline 超出允许范围".into(),
            ));
        }

        // Native Messaging has no multiplexing channel. Bound the whole
        // serialized exchange; a timeout poisons this stream so a partial
        // frame can never be mistaken for the next request's response.
        let result = timeout(Duration::from_millis(deadline_ms), async {
            let _exchange = self.exchange.lock().await;
            let frame = encode_native_message(&request)?;
            let mut writer = self.writer.lock().await;
            writer
                .write_all(&frame)
                .await
                .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
            writer
                .flush()
                .await
                .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
            drop(writer);

            let mut reader = self.reader.lock().await;
            let mut header = [0u8; 4];
            reader
                .read_exact(&mut header)
                .await
                .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
            let length = u32::from_le_bytes(header) as usize;
            if length > MAX_MESSAGE_BYTES {
                return Err(ExtensionBrowserError::Protocol(
                    "响应超过 1 MiB 限制".into(),
                ));
            }
            let mut body = vec![0u8; length];
            reader
                .read_exact(&mut body)
                .await
                .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
            let mut full = header.to_vec();
            full.extend_from_slice(&body);
            decode_native_message(&full)
        })
        .await;

        match result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => {
                self.broken.store(true, Ordering::Release);
                Err(error)
            }
            Err(_) => {
                self.broken.store(true, Ordering::Release);
                Err(ExtensionBrowserError::Timeout)
            }
        }
    }
}

#[derive(Clone)]
pub struct ExtensionBrowserClient {
    transport: Arc<dyn ExtensionTransport>,
    instance: BrowserInstanceRef,
}

impl ExtensionBrowserClient {
    pub fn new(transport: Arc<dyn ExtensionTransport>, instance: BrowserInstanceRef) -> Self {
        Self {
            transport,
            instance,
        }
    }

    pub fn instance(&self) -> &BrowserInstanceRef {
        &self.instance
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ExtensionBrowserError> {
        let deadline_ms = request_deadline_ms(method, &params);
        let request = ExtensionRequest {
            protocol: PROTOCOL.into(),
            id: format!("rivo-{}", uuid_like_id()),
            method: method.into(),
            browser_instance: self.instance.clone(),
            params,
            deadline_ms,
        };
        let request_id = request.id.clone();
        let response = self.transport.request(request).await?;
        if response.protocol != PROTOCOL {
            return Err(ExtensionBrowserError::Protocol("协议版本不匹配".into()));
        }
        if response.id != request_id {
            return Err(ExtensionBrowserError::Protocol("扩展响应 ID 不匹配".into()));
        }
        if response.browser_instance.as_ref() != Some(&self.instance) {
            return Err(ExtensionBrowserError::Protocol("扩展实例身份不匹配".into()));
        }
        if response.ok {
            Ok(response.result)
        } else {
            Err(ExtensionBrowserError::Protocol(
                response.error.unwrap_or_else(|| "扩展请求失败".into()),
            ))
        }
    }

    /// Connects a Rivo runtime to the host's per-instance Windows named pipe.
    #[cfg(windows)]
    pub async fn connect_named_pipe(
        pipe_name: &str,
        expected: BrowserInstanceRef,
    ) -> Result<Self, ExtensionBrowserError> {
        let started = std::time::Instant::now();
        loop {
            let attempt = async {
                let pipe = tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(pipe_name)
                    .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
                let (mut reader, writer) = tokio::io::split(pipe);
                let mut header = [0u8; 4];
                reader
                    .read_exact(&mut header)
                    .await
                    .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
                let length = u32::from_le_bytes(header) as usize;
                if length > MAX_MESSAGE_BYTES {
                    return Err(ExtensionBrowserError::Protocol("扩展握手超过 1 MiB".into()));
                }
                let mut body = vec![0u8; length];
                reader
                    .read_exact(&mut body)
                    .await
                    .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
                let mut frame = header.to_vec();
                frame.extend_from_slice(&body);
                let hello: ExtensionHello = decode_native_message(&frame)?;
                let instance = hello.instance();
                if !instance_matches(&expected, &instance) {
                    return Err(ExtensionBrowserError::Protocol("扩展实例身份不匹配".into()));
                }
                Ok(Self::new(
                    Arc::new(NativeMessagingTransport::new(reader, writer)),
                    instance,
                ))
            }
            .await;
            match attempt {
                Ok(client) => return Ok(client),
                Err(error) => {
                    let retryable = matches!(&error, ExtensionBrowserError::Connection(_))
                        || matches!(
                            &error,
                            ExtensionBrowserError::Protocol(message)
                                if message == "扩展实例身份不匹配"
                        );
                    if !retryable || started.elapsed() >= std::time::Duration::from_secs(5) {
                        return Err(error);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[cfg(not(windows))]
    pub async fn connect_named_pipe(
        _pipe_name: &str,
        _instance: BrowserInstanceRef,
    ) -> Result<Self, ExtensionBrowserError> {
        Err(ExtensionBrowserError::Connection(
            "Native Messaging Host 仅支持 Windows 11 本地预览".into(),
        ))
    }
}

fn request_deadline_ms(method: &str, params: &Value) -> u64 {
    if method == "browser_wait" {
        params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(1_000)
            .clamp(50, 30_000)
            .saturating_add(5_000)
    } else {
        20_000
    }
}

#[cfg(windows)]
fn instance_matches(expected: &BrowserInstanceRef, actual: &BrowserInstanceRef) -> bool {
    (expected.browser_family.is_empty() || actual.browser_family == expected.browser_family)
        && (expected.extension_id.is_empty() || actual.extension_id == expected.extension_id)
        && (expected.installation_id.is_empty()
            || actual.installation_id == expected.installation_id)
        && (expected.profile_key.is_empty() || actual.profile_key == expected.profile_key)
        && (expected.boot_id.is_empty() || actual.boot_id == expected.boot_id)
}

fn uuid_like_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

#[derive(Default)]
pub struct BrowserInstanceRegistry {
    instances: Mutex<Vec<ExtensionHello>>,
}

impl BrowserInstanceRegistry {
    pub fn register(
        &self,
        hello: ExtensionHello,
    ) -> Result<BrowserInstanceRef, ExtensionBrowserError> {
        if hello.protocol != PROTOCOL
            || hello.browser_family != "chrome" && hello.browser_family != "edge"
        {
            return Err(ExtensionBrowserError::Protocol("无效的扩展握手".into()));
        }
        let instance = hello.instance();
        let mut instances = self
            .instances
            .lock()
            .map_err(|_| ExtensionBrowserError::Connection("实例锁损坏".into()))?;
        instances.retain(|item| item.instance() != instance);
        instances.push(hello);
        Ok(instance)
    }

    pub fn list(&self) -> Result<Vec<BrowserInstanceRef>, ExtensionBrowserError> {
        Ok(self
            .instances
            .lock()
            .map_err(|_| ExtensionBrowserError::Connection("实例锁损坏".into()))?
            .iter()
            .map(ExtensionHello::instance)
            .collect())
    }

    pub fn remove_boot(&self, boot_id: &str) -> Result<(), ExtensionBrowserError> {
        self.instances
            .lock()
            .map_err(|_| ExtensionBrowserError::Connection("实例锁损坏".into()))?
            .retain(|item| item.boot_id != boot_id);
        Ok(())
    }

    pub fn contains(&self, instance: &BrowserInstanceRef) -> Result<bool, ExtensionBrowserError> {
        Ok(self
            .instances
            .lock()
            .map_err(|_| ExtensionBrowserError::Connection("实例锁损坏".into()))?
            .iter()
            .any(|item| &item.instance() == instance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_message_round_trip_and_limits() {
        let value = serde_json::json!({"ok": true});
        let frame = encode_native_message(&value).unwrap();
        let decoded: Value = decode_native_message(&frame).unwrap();
        assert_eq!(decoded, value);
        assert!(decode_native_message::<Value>(&[1, 2, 3]).is_err());
        let oversized = serde_json::json!("x".repeat(MAX_MESSAGE_BYTES));
        assert!(encode_native_message(&oversized).is_err());
        let mut oversized_header = ((MAX_MESSAGE_BYTES + 1) as u32).to_le_bytes().to_vec();
        oversized_header.extend_from_slice(b"{}");
        assert!(decode_native_message::<Value>(&oversized_header).is_err());
    }

    #[test]
    fn registry_keeps_chrome_and_edge_separate() {
        let registry = BrowserInstanceRegistry::default();
        for family in ["chrome", "edge"] {
            registry
                .register(ExtensionHello {
                    protocol: PROTOCOL.into(),
                    browser_family: family.into(),
                    extension_id: format!("{family}-id"),
                    installation_id: "install".into(),
                    profile_key: "work".into(),
                    boot_id: family.into(),
                    capabilities: vec![],
                })
                .unwrap();
        }
        assert_eq!(registry.list().unwrap().len(), 2);
    }

    #[test]
    fn registry_rejects_unknown_family_and_tracks_boot_identity() {
        let registry = BrowserInstanceRegistry::default();
        let hello = ExtensionHello {
            protocol: PROTOCOL.into(),
            browser_family: "firefox".into(),
            extension_id: "id".into(),
            installation_id: "install".into(),
            profile_key: "work".into(),
            boot_id: "boot".into(),
            capabilities: vec![],
        };
        assert!(registry.register(hello).is_err());
        let hello = ExtensionHello {
            browser_family: "chrome".into(),
            protocol: PROTOCOL.into(),
            extension_id: "id".into(),
            installation_id: "install".into(),
            profile_key: "work".into(),
            boot_id: "boot".into(),
            capabilities: vec![],
        };
        let instance = registry.register(hello).unwrap();
        assert!(registry.contains(&instance).unwrap());
        registry.remove_boot("boot").unwrap();
        assert!(!registry.contains(&instance).unwrap());
    }

    #[tokio::test]
    async fn native_transport_binds_response_to_request_instance() {
        let instance = BrowserInstanceRef {
            browser_family: "chrome".into(),
            extension_id: "extension".into(),
            installation_id: "install".into(),
            profile_key: "work".into(),
            boot_id: "boot".into(),
        };
        let (left, right) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(left);
        let (mut peer_reader, mut peer_writer) = tokio::io::split(right);
        let expected = instance.clone();
        let task = tokio::spawn(async move {
            let frame = read_test_frame(&mut peer_reader).await;
            let request: ExtensionRequest = decode_native_message(&frame).unwrap();
            assert_eq!(request.browser_instance, expected);
            assert_eq!(request.deadline_ms, 20_000);
            let response = ExtensionResponse {
                protocol: PROTOCOL.into(),
                id: request.id,
                ok: true,
                result: serde_json::json!({"ok": true}),
                error: None,
                browser_instance: Some(request.browser_instance),
            };
            peer_writer
                .write_all(&encode_native_message(&response).unwrap())
                .await
                .unwrap();
        });
        let client = ExtensionBrowserClient::new(
            Arc::new(NativeMessagingTransport::new(reader, writer)),
            instance,
        );
        assert_eq!(
            client.call("browser_snapshot", Value::Null).await.unwrap(),
            serde_json::json!({"ok": true})
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn client_rejects_stale_response_id() {
        let instance = BrowserInstanceRef {
            browser_family: "chrome".into(),
            extension_id: "extension".into(),
            installation_id: "install".into(),
            profile_key: "work".into(),
            boot_id: "boot".into(),
        };
        let (left, right) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(left);
        let (mut peer_reader, mut peer_writer) = tokio::io::split(right);
        let task = tokio::spawn(async move {
            let frame = read_test_frame(&mut peer_reader).await;
            let request: ExtensionRequest = decode_native_message(&frame).unwrap();
            let response = ExtensionResponse {
                protocol: PROTOCOL.into(),
                id: "stale-response".into(),
                ok: true,
                result: serde_json::json!({"ok": true}),
                error: None,
                browser_instance: Some(request.browser_instance),
            };
            peer_writer
                .write_all(&encode_native_message(&response).unwrap())
                .await
                .unwrap();
        });
        let client = ExtensionBrowserClient::new(
            Arc::new(NativeMessagingTransport::new(reader, writer)),
            instance,
        );
        assert!(client.call("browser_snapshot", Value::Null).await.is_err());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn client_rejects_mismatched_protocol_and_old_boot_identity() {
        for mismatch in ["protocol", "boot_id"] {
            let instance = BrowserInstanceRef {
                browser_family: "chrome".into(),
                extension_id: "extension".into(),
                installation_id: "install".into(),
                profile_key: "work".into(),
                boot_id: "current-boot".into(),
            };
            let (left, right) = tokio::io::duplex(4096);
            let (reader, writer) = tokio::io::split(left);
            let (mut peer_reader, mut peer_writer) = tokio::io::split(right);
            let mismatch = mismatch.to_owned();
            let task = tokio::spawn(async move {
                let frame = read_test_frame(&mut peer_reader).await;
                let request: ExtensionRequest = decode_native_message(&frame).unwrap();
                let mut response_instance = request.browser_instance.clone();
                let protocol = if mismatch == "protocol" {
                    "rivo-browser-extension-v0".into()
                } else {
                    PROTOCOL.into()
                };
                if mismatch == "boot_id" {
                    response_instance.boot_id = "old-boot".into();
                }
                let response = ExtensionResponse {
                    protocol,
                    id: request.id,
                    ok: true,
                    result: serde_json::json!({"ok": true}),
                    error: None,
                    browser_instance: Some(response_instance),
                };
                peer_writer
                    .write_all(&encode_native_message(&response).unwrap())
                    .await
                    .unwrap();
            });
            let client = ExtensionBrowserClient::new(
                Arc::new(NativeMessagingTransport::new(reader, writer)),
                instance,
            );
            assert!(matches!(
                client.call("browser_snapshot", Value::Null).await,
                Err(ExtensionBrowserError::Protocol(_))
            ));
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn native_transport_rejects_oversized_response_before_reading_body() {
        let (left, right) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(left);
        let (mut peer_reader, mut peer_writer) = tokio::io::split(right);
        let task = tokio::spawn(async move {
            let _ = read_test_frame(&mut peer_reader).await;
            peer_writer
                .write_all(&((MAX_MESSAGE_BYTES + 1) as u32).to_le_bytes())
                .await
                .unwrap();
        });
        let client = ExtensionBrowserClient::new(
            Arc::new(NativeMessagingTransport::new(reader, writer)),
            BrowserInstanceRef {
                browser_family: "chrome".into(),
                extension_id: "extension".into(),
                installation_id: "install".into(),
                profile_key: "work".into(),
                boot_id: "boot".into(),
            },
        );
        assert!(matches!(
            client.call("browser_snapshot", Value::Null).await,
            Err(ExtensionBrowserError::Protocol(message)) if message.contains("超过 1 MiB")
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn wait_deadline_includes_bounded_wait_and_response_margin() {
        let instance = BrowserInstanceRef {
            browser_family: "chrome".into(),
            extension_id: "extension".into(),
            installation_id: "install".into(),
            profile_key: "work".into(),
            boot_id: "boot".into(),
        };
        let (left, right) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(left);
        let (mut peer_reader, mut peer_writer) = tokio::io::split(right);
        let task = tokio::spawn(async move {
            let frame = read_test_frame(&mut peer_reader).await;
            let request: ExtensionRequest = decode_native_message(&frame).unwrap();
            assert_eq!(request.deadline_ms, 5_100);
            let response = ExtensionResponse {
                protocol: PROTOCOL.into(),
                id: request.id,
                ok: true,
                result: serde_json::json!({"waited_ms": 100}),
                error: None,
                browser_instance: Some(request.browser_instance),
            };
            peer_writer
                .write_all(&encode_native_message(&response).unwrap())
                .await
                .unwrap();
        });
        let client = ExtensionBrowserClient::new(
            Arc::new(NativeMessagingTransport::new(reader, writer)),
            instance,
        );
        assert_eq!(
            client
                .call("browser_wait", serde_json::json!({"timeout_ms": 100}))
                .await
                .unwrap(),
            serde_json::json!({"waited_ms": 100})
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn native_transport_returns_deadline_timeout_and_poisons_partial_stream() {
        let (left, right) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(left);
        let (mut peer_reader, peer_writer) = tokio::io::split(right);
        let task = tokio::spawn(async move {
            let _keep_open = peer_writer;
            let _ = read_test_frame(&mut peer_reader).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let transport = NativeMessagingTransport::new(reader, writer);
        let request = ExtensionRequest {
            protocol: PROTOCOL.into(),
            id: "deadline-request".into(),
            method: "browser_snapshot".into(),
            browser_instance: BrowserInstanceRef {
                browser_family: "chrome".into(),
                extension_id: "extension".into(),
                installation_id: "install".into(),
                profile_key: "work".into(),
                boot_id: "boot".into(),
            },
            params: Value::Null,
            deadline_ms: 20,
        };
        assert!(matches!(
            transport.request(request).await,
            Err(ExtensionBrowserError::Timeout)
        ));
        let next = ExtensionRequest {
            protocol: PROTOCOL.into(),
            id: "next-request".into(),
            method: "browser_snapshot".into(),
            browser_instance: BrowserInstanceRef {
                browser_family: "chrome".into(),
                extension_id: "extension".into(),
                installation_id: "install".into(),
                profile_key: "work".into(),
                boot_id: "boot".into(),
            },
            params: Value::Null,
            deadline_ms: 20,
        };
        assert!(matches!(
            transport.request(next).await,
            Err(ExtensionBrowserError::Connection(_))
        ));
        task.await.unwrap();
    }

    async fn read_test_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Vec<u8> {
        let mut header = [0u8; 4];
        reader.read_exact(&mut header).await.unwrap();
        let mut body = vec![0u8; u32::from_le_bytes(header) as usize];
        reader.read_exact(&mut body).await.unwrap();
        [header.to_vec(), body].concat()
    }
}
