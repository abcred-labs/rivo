use futures::{SinkExt, StreamExt};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

const CDP_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_SNAPSHOT_CHARS: usize = 64 * 1024;
const MAX_CDP_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_CDP_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_BROWSER_TABS: usize = 100;
const MAX_TAB_TITLE_CHARS: usize = 512;
const MAX_TAB_URL_CHARS: usize = 4096;
const MAX_ELEMENT_TEXT_CHARS: usize = 200;
const MAX_ELEMENT_HREF_CHARS: usize = 4096;

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("CDP 地址无效: {0}")]
    InvalidEndpoint(String),
    #[error("无法连接浏览器 CDP: {0}")]
    Connection(String),
    #[error("浏览器 CDP 调用超时")]
    Timeout,
    #[error("浏览器 CDP 协议错误: {0}")]
    Protocol(String),
    #[error("未找到可控制的页面标签")]
    NoPage,
    #[error("目标标签页已关闭或不存在: {0}")]
    MissingTab(String),
    #[error("浏览器操作被拒绝: {0}")]
    Denied(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTab {
    pub id: String,
    pub title: String,
    pub url: String,
    pub selected: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct CdpTarget {
    id: String,
    #[serde(rename = "type")]
    target_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_debugger_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CdpTargetInfo {
    #[serde(rename = "targetId")]
    id: String,
    #[serde(rename = "type")]
    target_type: String,
    #[serde(rename = "openerId")]
    opener_id: Option<String>,
}

struct PermissionGuard {
    _release: tokio::sync::oneshot::Sender<()>,
}

struct PageSafetyGuard {
    _release: tokio::sync::oneshot::Sender<()>,
}

/// 只接受显式配置的本机 CDP。它绝不读取浏览器 Profile、Cookie 或调试端点的
/// 认证信息；浏览器本身负责持有专用 Profile 的登录态。
#[derive(Clone)]
pub struct BrowserClient {
    endpoint: Url,
    http: reqwest::Client,
    selected_target: Arc<Mutex<Option<String>>>,
}

impl BrowserClient {
    pub fn new(cdp_url: impl AsRef<str>) -> Result<Self, BrowserError> {
        let mut endpoint = Url::parse(cdp_url.as_ref())
            .map_err(|error| BrowserError::InvalidEndpoint(error.to_string()))?;
        if endpoint.scheme() != "http"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || !is_loopback_host(endpoint.host_str())
        {
            return Err(BrowserError::InvalidEndpoint(
                "必须是无凭据的 loopback HTTP 地址".into(),
            ));
        }
        endpoint.set_path("/");
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(CDP_TIMEOUT)
            .build()
            .map_err(|error| BrowserError::Connection(error.to_string()))?;
        Ok(Self {
            endpoint,
            http,
            selected_target: Arc::new(Mutex::new(None)),
        })
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub async fn list_tabs(&self) -> Result<Vec<BrowserTab>, BrowserError> {
        let selected = self
            .selected_target
            .lock()
            .expect("browser selection lock poisoned")
            .clone();
        Ok(self
            .targets()
            .await?
            .into_iter()
            .take(MAX_BROWSER_TABS)
            .map(|target| BrowserTab {
                selected: selected.as_deref() == Some(target.id.as_str()),
                id: target.id,
                title: truncate_chars(target.title, MAX_TAB_TITLE_CHARS),
                url: truncate_chars(target.url, MAX_TAB_URL_CHARS),
            })
            .collect())
    }

    pub async fn select_tab(&self, id: &str) -> Result<BrowserTab, BrowserError> {
        let target = self
            .targets()
            .await?
            .into_iter()
            .find(|target| target.id == id)
            .ok_or_else(|| BrowserError::MissingTab(id.into()))?;
        *self
            .selected_target
            .lock()
            .expect("browser selection lock poisoned") = Some(target.id.clone());
        Ok(BrowserTab {
            id: target.id,
            title: truncate_chars(target.title, MAX_TAB_TITLE_CHARS),
            url: truncate_chars(target.url, MAX_TAB_URL_CHARS),
            selected: true,
        })
    }

    pub async fn navigate(&self, url: &str) -> Result<Value, BrowserError> {
        let parsed = Url::parse(url).map_err(|error| BrowserError::Denied(error.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(BrowserError::Denied("只允许 HTTP(S) 页面地址".into()));
        }
        self.prepare_page().await?;
        let _permissions = self.permission_guard_for_url(&parsed).await?;
        // 专用 Profile 初始通常停留在 chrome://newtab。它不能被 snapshot 或选中，
        // 但可作为一次性的导航载体，进入 HTTP(S) 页面后才会暴露给其余工具。
        let target = self.navigation_target().await?;
        self.call_socket(
            target
                .websocket_debugger_url
                .as_deref()
                .ok_or(BrowserError::NoPage)?,
            "Page.navigate",
            json!({"url": parsed.as_str()}),
        )
        .await
    }

    pub async fn snapshot(&self) -> Result<Value, BrowserError> {
        self.prepare_page().await?;
        let expression = format!(
            r#"(()=>{{
 const visible=e=>{{const s=getComputedStyle(e),r=e.getBoundingClientRect();return s.display!=="none"&&s.visibility!=="hidden"&&r.width>0&&r.height>0}};
 const text=(document.body?.innerText||"").slice(0,{MAX_SNAPSHOT_CHARS});
 const elements=[]; let index=0;
 for(const e of document.querySelectorAll('a,button,input,textarea,select,[role="button"],[contenteditable="true"]')){{
   if(!visible(e)||index>=100) continue;
   if((e.type||"").toLowerCase()==="password"||(e.type||"").toLowerCase()==="file") continue;
   const ref=`rivo-${{++index}}`; e.setAttribute("data-rivo-ref",ref);
   elements.push({{ref,tag:e.tagName.toLowerCase(),text:(e.innerText||e.value||e.getAttribute("aria-label")||"").slice(0,{MAX_ELEMENT_TEXT_CHARS}),placeholder:(e.getAttribute("placeholder")||"").slice(0,{MAX_ELEMENT_TEXT_CHARS})||null,href:(e.href||"").slice(0,{MAX_ELEMENT_HREF_CHARS})||null}});
 }}
 return JSON.stringify({{title:document.title.slice(0,{MAX_TAB_TITLE_CHARS}),url:location.href.slice(0,{MAX_TAB_URL_CHARS}),text,elements}});
}})()"#
        );
        let snapshot = self.evaluate(&expression).await?;
        let text = snapshot
            .as_str()
            .ok_or_else(|| BrowserError::Protocol("snapshot 未返回文本".into()))?;
        serde_json::from_str(text).map_err(|error| BrowserError::Protocol(error.to_string()))
    }

    pub async fn click(&self, reference: &str) -> Result<Value, BrowserError> {
        validate_element_ref(reference)?;
        self.prepare_page().await?;
        let target = self.selected_target().await?;
        let script = element_script(
            reference,
            "if((e.target||'').toLowerCase()==='_blank'||(e.form&&((e.form.target||'').toLowerCase()==='_blank')))throw new Error('禁止打开新窗口');return {x:r.left+r.width/2,y:r.top+r.height/2};",
        );
        let result = async {
            let point = self.evaluate(&script).await?;
            let x = point
                .get("x")
                .and_then(Value::as_f64)
                .ok_or_else(|| BrowserError::Protocol("点击元素缺少 x 坐标".into()))?;
            let y = point
                .get("y")
                .and_then(Value::as_f64)
                .ok_or_else(|| BrowserError::Protocol("点击元素缺少 y 坐标".into()))?;
            self.call_page(
                "Input.dispatchMouseEvent",
                json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
            )
            .await?;
            self.call_page(
                "Input.dispatchMouseEvent",
                json!({"type":"mouseReleased","x":x,"y":y,"button":"left","clickCount":1}),
            )
            .await?;
            Ok(json!({"clicked":true}))
        }
        .await;
        // 页面脚本可能在 click handler 中使用预先缓存的 window.open。CDP 以
        // openerId 标记这类子 target，因此即使网页绕过运行时覆写也不会被接管。
        let close_result = self.close_child_pages(&target.id).await;
        let result = result?;
        close_result?;
        Ok(result)
    }

    pub async fn type_text(
        &self,
        reference: &str,
        text: &str,
        clear: bool,
    ) -> Result<Value, BrowserError> {
        validate_element_ref(reference)?;
        self.prepare_page().await?;
        if text.len() > 16 * 1024 {
            return Err(BrowserError::Denied("输入文本超过 16 KiB 限制".into()));
        }
        let value = serde_json::to_string(text).expect("text serializes");
        let body = format!(
            r#"const kind=(e.type||"").toLowerCase(); if(kind==="password"||kind==="file") throw new Error("禁止操作 password/file 字段"); e.focus(); if(e.isContentEditable){{if({clear}) e.textContent="";e.textContent+=({value});}} else {{if({clear}) e.value="";e.value+=({value});}} e.dispatchEvent(new Event("input",{{bubbles:true}})); e.dispatchEvent(new Event("change",{{bubbles:true}})); return {{value_length:(e.value||e.textContent||"").length}};"#,
        );
        let target = self.selected_target().await?;
        let result = self.evaluate(&element_script(reference, &body)).await;
        let close_result = self.close_child_pages(&target.id).await;
        let result = result?;
        close_result?;
        Ok(result)
    }

    pub async fn select(&self, reference: &str, value: &str) -> Result<Value, BrowserError> {
        validate_element_ref(reference)?;
        self.prepare_page().await?;
        let value = serde_json::to_string(value).expect("select value serializes");
        let target = self.selected_target().await?;
        let script = element_script(
            reference,
            &format!(
                r#"if(e.tagName!=="SELECT") throw new Error("目标不是 select"); const wanted={value}; const option=[...e.options].find(o=>o.value===wanted||o.text===wanted); if(!option) throw new Error("未找到选项"); e.value=option.value; e.dispatchEvent(new Event("input",{{bubbles:true}})); e.dispatchEvent(new Event("change",{{bubbles:true}})); return {{value:e.value}};"#,
            ),
        );
        let result = self.evaluate(&script).await;
        let close_result = self.close_child_pages(&target.id).await;
        let result = result?;
        close_result?;
        Ok(result)
    }

    pub async fn keypress(&self, key: &str) -> Result<Value, BrowserError> {
        if key.is_empty() || key.len() > 64 {
            return Err(BrowserError::Denied("按键名称无效".into()));
        }
        self.prepare_page().await?;
        let target = self.selected_target().await?;
        let result = async {
            self.call_page(
                "Input.dispatchKeyEvent",
                json!({"type":"keyDown","key":key}),
            )
            .await?;
            self.call_page("Input.dispatchKeyEvent", json!({"type":"keyUp","key":key}))
                .await
        }
        .await;
        let close_result = self.close_child_pages(&target.id).await;
        let result = result?;
        close_result?;
        Ok(result)
    }

    pub async fn scroll(&self, x: i64, y: i64) -> Result<Value, BrowserError> {
        self.prepare_page().await?;
        let target = self.selected_target().await?;
        let result = self
            .evaluate(&format!(
                "(()=>{{window.scrollBy({x},{y});return {{x:window.scrollX,y:window.scrollY}}}})()"
            ))
            .await;
        let close_result = self.close_child_pages(&target.id).await;
        let result = result?;
        close_result?;
        Ok(result)
    }

    pub async fn wait_for(
        &self,
        reference: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Value, BrowserError> {
        self.prepare_page().await?;
        let target = self.selected_target().await?;
        let timeout = Duration::from_millis(timeout_ms.clamp(50, 30_000));
        if let Some(reference) = reference {
            validate_element_ref(reference)?;
            let encoded = serde_json::to_string(reference).expect("reference serializes");
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let evaluated = self
                    .evaluate(&format!(
                        "[...document.querySelectorAll('[data-rivo-ref]')].some(e=>e.getAttribute('data-rivo-ref')==={encoded})"
                    ))
                    .await;
                let found = match evaluated {
                    Ok(value) => value.as_bool().unwrap_or(false),
                    Err(error) => {
                        self.close_child_pages(&target.id).await?;
                        return Err(error);
                    }
                };
                if found {
                    self.close_child_pages(&target.id).await?;
                    return Ok(json!({"ref":reference,"ready":true}));
                }
                if tokio::time::Instant::now() >= deadline {
                    self.close_child_pages(&target.id).await?;
                    return Err(BrowserError::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        tokio::time::sleep(timeout).await;
        self.close_child_pages(&target.id).await?;
        Ok(json!({"waited_ms": timeout.as_millis()}))
    }

    async fn targets(&self) -> Result<Vec<CdpTarget>, BrowserError> {
        let url = self
            .endpoint
            .join("json/list")
            .map_err(|error| BrowserError::InvalidEndpoint(error.to_string()))?;
        let targets: Vec<CdpTarget> = self.fetch_json(url).await?;
        Ok(targets
            .into_iter()
            .filter(is_web_page_target)
            .take(MAX_BROWSER_TABS)
            .collect())
    }

    async fn navigation_target(&self) -> Result<CdpTarget, BrowserError> {
        let selected = self
            .selected_target
            .lock()
            .expect("browser selection lock poisoned")
            .clone();
        let url = self
            .endpoint
            .join("json/list")
            .map_err(|error| BrowserError::InvalidEndpoint(error.to_string()))?;
        let targets: Vec<CdpTarget> = self.fetch_json(url).await?;
        let target = match selected.as_deref() {
            Some(id) => targets
                .iter()
                .find(|target| target.id == id && is_page_target(target))
                .cloned()
                .ok_or_else(|| BrowserError::MissingTab(id.into()))?,
            None => targets
                .iter()
                .find(|target| is_page_target(target))
                .cloned()
                .ok_or(BrowserError::NoPage)?,
        };
        *self
            .selected_target
            .lock()
            .expect("browser selection lock poisoned") = Some(target.id.clone());
        Ok(target)
    }

    async fn selected_target(&self) -> Result<CdpTarget, BrowserError> {
        let selected = self
            .selected_target
            .lock()
            .expect("browser selection lock poisoned")
            .clone();
        let targets = self.targets().await?;
        let target = match selected.as_deref() {
            Some(id) => targets
                .iter()
                .find(|target| target.id == id)
                .cloned()
                .ok_or_else(|| BrowserError::MissingTab(id.into()))?,
            None => targets.first().cloned().ok_or(BrowserError::NoPage)?,
        };
        *self
            .selected_target
            .lock()
            .expect("browser selection lock poisoned") = Some(target.id.clone());
        Ok(target)
    }

    async fn prepare_page(&self) -> Result<(), BrowserError> {
        let websocket = self.browser_websocket().await?;
        // 阻止下载，避免页面点击将文件写入用户目录。
        self.call_socket(
            &websocket,
            "Browser.setDownloadBehavior",
            json!({"behavior":"deny"}),
        )
        .await?;
        Ok(())
    }

    async fn permission_guard_for_url(&self, url: &Url) -> Result<PermissionGuard, BrowserError> {
        let origin = url.origin().ascii_serialization();
        if origin == "null" {
            return Err(BrowserError::Denied("无法确定页面 origin".into()));
        }
        self.permission_guard(Some(origin.as_str())).await
    }

    /// Browser.setPermission 的覆写与 CDP 连接生命周期绑定。保持浏览器级连接
    /// 直到页面动作完成，才能确保 Chromium/Edge 在实际用户输入时仍为 denied。
    async fn permission_guard(
        &self,
        origin: Option<&str>,
    ) -> Result<PermissionGuard, BrowserError> {
        let websocket = self.browser_websocket().await?;
        let websocket_url =
            Url::parse(&websocket).map_err(|error| BrowserError::Protocol(error.to_string()))?;
        if websocket_url.scheme() != "ws"
            || !websocket_url.username().is_empty()
            || websocket_url.password().is_some()
            || !is_loopback_host(websocket_url.host_str())
        {
            return Err(BrowserError::Protocol(
                "CDP WebSocket 必须指向无凭据的 loopback 地址".into(),
            ));
        }
        let origin = origin.map(str::to_owned);
        let operation = async move {
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_CDP_MESSAGE_BYTES))
                .max_frame_size(Some(MAX_CDP_MESSAGE_BYTES));
            let (mut socket, _) = connect_async_with_config(&websocket, Some(config), false)
                .await
                .map_err(|error| BrowserError::Connection(error.to_string()))?;
            for (index, permission) in [
                json!({"name":"geolocation"}),
                json!({"name":"notifications"}),
                json!({"name":"camera"}),
                json!({"name":"microphone"}),
                json!({"name":"midi"}),
                json!({"name":"midi","sysex":true}),
                json!({"name":"clipboard-read"}),
                json!({"name":"clipboard-write"}),
                json!({"name":"payment-handler"}),
            ]
            .into_iter()
            .enumerate()
            {
                let id = index as u64 + 1;
                let mut params = json!({"permission":permission,"setting":"denied"});
                if let Some(origin) = &origin {
                    params["origin"] = Value::String(origin.clone());
                }
                socket
                    .send(Message::Text(
                        json!({"id":id,"method":"Browser.setPermission","params":params})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .map_err(|error| BrowserError::Connection(error.to_string()))?;
                loop {
                    let message = socket
                        .next()
                        .await
                        .ok_or_else(|| BrowserError::Connection("CDP 连接已关闭".into()))?
                        .map_err(|error| BrowserError::Connection(error.to_string()))?;
                    let text = match message {
                        Message::Text(text) => text,
                        Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
                            .map_err(|error| BrowserError::Protocol(error.to_string()))?
                            .into(),
                        Message::Close(_) => {
                            return Err(BrowserError::Connection("CDP 连接已关闭".into()))
                        }
                        _ => continue,
                    };
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|error| BrowserError::Protocol(error.to_string()))?;
                    if value.get("id") != Some(&json!(id)) {
                        continue;
                    }
                    if let Some(error) = value.get("error") {
                        return Err(BrowserError::Protocol(error.to_string()));
                    }
                    break;
                }
            }
            let (release, closed) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _ = closed.await;
                let _ = socket.close(None).await;
            });
            Ok(PermissionGuard { _release: release })
        };
        tokio::time::timeout(CDP_TIMEOUT, operation)
            .await
            .map_err(|_| BrowserError::Timeout)?
    }

    async fn evaluate(&self, expression: &str) -> Result<Value, BrowserError> {
        let response = self
            .call_page(
                "Runtime.evaluate",
                json!({"expression":expression,"awaitPromise":true,"returnByValue":true,"userGesture":true}),
            )
            .await?;
        if let Some(details) = response.get("exceptionDetails") {
            return Err(BrowserError::Protocol(format!("页面脚本失败: {details}")));
        }
        response
            .pointer("/result/value")
            .cloned()
            .ok_or_else(|| BrowserError::Protocol("页面脚本没有返回值".into()))
    }

    async fn call_page(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
        let target = self.selected_target().await?;
        let page_url =
            Url::parse(&target.url).map_err(|error| BrowserError::Protocol(error.to_string()))?;
        let _permissions = self.permission_guard_for_url(&page_url).await?;
        let _file_chooser = self.file_safety_guard(&target).await?;
        // 拦截所有由页面触发的文件选择器；否则 label 或页面脚本能绕过对
        // input[type=file] 的元素级拒绝并弹出原生上传窗口。
        // 所有由工具注入的页面交互都先永久屏蔽 window.open；click/type/select
        // 的事件处理器因而不能接管一个新窗口。close_child_pages 是第二道防线。
        self.call_socket(
            target
                .websocket_debugger_url
                .as_deref()
                .ok_or(BrowserError::NoPage)?,
            "Runtime.evaluate",
            json!({
                "expression":"(()=>{try{Object.defineProperty(window,'open',{value:()=>null,writable:false,configurable:false})}catch(_){try{window.open=()=>null}catch(_){}}})()",
                "returnByValue":true
            }),
        )
        .await?;
        self.call_socket(
            target
                .websocket_debugger_url
                .as_deref()
                .ok_or(BrowserError::NoPage)?,
            method,
            params,
        )
        .await
    }

    /// Page.setInterceptFileChooserDialog 绑定页面 CDP 会话。保持该会话直到
    /// 当前动作结束，避免页面脚本绕过 input[type=file] 的元素级检查。
    async fn file_safety_guard(&self, target: &CdpTarget) -> Result<PageSafetyGuard, BrowserError> {
        let websocket = target
            .websocket_debugger_url
            .as_deref()
            .ok_or(BrowserError::NoPage)?;
        let websocket_url =
            Url::parse(websocket).map_err(|error| BrowserError::Protocol(error.to_string()))?;
        if websocket_url.scheme() != "ws"
            || !websocket_url.username().is_empty()
            || websocket_url.password().is_some()
            || !is_loopback_host(websocket_url.host_str())
        {
            return Err(BrowserError::Protocol(
                "CDP WebSocket 必须指向无凭据的 loopback 地址".into(),
            ));
        }
        let operation = async {
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_CDP_MESSAGE_BYTES))
                .max_frame_size(Some(MAX_CDP_MESSAGE_BYTES));
            let (mut socket, _) = connect_async_with_config(websocket, Some(config), false)
                .await
                .map_err(|error| BrowserError::Connection(error.to_string()))?;
            socket
                .send(Message::Text(
                    json!({"id":1,"method":"Page.setInterceptFileChooserDialog","params":{"enabled":true}})
                        .to_string()
                        .into(),
                ))
                .await
                .map_err(|error| BrowserError::Connection(error.to_string()))?;
            loop {
                let message = socket
                    .next()
                    .await
                    .ok_or_else(|| BrowserError::Connection("CDP 连接已关闭".into()))?
                    .map_err(|error| BrowserError::Connection(error.to_string()))?;
                let text = match message {
                    Message::Text(text) => text,
                    Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
                        .map_err(|error| BrowserError::Protocol(error.to_string()))?
                        .into(),
                    Message::Close(_) => {
                        return Err(BrowserError::Connection("CDP 连接已关闭".into()))
                    }
                    _ => continue,
                };
                let value: Value = serde_json::from_str(&text)
                    .map_err(|error| BrowserError::Protocol(error.to_string()))?;
                if value.get("id") != Some(&json!(1)) {
                    continue;
                }
                if let Some(error) = value.get("error") {
                    return Err(BrowserError::Protocol(error.to_string()));
                }
                break;
            }
            let (release, closed) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _ = closed.await;
                let _ = socket.close(None).await;
            });
            Ok(PageSafetyGuard { _release: release })
        };
        tokio::time::timeout(CDP_TIMEOUT, operation)
            .await
            .map_err(|_| BrowserError::Timeout)?
    }

    async fn browser_websocket(&self) -> Result<String, BrowserError> {
        let version_url = self
            .endpoint
            .join("json/version")
            .map_err(|error| BrowserError::InvalidEndpoint(error.to_string()))?;
        let version: Value = self.fetch_json(version_url).await?;
        version
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| BrowserError::Protocol("CDP version 缺少 WebSocket 地址".into()))
    }

    async fn close_child_pages(&self, opener_id: &str) -> Result<(), BrowserError> {
        let websocket = self.browser_websocket().await?;
        let targets = self
            .call_socket(&websocket, "Target.getTargets", json!({}))
            .await?;
        let children = targets
            .pointer("/targetInfos")
            .and_then(Value::as_array)
            .ok_or_else(|| BrowserError::Protocol("CDP 未返回 targetInfos".into()))?
            .iter()
            .cloned()
            .map(serde_json::from_value::<CdpTargetInfo>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| BrowserError::Protocol(error.to_string()))?;
        for child in children.into_iter().filter(|child| {
            child.target_type == "page" && child.opener_id.as_deref() == Some(opener_id)
        }) {
            let closed = self
                .call_socket(
                    &websocket,
                    "Target.closeTarget",
                    json!({"targetId":child.id}),
                )
                .await?;
            if closed.get("success").and_then(Value::as_bool) != Some(true) {
                return Err(BrowserError::Protocol("无法关闭页面打开的新窗口".into()));
            }
        }
        Ok(())
    }

    async fn call_socket(
        &self,
        websocket: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        self.call_socket_with_timeout(websocket, method, params, CDP_TIMEOUT)
            .await
    }

    async fn call_socket_with_timeout(
        &self,
        websocket: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, BrowserError> {
        let websocket_url =
            Url::parse(websocket).map_err(|error| BrowserError::Protocol(error.to_string()))?;
        if websocket_url.scheme() != "ws"
            || !websocket_url.username().is_empty()
            || websocket_url.password().is_some()
            || !is_loopback_host(websocket_url.host_str())
        {
            return Err(BrowserError::Protocol(
                "CDP WebSocket 必须指向无凭据的 loopback 地址".into(),
            ));
        }
        let operation = async {
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_CDP_MESSAGE_BYTES))
                .max_frame_size(Some(MAX_CDP_MESSAGE_BYTES));
            let (socket, _) = connect_async_with_config(websocket, Some(config), false)
                .await
                .map_err(|error| BrowserError::Connection(error.to_string()))?;
            let (mut write, mut read) = socket.split();
            // Page 的事件订阅绑定在当前 CDP WebSocket 会话上。Runtime.evaluate
            // 可能触发 alert/confirm，因此必须先在同一连接启用 Page 域。
            if method == "Runtime.evaluate" || method.starts_with("Input.") {
                write
                    .send(Message::Text(
                        json!({"id":2,"method":"Page.enable","params":{}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .map_err(|error| BrowserError::Connection(error.to_string()))?;
            }
            write
                .send(Message::Text(
                    json!({"id":1,"method":method,"params":params})
                        .to_string()
                        .into(),
                ))
                .await
                .map_err(|error| BrowserError::Connection(error.to_string()))?;
            while let Some(message) = read.next().await {
                let message =
                    message.map_err(|error| BrowserError::Connection(error.to_string()))?;
                let text = match message {
                    Message::Text(text) if text.len() <= MAX_CDP_MESSAGE_BYTES => text,
                    Message::Binary(bytes) if bytes.len() <= MAX_CDP_MESSAGE_BYTES => {
                        String::from_utf8(bytes.to_vec())
                            .map_err(|error| BrowserError::Protocol(error.to_string()))?
                            .into()
                    }
                    Message::Text(_) | Message::Binary(_) => {
                        return Err(BrowserError::Protocol(
                            "CDP WebSocket 响应超过 1 MiB 限制".into(),
                        ))
                    }
                    Message::Close(_) => {
                        return Err(BrowserError::Connection("CDP 连接已关闭".into()))
                    }
                    _ => continue,
                };
                let value: Value = serde_json::from_str(&text)
                    .map_err(|error| BrowserError::Protocol(error.to_string()))?;
                if value.get("method").and_then(Value::as_str)
                    == Some("Page.javascriptDialogOpening")
                {
                    write
                        .send(Message::Text(
                            json!({
                                "id": 3,
                                "method": "Page.handleJavaScriptDialog",
                                "params": {"accept": false}
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .map_err(|error| BrowserError::Connection(error.to_string()))?;
                    continue;
                }
                if value.get("id") != Some(&json!(1)) {
                    continue;
                }
                if let Some(error) = value.get("error") {
                    return Err(BrowserError::Protocol(error.to_string()));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            Err(BrowserError::Connection("CDP 未返回响应".into()))
        };
        tokio::time::timeout(timeout, operation)
            .await
            .map_err(|_| BrowserError::Timeout)?
    }

    async fn fetch_json<T: DeserializeOwned>(&self, url: Url) -> Result<T, BrowserError> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| BrowserError::Connection(error.to_string()))?;
        if response.status().is_redirection() {
            return Err(BrowserError::Protocol(
                "CDP HTTP 发现端点不允许重定向".into(),
            ));
        }
        let mut response = response
            .error_for_status()
            .map_err(|error| BrowserError::Connection(error.to_string()))?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| BrowserError::Connection(error.to_string()))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_CDP_RESPONSE_BYTES {
                return Err(BrowserError::Protocol(
                    "CDP HTTP 响应超过 1 MiB 限制".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|error| BrowserError::Protocol(error.to_string()))
    }
}

fn is_loopback_host(host: Option<&str>) -> bool {
    match host {
        Some("localhost") | Some("localhost.") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false),
        None => false,
    }
}

fn is_page_target(target: &CdpTarget) -> bool {
    target.target_type == "page" && target.websocket_debugger_url.is_some()
}

fn is_web_page_target(target: &CdpTarget) -> bool {
    is_page_target(target)
        && Url::parse(&target.url)
            .ok()
            .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn validate_element_ref(reference: &str) -> Result<(), BrowserError> {
    if reference.len() > 32
        || !reference.starts_with("rivo-")
        || reference["rivo-".len()..].is_empty()
        || !reference["rivo-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(BrowserError::Denied(
            "元素引用必须是 snapshot 提供的 rivo-N".into(),
        ));
    }
    Ok(())
}

fn truncate_chars(mut value: String, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value;
    }
    let end = value
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    value.truncate(end);
    value
}

fn element_script(reference: &str, body: &str) -> String {
    let reference = serde_json::to_string(reference).expect("reference serializes");
    format!(
        r#"(()=>{{const e=[...document.querySelectorAll('[data-rivo-ref]')].find(e=>e.getAttribute('data-rivo-ref')==={reference});if(!e)throw new Error("找不到 snapshot 元素引用");const s=getComputedStyle(e),r=e.getBoundingClientRect();if(s.display==="none"||s.visibility==="hidden"||r.width<=0||r.height<=0)throw new Error("元素不再可见");if(e.matches('input[type=file]'))throw new Error("禁止操作 file 字段");{body}}})()"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct CdpFixture {
        endpoint: String,
        methods: Arc<tokio::sync::Mutex<Vec<String>>>,
        requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
    }

    async fn target_fixture(targets: Value) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            let body = targets.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }

    async fn redirect_fixture() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://example.com/json/list\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        format!("http://{address}")
    }

    async fn cdp_fixture() -> CdpFixture {
        let websocket_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let websocket_address = websocket_listener.local_addr().unwrap();
        let methods = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let websocket_methods = methods.clone();
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let websocket_requests = requests.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = websocket_listener.accept().await {
                let methods = websocket_methods.clone();
                let requests = websocket_requests.clone();
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(message)) = socket.next().await {
                        let Message::Text(text) = message else {
                            continue;
                        };
                        let request: Value = serde_json::from_str(&text).unwrap();
                        let method = request["method"].as_str().unwrap().to_owned();
                        methods.lock().await.push(method.clone());
                        requests.lock().await.push(request.clone());
                        let opens_dialog = method == "Input.dispatchMouseEvent"
                            && request["params"]["type"] == "mouseReleased";
                        let result = match method.as_str() {
                            "Runtime.evaluate" => {
                                let expression =
                                    request["params"]["expression"].as_str().unwrap_or_default();
                                if expression.contains("return JSON.stringify") {
                                    json!({"result":{"value":"{\"title\":\"Fixture\",\"url\":\"https://example.com/\",\"text\":\"body\",\"elements\":[{\"ref\":\"rivo-1\",\"tag\":\"button\",\"text\":\"go\",\"placeholder\":null,\"href\":null}]}"}})
                                } else if expression.contains("r.left+r.width/2") {
                                    json!({"result":{"value":{"x":10.0,"y":10.0}}})
                                } else if expression.contains("querySelectorAll('[data-rivo-ref]')")
                                {
                                    json!({"result":{"value":true}})
                                } else {
                                    json!({"result":{"value":{"url":"https://example.com/"}}})
                                }
                            }
                            "Target.getTargets" => {
                                json!({"targetInfos":[{"targetId":"popup-1","type":"page","openerId":"page-1"}]})
                            }
                            "Target.closeTarget" => json!({"success":true}),
                            "Page.navigate" => json!({"frameId":"frame-1"}),
                            _ => json!({}),
                        };
                        if opens_dialog {
                            socket
                                .send(Message::Text(
                                    json!({
                                        "method":"Page.javascriptDialogOpening",
                                        "params":{"type":"alert","message":"fixture"}
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await
                                .unwrap();
                            let Some(Ok(Message::Text(reply))) = socket.next().await else {
                                return;
                            };
                            let reply: Value = serde_json::from_str(&reply).unwrap();
                            assert_eq!(reply["method"], "Page.handleJavaScriptDialog");
                            assert_eq!(reply["params"]["accept"], false);
                            methods
                                .lock()
                                .await
                                .push(reply["method"].as_str().unwrap().into());
                            requests.lock().await.push(reply);
                        }
                        let response = json!({"id":request["id"],"result":result});
                        socket
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                    }
                });
            }
        });

        let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_address = http_listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = http_listener.accept().await {
                let websocket = format!("ws://{websocket_address}/devtools/browser");
                tokio::spawn(async move {
                    let mut request = [0u8; 4096];
                    let count = socket.read(&mut request).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..count]);
                    let body = if request.starts_with("GET /json/version") {
                        json!({"webSocketDebuggerUrl":websocket}).to_string()
                    } else {
                        json!([{"id":"page-1","type":"page","title":"Fixture","url":"https://example.com/","webSocketDebuggerUrl":format!("ws://{websocket_address}/devtools/page/page-1")}]).to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        CdpFixture {
            endpoint: format!("http://{http_address}"),
            methods,
            requests,
        }
    }

    async fn live_page_fixture() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let html = Arc::new(
            r#"<!doctype html><meta charset="utf-8"><title>Rivo CDP fixture</title>
<button id="popup" onclick="window.open('https://example.com/','_blank')">open popup</button>
<button id="dialog" onclick="alert('fixture dialog');document.querySelector('#result').textContent='dialog-closed'">open dialog</button>
<button id="permission" onclick="Notification.requestPermission().then(value=>document.querySelector('#result').textContent=value)">request permission</button>
<button id="file-trigger" onclick="document.querySelector('#file').click()">open file chooser</button><input id="file" type="file">
<input id="input" placeholder="fixture input"><select id="select"><option value="one">one</option><option value="two">two</option></select>
<output id="result"></output><div style="height:3000px">scroll fixture</div>"#,
        );
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let html = html.clone();
                tokio::spawn(async move {
                    let mut request = [0u8; 4096];
                    let _ = socket.read(&mut request).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        html.len(),
                        html
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{address}/")
    }

    async fn websocket_fixture(mode: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = socket.next().await;
            match mode {
                "close" => {
                    let _ = socket.close(None).await;
                }
                "stall" => tokio::time::sleep(Duration::from_millis(100)).await,
                _ => unreachable!(),
            }
        });
        format!("ws://{address}/devtools/page/fixture")
    }

    fn snapshot_ref(snapshot: &Value, tag: &str, text: Option<&str>) -> String {
        snapshot["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| {
                element["tag"] == tag
                    && text.is_none_or(|expected| element["text"].as_str() == Some(expected))
            })
            .and_then(|element| element["ref"].as_str())
            .unwrap()
            .to_owned()
    }

    #[test]
    fn only_allows_loopback_http_cdp_endpoints() {
        assert!(BrowserClient::new("http://127.0.0.1:9222").is_ok());
        assert!(BrowserClient::new("http://[::1]:9222").is_ok());
        assert!(BrowserClient::new("https://127.0.0.1:9222").is_err());
        assert!(BrowserClient::new("http://user:pass@127.0.0.1:9222").is_err());
        assert!(BrowserClient::new("http://example.com:9222").is_err());
    }

    #[test]
    fn only_accepts_snapshot_element_references() {
        assert!(validate_element_ref("").is_err());
        assert!(validate_element_ref("button").is_err());
        assert!(validate_element_ref("rivo-").is_err());
        assert!(validate_element_ref("rivo-1").is_ok());
    }

    #[test]
    fn rejects_non_local_websocket_targets() {
        let client = BrowserClient::new("http://127.0.0.1:9222").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime
            .block_on(client.call_socket(
                "ws://example.com/devtools/page/1",
                "Runtime.evaluate",
                json!({}),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("loopback"));
    }

    #[tokio::test]
    async fn cdp_disconnect_and_transport_timeout_are_structured_failures() {
        let client = BrowserClient::new("http://127.0.0.1:9222").unwrap();
        let error = client
            .call_socket(
                &websocket_fixture("close").await,
                "Runtime.evaluate",
                json!({}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("连接已关闭"));

        let error = client
            .call_socket_with_timeout(
                &websocket_fixture("stall").await,
                "Runtime.evaluate",
                json!({}),
                Duration::from_millis(20),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, BrowserError::Timeout));
    }

    #[tokio::test]
    async fn never_follows_cdp_discovery_redirects() {
        let client = BrowserClient::new(redirect_fixture().await).unwrap();
        let error = client.list_tabs().await.unwrap_err();
        assert!(error.to_string().contains("不允许重定向"));
    }

    #[tokio::test]
    async fn only_exposes_http_pages_but_can_use_internal_new_tab_for_navigation() {
        let targets = json!([
            {"id":"newtab","type":"page","title":"New Tab","url":"chrome://newtab/","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/newtab"},
            {"id":"history","type":"page","title":"History","url":"chrome://history/","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/history"},
            {"id":"extension","type":"page","title":"Extension","url":"chrome-extension://id/popup.html","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/extension"},
            {"id":"web","type":"page","title":"Web","url":"https://example.com/","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/web"}
        ]);
        let client = BrowserClient::new(target_fixture(targets.clone()).await).unwrap();
        let tabs = client.list_tabs().await.unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, "web");

        let client = BrowserClient::new(target_fixture(targets).await).unwrap();
        let target = client.navigation_target().await.unwrap();
        assert_eq!(target.id, "newtab");
    }

    #[tokio::test]
    async fn closed_selected_target_never_falls_back_to_another_page() {
        let targets = json!([{
            "id":"other",
            "type":"page",
            "title":"Other",
            "url":"https://example.com/",
            "webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/other"
        }]);
        let client = BrowserClient::new(target_fixture(targets.clone()).await).unwrap();
        *client.selected_target.lock().unwrap() = Some("closed".into());
        assert!(matches!(
            client.selected_target().await,
            Err(BrowserError::MissingTab(id)) if id == "closed"
        ));

        let client = BrowserClient::new(target_fixture(targets).await).unwrap();
        *client.selected_target.lock().unwrap() = Some("closed".into());
        assert!(matches!(
            client.navigation_target().await,
            Err(BrowserError::MissingTab(id)) if id == "closed"
        ));
    }

    #[tokio::test]
    async fn bounds_tab_metadata_and_discovery_response_size() {
        let title = "t".repeat(MAX_TAB_TITLE_CHARS + 1);
        let url = format!("https://example.com/{}", "a".repeat(MAX_TAB_URL_CHARS + 1));
        let targets = json!([{
            "id":"web",
            "type":"page",
            "title":title,
            "url":url,
            "webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/web"
        }]);
        let client = BrowserClient::new(target_fixture(targets).await).unwrap();
        let tabs = client.list_tabs().await.unwrap();
        assert_eq!(tabs[0].title.chars().count(), MAX_TAB_TITLE_CHARS);
        assert_eq!(tabs[0].url.chars().count(), MAX_TAB_URL_CHARS);

        let oversized = json!([{
            "id":"web",
            "type":"page",
            "title":"x".repeat(MAX_CDP_RESPONSE_BYTES),
            "url":"https://example.com/",
            "webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/web"
        }]);
        let client = BrowserClient::new(target_fixture(oversized).await).unwrap();
        let error = client.list_tabs().await.unwrap_err();
        assert!(error.to_string().contains("超过 1 MiB"));
    }

    #[tokio::test]
    async fn cdp_fixture_covers_all_public_actions_and_safety_controls() {
        let fixture = cdp_fixture().await;
        let client = BrowserClient::new(&fixture.endpoint).unwrap();
        assert_eq!(client.list_tabs().await.unwrap().len(), 1);
        assert!(client.select_tab("page-1").await.unwrap().selected);
        assert_eq!(
            client.navigate("https://example.com/path").await.unwrap()["frameId"],
            "frame-1"
        );
        assert_eq!(client.snapshot().await.unwrap()["title"], "Fixture");
        client.click("rivo-1").await.unwrap();
        client.type_text("rivo-1", "hello", true).await.unwrap();
        client.select("rivo-1", "value").await.unwrap();
        client.keypress("Enter").await.unwrap();
        client.scroll(0, 120).await.unwrap();
        assert_eq!(
            client.wait_for(Some("rivo-1"), 50).await.unwrap()["ready"],
            true
        );

        let methods = fixture.methods.lock().await.clone();
        for method in [
            "Browser.setDownloadBehavior",
            "Browser.setPermission",
            "Page.setInterceptFileChooserDialog",
            "Page.handleJavaScriptDialog",
            "Page.navigate",
            "Runtime.evaluate",
            "Input.dispatchKeyEvent",
            "Target.getTargets",
            "Target.closeTarget",
        ] {
            assert!(
                methods.iter().any(|seen| seen == method),
                "missing {method}"
            );
        }
        assert!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.closeTarget")
                .count()
                >= 6,
            "every page interaction should close child targets"
        );
        let permissions = fixture
            .requests
            .lock()
            .await
            .iter()
            .filter(|request| request["method"] == "Browser.setPermission")
            .map(|request| request["params"]["permission"]["name"].clone())
            .collect::<Vec<_>>();
        for permission in [
            "geolocation",
            "notifications",
            "camera",
            "microphone",
            "midi",
            "clipboard-read",
            "clipboard-write",
            "payment-handler",
        ] {
            assert!(permissions.iter().any(|seen| seen == permission));
        }
    }

    /// 手动验收：用户必须以专用非默认 Profile 启动 Chrome/Edge，且仅绑定
    /// 127.0.0.1:9222。该测试只操作临时本机页面，不读取 Profile 数据。
    #[tokio::test]
    #[ignore = "optional: requires a user-started dedicated CDP browser on 127.0.0.1:9222"]
    async fn live_cdp_browser_enforces_interaction_safety_contract() {
        let client = BrowserClient::new("http://127.0.0.1:9222").unwrap();
        let url = live_page_fixture().await;
        client.navigate(&url).await.unwrap();
        client.wait_for(None, 500).await.unwrap();

        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot["title"], "Rivo CDP fixture");
        let local_tab = client
            .list_tabs()
            .await
            .unwrap()
            .into_iter()
            .find(|tab| tab.url == url)
            .unwrap();
        assert!(client.select_tab(&local_tab.id).await.unwrap().selected);
        let tabs_before_popup = client.list_tabs().await.unwrap().len();
        client
            .click(&snapshot_ref(&snapshot, "button", Some("open popup")))
            .await
            .unwrap();
        client.wait_for(None, 200).await.unwrap();
        assert_eq!(client.list_tabs().await.unwrap().len(), tabs_before_popup);

        let snapshot = client.snapshot().await.unwrap();
        client
            .click(&snapshot_ref(&snapshot, "button", Some("open dialog")))
            .await
            .unwrap();
        let snapshot = client.snapshot().await.unwrap();
        assert!(snapshot["text"].as_str().unwrap().contains("dialog-closed"));

        client
            .click(&snapshot_ref(
                &snapshot,
                "button",
                Some("request permission"),
            ))
            .await
            .unwrap();
        client.wait_for(None, 200).await.unwrap();
        let snapshot = client.snapshot().await.unwrap();
        assert!(
            snapshot["text"].as_str().unwrap().contains("denied"),
            "permission result: {:?}",
            snapshot["text"]
        );

        client
            .click(&snapshot_ref(
                &snapshot,
                "button",
                Some("open file chooser"),
            ))
            .await
            .unwrap();

        let input = snapshot_ref(&snapshot, "input", None);
        assert_eq!(
            client.type_text(&input, "hello", true).await.unwrap()["value_length"],
            5
        );
        let select = snapshot_ref(&snapshot, "select", None);
        assert_eq!(client.select(&select, "two").await.unwrap()["value"], "two");
        client.keypress("Tab").await.unwrap();
        client.scroll(0, 500).await.unwrap();
        assert_eq!(
            client.wait_for(Some(&input), 50).await.unwrap()["ready"],
            true
        );
    }

    #[test]
    fn click_script_rejects_new_window_targets() {
        let script = element_script(
            "rivo-1",
            "if((e.target||'').toLowerCase()==='_blank'||(e.form&&((e.form.target||'').toLowerCase()==='_blank')))throw new Error('禁止打开新窗口');return {x:r.left+r.width/2,y:r.top+r.height/2};",
        );
        assert!(script.contains("禁止打开新窗口"));
        assert!(script.contains("e.form"));
        assert!(script.contains("r.left+r.width/2"));
    }
}
