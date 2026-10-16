use reqwest::{Client, Url};
use rivo_core::{EvidenceRef, EvidenceSource};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::net::lookup_host;

#[derive(Debug, Error)]
pub enum WebError {
    #[error("Web 能力未启用")]
    Disabled,
    #[error("URL 被安全边界拒绝: {0}")]
    UnsafeUrl(String),
    #[error("网络错误: {0}")]
    Network(String),
    #[error("响应类型不支持: {0}")]
    UnsupportedMime(String),
    #[error("响应超过 2 MiB 限制")]
    TooLarge,
    #[error("重定向次数超过限制")]
    TooManyRedirects,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchRequest {
    pub url: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchResult {
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub title: Option<String>,
    pub content: String,
    pub content_type: String,
    pub truncated: bool,
    pub evidence: EvidenceRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchRequest {
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchItem {
    pub rank: usize,
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchResult {
    pub items: Vec<WebSearchItem>,
    pub query: String,
    pub provider: String,
}

pub struct BraveSearchClient {
    client: Client,
    pub network_enabled: bool,
    endpoint: String,
}

impl BraveSearchClient {
    pub fn new(network_enabled: bool) -> Result<Self, WebError> {
        Ok(Self {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| WebError::Network(e.to_string()))?,
            network_enabled,
            endpoint: "https://api.search.brave.com/res/v1/web/search".into(),
        })
    }
    pub fn with_endpoint(
        network_enabled: bool,
        endpoint: impl Into<String>,
    ) -> Result<Self, WebError> {
        let mut client = Self::new(network_enabled)?;
        client.endpoint = endpoint.into();
        Ok(client)
    }
    pub async fn search(&self, request: WebSearchRequest) -> Result<WebSearchResult, WebError> {
        if !self.network_enabled {
            return Err(WebError::Disabled);
        }
        if request.query.chars().count() > 500 {
            return Err(WebError::UnsafeUrl("搜索 query 超过 500 字符".into()));
        }
        let key = std::env::var("BRAVE_SEARCH_API_KEY")
            .map_err(|_| WebError::Network("缺少 BRAVE_SEARCH_API_KEY".into()))?;
        let response = self
            .client
            .get(&self.endpoint)
            .header("X-Subscription-Token", key)
            .query(&[("q", request.query.as_str()), ("count", "10")])
            .send()
            .await
            .map_err(|e| WebError::Network(format!("{e:?}")))?;
        if !response.status().is_success() {
            return Err(WebError::Network(format!(
                "Brave 返回 HTTP {}",
                response.status()
            )));
        }
        let (body, truncated) = read_response_limited(response, 1024 * 1024).await?;
        if truncated {
            return Err(WebError::TooLarge);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| WebError::Network(e.to_string()))?;
        let items = value
            .pointer("/web/results")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .take(10)
            .enumerate()
            .filter_map(|(index, item)| {
                Some(WebSearchItem {
                    rank: index + 1,
                    title: item.get("title")?.as_str()?.into(),
                    url: item.get("url")?.as_str()?.into(),
                    snippet: item
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    provider: "brave".into(),
                })
            })
            .collect();
        Ok(WebSearchResult {
            items,
            query: request.query,
            provider: "brave".into(),
        })
    }
}

pub struct WebFetcher {
    client: Client,
    pub network_enabled: bool,
    pub allow_http_localhost: bool,
    pub unrestricted: bool,
}

impl WebFetcher {
    pub fn new(network_enabled: bool) -> Result<Self, WebError> {
        Ok(Self {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| WebError::Network(e.to_string()))?,
            network_enabled,
            allow_http_localhost: false,
            unrestricted: false,
        })
    }
    pub fn allow_local_fixture(mut self) -> Self {
        self.allow_http_localhost = true;
        self
    }
    /// Full Control only: policy has already made the explicit decision to allow
    /// non-public HTTP targets. Credentials in URLs remain forbidden.
    pub fn unrestricted(mut self) -> Self {
        self.unrestricted = true;
        self
    }
    pub async fn fetch(&self, request: WebFetchRequest) -> Result<WebFetchResult, WebError> {
        if !self.network_enabled {
            return Err(WebError::Disabled);
        }
        let requested = Url::parse(&request.url).map_err(|e| WebError::UnsafeUrl(e.to_string()))?;
        self.validate_url(&requested).await?;
        let mut url = requested.clone();
        let mut response = None;
        for _ in 0..=5 {
            self.validate_url(&url).await?;
            let current = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| WebError::Network(format!("{e:?}")))?;
            self.validate_peer(&current, &url)?;
            if current.status().is_redirection() {
                let Some(location) = current
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                else {
                    return Err(WebError::Network("重定向缺少 Location".into()));
                };
                url = current
                    .url()
                    .join(location)
                    .map_err(|e| WebError::UnsafeUrl(e.to_string()))?;
                continue;
            }
            response = Some(current);
            break;
        }
        let response = response.ok_or(WebError::TooManyRedirects)?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        if !["text/", "text/html", "application/json"]
            .iter()
            .any(|prefix| content_type.starts_with(prefix))
        {
            return Err(WebError::UnsupportedMime(content_type));
        }
        let limit = request
            .max_bytes
            .unwrap_or(2 * 1024 * 1024)
            .min(2 * 1024 * 1024);
        let (bytes, truncated) = read_response_limited(response, limit).await?;
        let raw_content = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).into_owned();
        let title = extract_title(&raw_content);
        let content = if content_type.starts_with("text/html") {
            extract_readable_text(&raw_content)
        } else {
            raw_content
        };
        let evidence = EvidenceRef {
            id: format!("web-{}", sha256(url.as_ref())),
            source: EvidenceSource::Web {
                url: url.to_string(),
                retrieved_at: chrono_like_now(),
            },
            excerpt: evidence_excerpt(&content),
            truncated,
        };
        Ok(WebFetchResult {
            requested_url: requested.to_string(),
            final_url: url.to_string(),
            status,
            title,
            content,
            content_type,
            truncated,
            evidence,
        })
    }
    async fn validate_url(&self, url: &Url) -> Result<(), WebError> {
        if !matches!(url.scheme(), "https" | "http")
            || url.username() != ""
            || url.password().is_some()
        {
            return Err(WebError::UnsafeUrl(url.to_string()));
        }
        if url.scheme() == "http"
            && !self.unrestricted
            && !(self.allow_http_localhost
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1")))
        {
            return Err(WebError::UnsafeUrl("生产环境只允许 HTTPS".into()));
        }
        let host = url
            .host_str()
            .ok_or_else(|| WebError::UnsafeUrl("缺少主机名".into()))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| WebError::UnsafeUrl("缺少端口".into()))?;
        let addresses = lookup_host((host, port))
            .await
            .map_err(|e| WebError::Network(format!("{e:?}")))?;
        let local_fixture = self.allow_http_localhost
            && url.scheme() == "http"
            && matches!(host, "localhost" | "127.0.0.1" | "::1");
        for address in addresses {
            if !self.unrestricted && !local_fixture && is_private(address.ip()) {
                return Err(WebError::UnsafeUrl(format!(
                    "解析到受限地址 {}",
                    address.ip()
                )));
            }
        }
        Ok(())
    }

    fn validate_peer(&self, response: &reqwest::Response, url: &Url) -> Result<(), WebError> {
        let local_fixture = self.allow_http_localhost
            && url.scheme() == "http"
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
        if let Some(address) = response.remote_addr() {
            if !self.unrestricted && !local_fixture && is_private(address.ip()) {
                return Err(WebError::UnsafeUrl(format!(
                    "连接对端为受限地址 {}",
                    address.ip()
                )));
            }
        }
        Ok(())
    }
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1])
        }
        IpAddr::V6(ip) => {
            ip.to_ipv4_mapped()
                .is_some_and(|mapped| is_private(IpAddr::V4(mapped)))
                || ip.is_loopback()
                || ip.is_unspecified()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

async fn read_response_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), WebError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| WebError::Network(e.to_string()))?
    {
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            return Ok((bytes, true));
        }
    }
    Ok((bytes, false))
}

