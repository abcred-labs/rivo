use rivo_session::{read_file_config, AuthConfig, ModelDiscoveryCache};
use rivo_test_support::{
    assert_redacted, CliOutput, HttpResponse, ScriptedHttpServer, TestEnvironment,
};
use std::path::Path;

mod support;

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_rivo"))
}

fn fixture() -> TestEnvironment {
    let environment = TestEnvironment::new().unwrap();
    support::write_routing_fixture(
        environment.config_path(),
        environment.workspace(),
        "https://fixture.example/v1",
        "fixture-model",
        "RIVO_FIXTURE_KEY",
    );
    environment
}

fn run(environment: &TestEnvironment, args: &[&str]) -> CliOutput {
    environment
        .command(binary())
        .args(args.iter().copied())
        .run()
        .unwrap()
}

#[test]
fn provider_set_preserves_omitted_fields_and_same_url_neighbor() {
    let environment = fixture();
    let mut file = read_file_config(environment.config_path()).unwrap();
    let provider = file.providers.get_mut("fixture").unwrap();
    provider.enabled = false;
    provider.priority = 83;
    provider.auth = AuthConfig {
        credential: Some("unread-fixture-key".into()),
    };
    provider.discovery.config_fingerprint = Some(rivo_session::provider_connection_fingerprint(
        "fixture", provider,
    ));
    let original = provider.clone();
    file.providers.insert("neighbor".into(), original.clone());
    std::fs::write(
        environment.config_path(),
        toml::to_string_pretty(&file).unwrap(),
    )
    .unwrap();

    for args in [
        vec!["config", "provider", "set", "fixture"],
        vec!["config", "provider", "set", "fixture", "--enabled", "true"],
        vec!["config", "provider", "set", "fixture", "--priority", "0"],
    ] {
        run(&environment, &args).assert_success();
        let saved = read_file_config(environment.config_path()).unwrap();
        let actual = &saved.providers["fixture"];
        assert_eq!(actual.base_url, original.base_url);
        assert_eq!(actual.auth, original.auth);
        assert_eq!(actual.discovery, original.discovery);
        assert_eq!(saved.providers["neighbor"], original);
        if args.last() == Some(&"true") {
            assert!(actual.enabled);
            assert_eq!(actual.priority, 83);
        } else if args.last() == Some(&"0") {
            assert!(actual.enabled);
            assert_eq!(actual.priority, 0);
        } else {
            assert_eq!(actual, &original);
        }
    }
}

#[test]
fn config_catalog_query_formats_preserve_text_and_render_table_and_json() {
    let environment = fixture();

    let text = run(&environment, &["config", "vendor", "list"]);
    text.assert_success();
    assert!(text.stdout.contains("fixture\tFixture\tsource=Local"));

    let table = run(
        &environment,
        &["config", "vendor", "list", "--format", "table"],
    );
    table.assert_success();
    assert!(table.stdout.starts_with('+'));
    assert!(table.stdout.contains("| ID"));
    assert!(table.stdout.contains("| fixture"));

    let json = run(
        &environment,
        &["config", "vendor", "list", "--format", "json"],
    );
    json.assert_success();
    let rows: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert!(rows.is_array());
    assert!(rows
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == "fixture" && row["source"] == "Local"));

    let detail = run(
        &environment,
        &[
            "config",
            "model",
            "show",
            "fixture-model",
            "--format",
            "json",
        ],
    );
    detail.assert_success();
    let detail: serde_json::Value = serde_json::from_str(&detail.stdout).unwrap();
    assert!(detail.is_object());
    assert_eq!(detail["id"], "fixture-model");

    let duplicate = run(
        &environment,
        &[
            "config", "vendor", "list", "--format", "json", "--format", "table",
        ],
    );
    assert_eq!(duplicate.status, 2);
    assert!(duplicate.stderr.contains("--format cannot be repeated"));
}

