use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// 仅使用临时目录和 loopback HTTP fixture；不读取本机配置、凭据或 live target。
async fn provider_fixture(failures: Vec<bool>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        for fail in failures {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0, "request closed before its body");
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    assert!(headers.starts_with("POST /v1/chat/completions "));
                    // OpenAI transport 即使无认证也可能发送空 Bearer；不允许任何凭据值。
                    for line in headers.lines() {
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("authorization") {
                                assert_eq!(value.trim(), "Bearer");
                            }
                        }
                    }
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        let body: serde_json::Value =
                            serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
                        assert_eq!(body["model"], "gpt-5");
                        break;
                    }
                }
            }
            let (status, content_type, body) = if fail {
                (
                    "400 Bad Request",
                    "application/json",
                    r#"{"error":{"message":"fixture failure"}}"#,
                )
            } else {
                ("200 OK", "text/event-stream", "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (url, task)
}

async fn next_turn(events: &mut SessionEventStream) -> Vec<AgentEvent> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut collected = Vec::new();
        loop {
            let event = events.next().await.expect("turn event").event;
            let terminal = matches!(
                event,
                AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
            );
            collected.push(event);
            if terminal {
                return collected;
            }
        }
    })
    .await
    .expect("fixture turn must finish")
}

fn assert_started(
    events: &[AgentEvent],
    provider_id: &str,
    expected: ProviderSelection,
) -> ResolvedRoute {
    let AgentEvent::Started {
        provider,
        model,
        selection: Some(selection),
        route: Some(route),
        ..
    } = &events[0]
    else {
        panic!("missing authoritative Started: {events:?}");
    };
    assert_eq!(selection.provider, expected);
    assert_eq!(selection.vendor, route.vendor);
    assert_eq!(selection.model, route.model);
    assert_eq!(model, &route.model);
    assert_eq!(provider, &route.protocol);
    assert_eq!(route.provider_id, provider_id);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Started { .. }))
            .count(),
        1
    );
    route.clone()
}

#[tokio::test]
async fn started_route_auto_re_resolves_explicit_stays_explicit_and_retry_freezes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let (a_url, a_server) = provider_fixture(vec![false, false]).await;
    let (b_url, b_server) = provider_fixture(vec![true, false, false]).await;
    let mut file = FileConfig::default();
    file.defaults.vendor = Some("openai".into());
    file.defaults.model = Some("gpt-5".into());
    file.defaults.subagents_enabled = false;
    file.defaults.budget.max_retries = 0;
    // fixture 只模拟 Chat Completions；通过本地模型覆盖固定协议。
    let mut gpt5 = model_routing::read_default_catalog().unwrap().models["gpt-5"].clone();
    gpt5.supported_protocols = vec![ProviderKind::OpenAiChatCompletions];
    file.models.insert("gpt-5".into(), gpt5);
    for (id, url) in [("a", a_url), ("b", b_url)] {
        let mut provider = ProviderConfig {
            base_url: url,
            auth: AuthConfig::default(),
            enabled: true,
            priority: 100,
            discovery: ModelDiscoveryCache::default(),
        };
        provider.discovery = ModelDiscoveryCache {
            config_fingerprint: Some(provider_connection_fingerprint(id, &provider)),
            supported: Some(true),
            models: vec!["gpt-5".into()],
            last_checked_at: Some(1),
            last_error: None,
        };
        file.providers.insert(id.into(), provider);
    }
    std::fs::write(&path, toml::to_string_pretty(&file).unwrap()).unwrap();
    let config = ResolvedConfig::resolve_at(
        file,
        &BTreeMap::new(),
        ConfigOverrides {
            workspace: Some(dir.path().into()),
            ..ConfigOverrides::default()
        },
        path.clone(),
    )
    .unwrap();
    let active = build_active_llm(config.clone()).unwrap();
    let session = Arc::new(Session {
        config,
        active_llm: Arc::new(Mutex::new(Some(active))),
        session_id: "started-route-fixture".into(),
        storage: None,
        workspace_lease: None,
        session_messenger: Mutex::new(None),
    });

    // 单向提交也携带权威快照；正常新 turn 随后必须重新解析 Auto。
    let mut legacy = session
        .handle(Intent::SubmitTask {
            text: "first".into(),
        })
        .unwrap();
    let first = next_turn(&mut legacy).await;
    assert_started(&first, "a", ProviderSelection::Auto);
    assert!(matches!(first.last(), Some(AgentEvent::FinalAnswer { .. })));
    let mut draft = read_model_routing_draft(&path).unwrap();
    draft.config.providers.get_mut("b").unwrap().priority = 200;
    commit_model_routing_draft(&path, &draft.revision, &draft.config).unwrap();

    let (sender, mut events) = session.clone().channel();
    sender
        .send(Intent::SubmitTask {
            text: "second".into(),
        })
        .await
        .unwrap();
    let failed = next_turn(&mut events).await;
    let frozen = assert_started(&failed, "b", ProviderSelection::Auto);
    let retry_id = failed
        .iter()
        .find_map(|event| match event {
            AgentEvent::RetryAvailable { step_id } => Some(step_id.clone()),
            _ => None,
        })
        .expect("provider failure offers retry");

    // 改优先级后重试仍请求 B（fixture 精确限定各 Provider 的请求数）。
    let mut draft = read_model_routing_draft(&path).unwrap();
    draft.config.providers.get_mut("a").unwrap().priority = 300;
    commit_model_routing_draft(&path, &draft.revision, &draft.config).unwrap();
    sender
        .send(Intent::RetryStep { step_id: retry_id })
        .await
        .unwrap();
    let retried = next_turn(&mut events).await;
    assert_eq!(
        assert_started(&retried, "b", ProviderSelection::Auto),
        frozen
    );
    assert!(matches!(
        retried.last(),
        Some(AgentEvent::FinalAnswer { .. })
    ));

    sender
        .send(Intent::SubmitTask {
            text: "third".into(),
        })
        .await
        .unwrap();
    let third = next_turn(&mut events).await;
    assert_started(&third, "a", ProviderSelection::Auto);
    assert!(matches!(third.last(), Some(AgentEvent::FinalAnswer { .. })));

    let explicit = ProviderSelection::Explicit {
        provider_id: "b".into(),
    };
    sender
        .send(Intent::SelectModel {
            selection: ModelSelection {
                vendor: "openai".into(),
                model: "gpt-5".into(),
                provider: explicit.clone(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        events.next().await.unwrap().event,
        AgentEvent::ModelSelected { .. }
    ));
    sender
        .send(Intent::SubmitTask {
            text: "explicit".into(),
        })
        .await
        .unwrap();
    let explicit_turn = next_turn(&mut events).await;
    assert_started(&explicit_turn, "b", explicit);
    assert!(matches!(
        explicit_turn.last(),
        Some(AgentEvent::FinalAnswer { .. })
    ));
    a_server.await.unwrap();
    b_server.await.unwrap();
    drop(sender);
}

#[test]
fn started_route_fields_default_to_none_for_older_events() {
    let event: AgentEvent = serde_json::from_value(serde_json::json!({
        "type": "started", "provider": "openai_responses", "model": "gpt-5",
        "turn_id": "older", "capability_profile": "general", "policy_profile": "strict"
    }))
    .unwrap();
    assert!(matches!(
        event,
        AgentEvent::Started {
            selection: None,
            route: None,
            ..
        }
    ));
    assert_eq!(
        serde_json::from_value::<AgentEvent>(serde_json::to_value(&event).unwrap()).unwrap(),
        event
    );
}