fn extract_title(content: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    let start = lower.find("<title>")? + 7;
    let end = lower[start..].find("</title>")? + start;
    Some(content[start..end].trim().to_owned())
}

fn extract_readable_text(content: &str) -> String {
    let mut text = content.to_owned();
    for tag in ["script", "style", "nav", "noscript"] {
        let lower = text.to_ascii_lowercase();
        let mut cursor = 0;
        let mut ranges = Vec::new();
        while let Some(start_offset) = lower[cursor..].find(&format!("<{tag}")) {
            let start = cursor + start_offset;
            let Some(end_offset) = lower[start..].find(&format!("</{tag}>")) else {
                break;
            };
            ranges.push((start, start + end_offset + tag.len() + 3));
            cursor = start + end_offset + tag.len() + 3;
        }
        for (start, end) in ranges.into_iter().rev() {
            text.replace_range(start..end.min(text.len()), " ");
        }
    }
    let mut output = String::with_capacity(text.len());
    let mut in_tag = false;
    for character in text.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn sha256(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn evidence_excerpt(value: &str) -> String {
    value.chars().take(4096).collect()
}

fn chrono_like_now() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        day_seconds / 3_600,
        (day_seconds % 3_600) / 60,
        day_seconds % 60,
        elapsed.subsec_millis()
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        .div_euclid(365);
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2).div_euclid(153);
    let day = day_of_year - (153 * month_part + 2).div_euclid(5) + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn private_address_classifier_covers_ipv4_ipv6_and_mapped_ranges() {
        assert!(is_private(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_private(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(!is_private(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(is_private(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private(IpAddr::V6("fc00::1".parse().unwrap())));
        assert!(is_private(IpAddr::V6("fe80::1".parse().unwrap())));
        assert!(is_private(IpAddr::V6("::ffff:127.0.0.1".parse().unwrap())));
        assert!(!is_private(IpAddr::V6(
            "2606:4700:4700::1111".parse().unwrap()
        )));
    }

    #[tokio::test]
    async fn local_fixture_extracts_readable_html_and_title() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await;
            let body = "<html><title>Fixture</title><nav>noise</nav><main>Hello <b>world</b></main><script>secret()</script></html>";
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let fetcher = WebFetcher::new(true).unwrap().allow_local_fixture();
        let result = fetcher
            .fetch(WebFetchRequest {
                url: format!("http://{address}/fixture"),
                max_bytes: None,
            })
            .await
            .unwrap();
        assert_eq!(result.title.as_deref(), Some("Fixture"));
        assert!(result.content.contains("Hello world"));
        assert!(!result.content.contains("secret"));
        assert!(!result.content.contains("noise"));
        match result.evidence.source {
            EvidenceSource::Web { retrieved_at, .. } => {
                assert!(retrieved_at.ends_with('Z'));
                assert!(retrieved_at.contains('T'));
            }
            _ => panic!("expected web evidence"),
        }
    }

    #[tokio::test]
    async fn brave_adapter_parses_mock_results_without_external_network() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await;
            let body = r#"{"web":{"results":[{"title":"One","url":"https://example.com","description":"snippet"}]}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        std::env::set_var("BRAVE_SEARCH_API_KEY", "canary-not-persisted");
        let client =
            BraveSearchClient::with_endpoint(true, format!("http://{address}/search")).unwrap();
        let result = client
            .search(WebSearchRequest {
                query: "query".into(),
            })
            .await
            .unwrap();
        assert_eq!(result.items[0].title, "One");
        std::env::remove_var("BRAVE_SEARCH_API_KEY");
    }

    #[test]
    fn web_evidence_excerpt_is_bounded() {
        let value = "x".repeat(8 * 1024);
        assert_eq!(evidence_excerpt(&value).chars().count(), 4096);
    }
}
