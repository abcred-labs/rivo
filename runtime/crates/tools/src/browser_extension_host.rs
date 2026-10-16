//! Native Messaging host used by the Chrome/Edge extension.
//!
//! The browser owns the stdio side of this process. Rivo connects to the
//! per-host named pipe to submit requests, which keeps browser selection local
//! and prevents an extension from opening a general-purpose HTTP listener.

use crate::browser_extension::ExtensionBrowserError;

#[cfg(windows)]
use crate::browser_extension::{
    decode_native_message, encode_native_message, ExtensionHello, ExtensionRequest,
    ExtensionResponse, MAX_MESSAGE_BYTES, MAX_REQUEST_DEADLINE_MS, PROTOCOL,
};
#[cfg(windows)]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[cfg(windows)]
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Vec<u8>, ExtensionBrowserError> {
    let mut header = [0u8; 4];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_MESSAGE_BYTES {
        return Err(ExtensionBrowserError::Protocol(
            "Native Messaging 消息超过 1 MiB".into(),
        ));
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
    let mut frame = header.to_vec();
    frame.extend_from_slice(&body);
    Ok(frame)
}

#[cfg(windows)]
async fn write_frame<W: AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), ExtensionBrowserError> {
    writer
        .write_all(&encode_native_message(value)?)
        .await
        .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))
}