#[test]
fn model_list_filters_by_vendor_and_batch_enablement_is_partial_and_idempotent() {
    let environment = fixture();

    let table = run(
        &environment,
        &[
            "config", "model", "list", "--state", "enabled", "--vendor", "fixture", "--format",
            "table",
        ],
    );
    table.assert_success();
    assert!(table.stdout.contains("| Enabled"));
    assert!(table.stdout.contains("| fixture-model"));
    assert!(table.stdout.contains("| true"));

    let json = run(
        &environment,
        &[
            "config", "model", "list", "--vendor", "fixture", "--format", "json",
        ],
    );
    json.assert_success();
    let rows: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(rows[0]["vendor"], "fixture");
    assert_eq!(rows[0]["enabled"], true);
    let all_rows: serde_json::Value = serde_json::from_str(
        &run(
            &environment,
            &[
                "config", "model", "list", "--vendor", "fixture", "--state", "all", "--format",
                "json",
            ],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(all_rows, rows);

    let disabled = run(
        &environment,
        &[
            "config",
            "model",
            "disable",
            "fixture-model",
            "missing-model",
            "fixture-model",
        ],
    );
    disabled.assert_success();
    assert!(disabled.stdout.contains("model=fixture-model\tdisabled"));
    assert!(disabled.stdout.contains("model=missing-model\tmissing"));
    assert!(!read_file_config(environment.config_path()).unwrap().models["fixture-model"].enabled);
    let disabled_rows: serde_json::Value = serde_json::from_str(
        &run(
            &environment,
            &[
                "config", "model", "list", "--state", "disabled", "--vendor", "fixture",
                "--format", "json",
            ],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(disabled_rows[0]["enabled"], false);
    assert!(run(
        &environment,
        &["config", "model", "list", "--vendor", "fixture", "--state", "enabled",],
    )
    .stdout
    .is_empty());
    assert!(run(
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
    .status
    .ne(&0));

    let enabled = run(
        &environment,
        &["config", "model", "enable", "fixture-model"],
    );
    enabled.assert_success();
    assert!(read_file_config(environment.config_path()).unwrap().models["fixture-model"].enabled);
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
    .assert_success();

    run(&environment, &["config", "model", "disable", "gpt-5"]).assert_success();
    assert!(!read_file_config(environment.config_path()).unwrap().models["gpt-5"].enabled);
    run(&environment, &["config", "model", "enable", "gpt-5"]).assert_success();
    assert!(!read_file_config(environment.config_path())
        .unwrap()
        .models
        .contains_key("gpt-5"));

    let unknown_vendor = run(
        &environment,
        &["config", "model", "list", "--vendor", "missing"],
    );
    assert_ne!(unknown_vendor.status, 0);

    let before = std::fs::read(environment.config_path()).unwrap();
    for args in [
        vec!["config", "model", "list", "--state"],
        vec!["config", "model", "list", "--state", "pending"],
        vec![
            "config", "model", "list", "--state", "all", "--state", "enabled",
        ],
    ] {
        assert_eq!(run(&environment, &args).status, 2, "{args:?}");
        assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
    }
}

#[test]
fn query_format_defaults_apply_to_queries_and_explicit_values_override() {
    let environment = fixture();
    let server = r#"{"name":"docs","enabled":false,"transport":{"transport":"stdio","program":"fixture-mcp","args":[],"cwd":null},"allowed_capabilities":["tools"],"toolsets":{"read":{"tools":["search"],"risk":"read_only"}}}"#;
    run(&environment, &["config", "set", "mcp.server", server]).assert_success();
    let history_dir = environment.data_dir().join("rivo");
    std::fs::create_dir_all(&history_dir).unwrap();
    let store = rivo_storage::Store::open(history_dir.join("rivo.db")).unwrap();
    store
        .append_event(&rivo_storage::StoredEvent {
            session_id: "query-session".into(),
            turn_id: "query-turn".into(),
            sequence: 1,
            kind: "started".into(),
            body: "{}".into(),
        })
        .unwrap();
    store
        .append_message(&rivo_storage::StoredMessage {
            session_id: "query-session".into(),
            turn_id: "query-turn".into(),
            sequence: 1,
            role: "user".into(),
            body: "query fixture".into(),
        })
        .unwrap();
    drop(store);
    let queries = [
        vec!["config", "vendor", "list"],
        vec!["config", "vendor", "show", "fixture"],
        vec!["config", "model", "list"],
        vec!["config", "model", "show", "fixture-model"],
        vec!["config", "provider", "list"],
        vec!["config", "provider", "show", "fixture"],
        vec![
            "config",
            "route",
            "--vendor",
            "fixture",
            "--model",
            "fixture-model",
        ],
        vec!["sessions", "list"],
        vec!["sessions", "show", "query-session"],
        vec!["mcp", "list"],
        vec!["mcp", "show", "docs"],
    ];
    let formats = ["text", "table", "json"];
    let snapshots = queries
        .iter()
        .map(|query| {
            let legacy = run(&environment, query);
            legacy.assert_success();
            formats.map(|format| {
                let mut args = query.clone();
                args.extend(["--format", format]);
                let output = run(&environment, &args);
                output.assert_success();
                if format == "text" {
                    assert_eq!(output.stdout, legacy.stdout);
                } else if format == "table" {
                    assert!(output.stdout.starts_with('+'));
                } else {
                    let value: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
                    assert_eq!(value.is_array(), query.last() == Some(&"list"));
                }
                output.stdout
            })
        })
        .collect::<Vec<_>>();
    for (format_index, configured) in formats.iter().enumerate() {
        run(&environment, &["config", "set", "query_format", configured]).assert_success();
        let before = std::fs::read(environment.config_path()).unwrap();
        for (query_index, query) in queries.iter().enumerate() {
            let output = run(&environment, query);
            output.assert_success();
            assert_eq!(
                output.stdout, snapshots[query_index][format_index],
                "{query:?}"
            );
            for (explicit_index, explicit) in formats.iter().enumerate() {
                let mut args = query.clone();
                args.extend(["--format", explicit]);
                let output = run(&environment, &args);
                output.assert_success();
                assert_eq!(
                    output.stdout, snapshots[query_index][explicit_index],
                    "{args:?}"
                );
            }
        }
        let implicit = run(&environment, &["config", "vendor"]);
        implicit.assert_success();
        assert_eq!(implicit.stdout, snapshots[0][format_index]);
        let implicit = run(&environment, &["sessions"]);
        implicit.assert_success();
        assert_eq!(implicit.stdout, snapshots[7][format_index]);
        assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
    }
    run(&environment, &["config", "unset", "query_format"]).assert_success();
    for (query, expected) in queries.iter().zip(&snapshots) {
        assert_eq!(run(&environment, query).stdout, expected[0]);
    }
}

#[test]
fn query_format_configuration_is_optional_strict_and_does_not_require_models() {
    let environment = TestEnvironment::new().unwrap();
    std::fs::remove_file(environment.config_path()).unwrap();
    for query in [vec!["sessions", "list"], vec!["mcp", "list"]] {
        let output = run(&environment, &query);
        output.assert_success();
        assert!(output.stdout.is_empty());
    }
    assert!(!environment.config_path().exists());
    std::fs::write(environment.config_path(), "version = 6\n").unwrap();
    for (name, expected) in [
        ("text", rivo_session::QueryFormat::Text),
        ("table", rivo_session::QueryFormat::Table),
        ("json", rivo_session::QueryFormat::Json),
    ] {
        run(&environment, &["config", "set", "query_format", name]).assert_success();
        assert_eq!(
            read_file_config(environment.config_path())
                .unwrap()
                .defaults
                .query_format,
            Some(expected)
        );
        let shown = run(&environment, &["config", "show"]);
        shown.assert_success();
        assert!(shown.stdout.contains(&format!("query_format = {name:?}")));
        run(&environment, &["config", "validate"]).assert_success();
        for query in [vec!["sessions", "list"], vec!["mcp", "list"]] {
            let before = std::fs::read(environment.config_path()).unwrap();
            let output = run(&environment, &query);
            output.assert_success();
            match name {
                "text" => assert!(output.stdout.is_empty()),
                "table" => assert!(output.stdout.starts_with('+')),
                _ => assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&output.stdout).unwrap(),
                    serde_json::json!([])
                ),
            }
            assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
        }
    }
    let before = std::fs::read(environment.config_path()).unwrap();
    for args in [
        vec!["config", "set", "query_format", "jsonl"],
        vec!["config", "set", "query_format", "JSON"],
        vec!["config", "set", "query_format", ""],
        vec!["config", "set", "query_format"],
        vec![
            "config", "route", "--vendor", "fixture", "--format", "json", "--format", "text",
        ],
        vec!["sessions", "list", "--format", "json", "--format", "text"],
        vec!["mcp", "list", "--format", "jsonl"],
        vec!["config", "set", "query_format", "text", "--format", "json"],
    ] {
        assert_eq!(run(&environment, &args).status, 2, "{args:?}");
        assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
    }
    run(&environment, &["config", "unset", "query_format"]).assert_success();
    assert_eq!(
        read_file_config(environment.config_path())
            .unwrap()
            .defaults
            .query_format,
        None
    );
    assert!(!std::fs::read_to_string(environment.config_path())
        .unwrap()
        .contains("query_format"));
    assert!(!environment.data_dir().join("rivo").join("rivo.db").exists());
    std::fs::write(
        environment.config_path(),
        "version = 6\n[defaults]\nquery_format = 'jsonl'\n",
    )
    .unwrap();
    for args in [
        vec!["sessions", "list"],
        vec!["mcp", "list"],
        vec!["config", "validate"],
    ] {
        assert_eq!(run(&environment, &args).status, 2);
    }
    for args in [
        vec!["sessions", "path"],
        vec!["config", "path"],
        vec!["mcp", "--help"],
        vec!["sessions", "--help"],
    ] {
        run(&environment, &args).assert_success();
    }
}

#[test]
fn provider_partial_connection_update_requires_confirmation_and_replaces_auth_reference() {
    let environment = fixture();
    rivo_session::set_credential(
        environment.config_path(),
        "unread-fixture-key",
        "test-only-credential-value",
    )
    .unwrap();
    for (flag, value) in [
        ("--base-url", "https://other.example/v1"),
        ("--credential-name", "unread-fixture-key"),
    ] {
        let before = std::fs::read(environment.config_path()).unwrap();
        let original = read_file_config(environment.config_path())
            .unwrap()
            .providers["fixture"]
            .clone();
        let args = ["config", "provider", "set", "fixture", flag, value];
        let plan = run(&environment, &args);
        assert_eq!(plan.status, 2);
        let plan: serde_json::Value = serde_json::from_str(&plan.stdout).unwrap();
        assert_eq!(plan["provider_impacts"].as_array().unwrap().len(), 1);
        assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
        let mut confirmed = args.to_vec();
        confirmed.extend(["--confirm", "wrong"]);
        assert_eq!(run(&environment, &confirmed).status, 2);
        assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
        *confirmed.last_mut().unwrap() = plan["confirmation_token"].as_str().unwrap();
        run(&environment, &confirmed).assert_success();
        let saved = read_file_config(environment.config_path())
            .unwrap()
            .providers["fixture"]
            .clone();
        let mut expected = original;
        expected.discovery = ModelDiscoveryCache::default();
        match flag {
            "--base-url" => expected.base_url = value.into(),
            "--credential-name" => {
                expected.auth = AuthConfig {
                    credential: Some(value.into()),
                }
            }
            _ => unreachable!(),
        }
        assert_eq!(saved, expected);
    }
}

#[test]
fn provider_set_rejects_incomplete_creation_and_invalid_updates_without_writes() {
    let environment = fixture();
    let before = std::fs::read(environment.config_path()).unwrap();
    for args in [
        vec!["new"],
        // Provider 不再携带协议；旧参数被拒绝而不是被静默忽略。
        vec![
            "new",
            "--protocol",
            "openai_responses",
            "--base-url",
            "http://127.0.0.1:12345/v1",
        ],
        vec!["fixture", "--base-url", ""],
        vec!["fixture", "--protocol", ""],
        vec!["fixture", "--priority", "1001"],
        vec!["fixture", "--enabled", "invalid"],
        vec!["fixture", "--credential-name", ""],
    ] {
        let mut command = vec!["config", "provider", "set"];
        command.extend(args);
        assert_eq!(run(&environment, &command).status, 2);
        assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
    }
    run(
        &environment,
        &[
            "config",
            "provider",
            "set",
            "new",
            "--base-url",
            "http://127.0.0.1:12345/v1",
        ],
    )
    .assert_success();
    let created = read_file_config(environment.config_path())
        .unwrap()
        .providers["new"]
        .clone();
    assert!(created.enabled);
    assert_eq!(created.priority, 0);
    assert_eq!(created.auth, AuthConfig::default());
    assert_eq!(created.discovery, ModelDiscoveryCache::default());
}

#[test]
fn removed_connection_environment_fails_before_run_side_effects() {
    let environment = fixture();
    let before = std::fs::read(environment.config_path()).unwrap();
    for key in [
        "RIVO_PROVIDER_PROFILE",
        "RIVO_PROVIDER",
        "RIVO_BASE_URL",
        "RIVO_API_KEY_ENV",
        "RIVO_CREDENTIAL_NAME",
    ] {
        let output = environment
            .command(binary())
            .args([
                "run",
                "--vendor",
                "fixture",
                "--model",
                "fixture-model",
                "--auto-provider",
                "--prompt",
                "task",
            ])
            .env(key, "sensitive-fixture-value")
            .run()
            .unwrap();
        assert_eq!(output.status, 2);
        assert!(output.stderr.contains(key));
        assert_redacted(
            &format!("{}{}", output.stdout, output.stderr),
            &["sensitive-fixture-value"],
        );
        assert_eq!(std::fs::read(environment.config_path()).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(environment.data_dir()).unwrap().count(),
            0
        );
    }
}

#[test]
fn selection_environment_and_provider_key_reference_work_through_real_binary() {
    let environment = fixture();
    let server = ScriptedHttpServer::start(vec![HttpResponse::sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"fixture answer\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
    )]).unwrap();
    support::write_routing_fixture(
        environment.config_path(),
        environment.workspace(),
        &server.base_url(),
        "fixture-model",
        "RIVO_FEAT029_CUSTOM_KEY",
    );
    let credential = environment
        .command(binary())
        .args([
            "config",
            "credentials",
            "set",
            "fixture-key",
            "--from-env",
            "RIVO_FEAT029_CUSTOM_KEY",
        ])
        .env("RIVO_FEAT029_CUSTOM_KEY", "synthetic-provider-secret")
        .run()
        .unwrap();
    credential.assert_success();
    let mut file = read_file_config(environment.config_path()).unwrap();
    file.defaults.vendor = None;
    file.defaults.model = None;
    let provider = file.providers.get_mut("fixture").unwrap();
    provider.auth = AuthConfig {
        credential: Some("fixture-key".into()),
    };
    provider.discovery.config_fingerprint = Some(rivo_session::provider_connection_fingerprint(
        "fixture", provider,
    ));
    std::fs::write(
        environment.config_path(),
        toml::to_string_pretty(&file).unwrap(),
    )
    .unwrap();
    let output = environment
        .command(binary())
        .args(["run", "--format", "jsonl", "--prompt", "task"])
        .env("RIVO_VENDOR", "fixture")
        .env("RIVO_MODEL", "fixture-model")
        .env("RIVO_PROVIDER_ID", "fixture")
        .env("RIVO_FEAT029_CUSTOM_KEY", "synthetic-provider-secret")
        .run()
        .unwrap();
    output.assert_success();
    assert_redacted(
        &format!("{}{}", output.stdout, output.stderr),
        &["synthetic-provider-secret"],
    );
    assert!(output.stdout.contains("fixture answer"));
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer synthetic-provider-secret"
    );
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(body["model"], "fixture-model");
}

/// 自动化测试可以不落盘地注入整份主配置；磁盘上的配置文件既不读也不写。
#[test]
fn injected_config_content_replaces_the_file_and_leaves_it_untouched() {
    let environment = fixture();
    let injected = {
        let mut file = read_file_config(environment.config_path()).unwrap();
        file.defaults.model = Some("injected-model".into());
        file.models.insert(
            "injected-model".into(),
            file.models["fixture-model"].clone(),
        );
        toml::to_string_pretty(&file).unwrap()
    };
    let on_disk = std::fs::read(environment.config_path()).unwrap();

    let show = environment
        .command(binary())
        .env("RIVO_CONFIG_CONTENT", &injected)
        .args(["config", "show"])
        .run()
        .unwrap();
    show.assert_success();
    assert!(
        show.stdout.contains("injected-model"),
        "config show ignored the injected document:\n{}",
        show.stdout
    );

    environment
        .command(binary())
        .env("RIVO_CONFIG_CONTENT", &injected)
        .args(["config", "validate"])
        .run()
        .unwrap()
        .assert_success();

    assert_eq!(
        std::fs::read(environment.config_path()).unwrap(),
        on_disk,
        "injected run modified the configuration file"
    );
}

#[test]
fn injected_config_content_works_without_a_configuration_file() {
    let environment = fixture();
    let injected = std::fs::read_to_string(environment.config_path()).unwrap();
    std::fs::remove_file(environment.config_path()).unwrap();

    let show = environment
        .command(binary())
        .env("RIVO_CONFIG_CONTENT", &injected)
        .args(["config", "show"])
        .run()
        .unwrap();
    show.assert_success();
    assert!(show.stdout.contains("fixture-model"), "{}", show.stdout);
    assert!(!environment.config_path().exists());
}

#[test]
fn injected_config_content_rejects_configuration_writes() {
    let environment = fixture();
    let injected = std::fs::read_to_string(environment.config_path()).unwrap();
    let on_disk = std::fs::read(environment.config_path()).unwrap();

    for args in [
        vec!["config", "set", "query_format", "json"],
        vec!["config", "unset", "query_format"],
        vec!["config", "provider", "set", "fixture", "--priority", "7"],
    ] {
        let output = environment
            .command(binary())
            .env("RIVO_CONFIG_CONTENT", &injected)
            .args(args.iter().copied())
            .run()
            .unwrap();
        output.assert_failure();
        assert!(
            output.stderr.contains("RIVO_CONFIG_CONTENT"),
            "{args:?} failed without naming the injected source:\n{}",
            output.stderr
        );
    }

    assert_eq!(
        std::fs::read(environment.config_path()).unwrap(),
        on_disk,
        "a rejected write still modified the configuration file"
    );
}

#[test]
fn injected_config_content_rejects_invalid_documents_without_echoing_them() {
    let environment = fixture();
    let secret = "fixture-injected-canary-key";
    for injected in [
        format!("version = 6\n[defaults]\nunknown_field = \"{secret}\"\n"),
        format!("version = 4\n[defaults]\nmodel = \"{secret}\"\n"),
        format!("not = toml = \"{secret}\"\n"),
    ] {
        let output = environment
            .command(binary())
            .env("RIVO_CONFIG_CONTENT", &injected)
            .args(["config", "show"])
            .run()
            .unwrap();
        output.assert_failure();
        assert_redacted(&output.stderr, &[secret]);
        assert_redacted(&output.stdout, &[secret]);
    }
}
