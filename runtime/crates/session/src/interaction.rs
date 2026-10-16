use super::{InteractionOption, InteractionRequest, ELICIT_DECLINE_ID};
use std::collections::HashSet;

pub(super) fn elicitation_to_interaction(
    pending: &rivo_agent::PendingElicitation,
) -> InteractionRequest {
    let mut body = if pending.message.trim().is_empty() {
        format!("MCP server {} 请求补充信息", pending.server)
    } else {
        pending.message.clone()
    };
    if let Some(props) = pending
        .requested_schema
        .get("properties")
        .and_then(|value| value.as_object())
    {
        if !props.is_empty() {
            let required = pending
                .requested_schema
                .get("required")
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            body.push_str("\n\n请提供以下字段：");
            for (name, spec) in props {
                let ty = spec
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("string");
                let desc = spec
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let flag = if required.contains(name.as_str()) {
                    "必填"
                } else {
                    "可选"
                };
                let separator = if desc.is_empty() { "" } else { "：" };
                body.push_str(&format!("\n- {name}（{ty}，{flag}）{separator}{desc}"));
            }
            if props.len() > 1 {
                body.push_str("\n\n可直接输入 JSON 对象作答。");
            }
        }
    }
    InteractionRequest {
        request_id: pending.request_id.clone(),
        title: format!("MCP server {} 请求输入", pending.server),
        body,
        options: vec![InteractionOption {
            id: ELICIT_DECLINE_ID.into(),
            label: "拒绝".into(),
            description: "不向该 MCP server 提供任何信息".into(),
        }],
        allow_freeform: true,
    }
}

pub(super) fn interaction_outcome(
    value: String,
    schema: &serde_json::Value,
) -> rivo_agent::ElicitationOutcome {
    use rivo_agent::ElicitationOutcome;
    if value == ELICIT_DECLINE_ID {
        return ElicitationOutcome::Decline;
    }
    if value.trim().is_empty() {
        return ElicitationOutcome::Cancel;
    }
    ElicitationOutcome::Accept(elicitation_content(&value, schema))
}

pub(super) fn elicitation_content(value: &str, schema: &serde_json::Value) -> serde_json::Value {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(value) {
        if parsed.is_object() {
            return parsed;
        }
    }
    if let Some(props) = schema.get("properties").and_then(|value| value.as_object()) {
        if props.len() == 1 {
            let (field, spec) = props.iter().next().expect("single-field schema");
            let mut map = serde_json::Map::new();
            map.insert(field.clone(), coerce_scalar(value, spec));
            return serde_json::Value::Object(map);
        }
    }
    serde_json::json!({ "value": value })
}

fn coerce_scalar(value: &str, spec: &serde_json::Value) -> serde_json::Value {
    match spec.get("type").and_then(|value| value.as_str()) {
        Some("integer") => value
            .trim()
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(value.to_owned())),
        Some("number") => value
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(value.to_owned())),
        Some("boolean") => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => serde_json::Value::Bool(true),
            "false" | "no" | "0" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(value.to_owned()),
        },
        _ => serde_json::Value::String(value.to_owned()),
    }
}