/// Runs as the executable named by the Chrome/Edge Native Messaging manifest.
/// On non-Windows hosts this returns a clear error instead of opening a TCP
/// fallback, keeping the transport local-only by construction.
#[cfg(windows)]
pub async fn run() -> Result<(), ExtensionBrowserError> {
    use tokio::io::{stdin, stdout};
    use tokio::net::windows::named_pipe::ServerOptions;
    use tokio::time::{timeout, Duration};
    let mut input = stdin();
    let mut output = stdout();
    let hello_frame = timeout(Duration::from_secs(10), read_frame(&mut input))
        .await
        .map_err(|_| ExtensionBrowserError::Timeout)??;
    let hello: ExtensionHello = decode_native_message(&hello_frame)?;
    if hello.protocol != PROTOCOL || !matches!(hello.browser_family.as_str(), "chrome" | "edge") {
        return Err(ExtensionBrowserError::Protocol("扩展握手无效".into()));
    }
    let pipe_name = std::env::var("RIVO_BROWSER_EXTENSION_PIPE")
        .unwrap_or_else(|_| format!(r"\\.\pipe\rivo-browser-extension-{}", hello.browser_family));
    // Keep reading the browser-owned stream even while no Rivo client is
    // connected. Otherwise an idle host can outlive a closed Edge Profile and
    // advertise its old boot identity until the next browser request fails.
    let (input_sender, mut input_receiver) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        loop {
            let frame = read_frame(&mut input).await;
            let disconnected = frame.is_err();
            if input_sender.send(frame).await.is_err() || disconnected {
                break;
            }
        }
    });
    loop {
        let mut server = ServerOptions::new()
            // Multiple Chrome/Edge profiles can launch one Native Messaging
            // host each.  Windows named-pipe instances with the same name let
            // the Rivo client select the instance by the hello identity instead
            // of making the second profile fail during host startup.
            .first_pipe_instance(false)
            .create(&pipe_name)
            .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
        tokio::select! {
            biased;
            frame = input_receiver.recv() => return Err(idle_input_error(frame)),
            connected = server.connect() => {
                connected.map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
            }
        }
        write_frame(&mut server, &hello).await?;
        loop {
            let request_frame = match tokio::select! {
                biased;
                frame = input_receiver.recv() => return Err(idle_input_error(frame)),
                frame = read_frame(&mut server) => frame,
            } {
                Ok(frame) => frame,
                Err(ExtensionBrowserError::Connection(_)) => break,
                Err(error) => return Err(error),
            };
            let request: ExtensionRequest = decode_native_message(&request_frame)?;
            if request.protocol != PROTOCOL || request.browser_instance != hello.instance() {
                let response = ExtensionResponse {
                    protocol: PROTOCOL.into(),
                    id: request.id,
                    ok: false,
                    result: serde_json::json!({}),
                    error: Some("扩展实例身份不匹配".into()),
                    browser_instance: Some(hello.instance()),
                };
                write_frame(&mut server, &response).await?;
                continue;
            }
            if request.deadline_ms == 0 || request.deadline_ms > MAX_REQUEST_DEADLINE_MS {
                let response = ExtensionResponse {
                    protocol: PROTOCOL.into(),
                    id: request.id,
                    ok: false,
                    result: serde_json::json!({}),
                    error: Some("请求 deadline 超出允许范围".into()),
                    browser_instance: Some(hello.instance()),
                };
                write_frame(&mut server, &response).await?;
                continue;
            }
            output
                .write_all(&request_frame)
                .await
                .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
            output
                .flush()
                .await
                .map_err(|e| ExtensionBrowserError::Connection(e.to_string()))?;
            let response_frame = match timeout(
                Duration::from_millis(request.deadline_ms),
                input_receiver.recv(),
            )
            .await
            {
                Ok(Some(Ok(frame))) => frame,
                Ok(Some(Err(_))) | Ok(None) => {
                    write_frame(
                        &mut server,
                        &error_response(&request, &hello, "Native Messaging 扩展已断开"),
                    )
                    .await?;
                    return Err(ExtensionBrowserError::Connection(
                        "Native Messaging 扩展已断开".into(),
                    ));
                }
                Err(_) => {
                    write_frame(
                        &mut server,
                        &error_response(&request, &hello, "请求 deadline 已到"),
                    )
                    .await?;
                    // Ending this Native Messaging process forces the
                    // extension's onDisconnect handler to reconnect. The
                    // Rivo client receives one bounded timeout response and
                    // its current stream is discarded.
                    return Err(ExtensionBrowserError::Timeout);
                }
            };
            let response: ExtensionResponse = match decode_native_message(&response_frame) {
                Ok(response) => response,
                Err(_) => {
                    write_frame(
                        &mut server,
                        &error_response(&request, &hello, "Native Messaging 响应格式无效"),
                    )
                    .await?;
                    return Err(ExtensionBrowserError::Protocol(
                        "Native Messaging 响应格式无效".into(),
                    ));
                }
            };
            if response.protocol != PROTOCOL
                || response.id != request.id
                || response.browser_instance.as_ref() != Some(&hello.instance())
            {
                write_frame(
                    &mut server,
                    &error_response(
                        &request,
                        &hello,
                        "Native Messaging 响应身份、ID 或协议不匹配",
                    ),
                )
                .await?;
                return Err(ExtensionBrowserError::Protocol(
                    "Native Messaging 响应身份、ID 或协议不匹配".into(),
                ));
            }
            write_frame(&mut server, &response).await?;
        }
    }
}

#[cfg(windows)]
fn idle_input_error(
    frame: Option<Result<Vec<u8>, ExtensionBrowserError>>,
) -> ExtensionBrowserError {
    match frame {
        Some(Err(error)) => error,
        None => ExtensionBrowserError::Connection("Native Messaging 扩展已断开".into()),
        Some(Ok(_)) => {
            ExtensionBrowserError::Protocol("Native Messaging 扩展发送了非请求响应".into())
        }
    }
}

#[cfg(windows)]
fn error_response(
    request: &ExtensionRequest,
    hello: &ExtensionHello,
    message: &str,
) -> ExtensionResponse {
    ExtensionResponse {
        protocol: PROTOCOL.into(),
        id: request.id.clone(),
        ok: false,
        result: serde_json::json!({}),
        error: Some(message.into()),
        browser_instance: Some(hello.instance()),
    }
}

#[cfg(not(windows))]
pub async fn run() -> Result<(), ExtensionBrowserError> {
    Err(ExtensionBrowserError::Connection(
        "Native Messaging Host 仅支持 Windows 11 本地预览".into(),
    ))
}
