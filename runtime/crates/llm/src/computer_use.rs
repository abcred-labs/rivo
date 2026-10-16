//! Provider-specific Computer Use wire handling.
//!
//! The Windows driver lives in `rivo-tools`; this module only translates the
//! provider envelopes to and from the backend-neutral action model. Keeping
//! this boundary explicit prevents provider JSON from leaking into policy or
//! desktop execution code.

use rivo_tools::computer::{from_provider_action, ComputerAction, ComputerError, Screenshot};
use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ComputerWireError {
    #[error("computer response is missing a computer call")]
    MissingCall,
    #[error("computer response has invalid action: {0}")]
    InvalidAction(#[from] ComputerError),
    #[error("computer response has invalid wire shape: {0}")]
    InvalidShape(String),
}

/// Parse an OpenAI Responses `computer_call` output item.
pub fn parse_openai_responses_call(
    response: &Value,
) -> Result<(String, Vec<ComputerAction>), ComputerWireError> {
    let item = response
        .get("output")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("computer_call"))
        })
        .ok_or(ComputerWireError::MissingCall)?;
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| ComputerWireError::InvalidShape("computer_call 缺少 call_id".into()))?
        .to_owned();
    let actions = item
        .get("actions")
        .and_then(Value::as_array)
        .ok_or_else(|| ComputerWireError::InvalidShape("computer_call 缺少 actions".into()))?
        .iter()
        .map(from_provider_action)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((call_id, actions))
}

/// Parse an Anthropic Messages computer tool `tool_use.input` object.
pub fn parse_anthropic_tool_input(input: &Value) -> Result<ComputerAction, ComputerWireError> {
    from_provider_action(input).map_err(ComputerWireError::InvalidAction)
}

/// Return the tool declaration for the selected provider.
pub fn tool_definition(provider: crate::ProviderKind) -> Value {
    match provider {
        crate::ProviderKind::OpenAiResponses => json!({"type":"computer"}),
        crate::ProviderKind::ClaudeMessages => json!({
            "type":"computer_20250124",
            "name":"computer",
            "display_width_px":1920,
            "display_height_px":1080
        }),
        crate::ProviderKind::OpenAiChatCompletions => json!({
            "type":"function",
            "function": {
                "name":"computer",
                "description":"Execute one guarded desktop action and return an observation.",
                "parameters": function_tool_parameters()
            }
        }),
    }
}

// Keep the provider-neutral declaration aligned with ComputerRegisteredTool.
// The cross-crate contract test in rivo-agent checks the complete schema.
fn function_tool_parameters() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {
                "type": "string",
                "enum": [
                    "screenshot", "list_windows", "ui_snapshot", "focus_window",
                    "mouse_move", "click", "double_click", "drag", "scroll", "type",
                    "key", "wait"
                ]
            },
            "actions": {"type": "array", "maxItems": 16, "items": {"type": "object"}},
            "window_id": {"type": ["string", "null"]},
            "max_nodes": {"type": "integer", "minimum": 1, "maximum": 500},
            "x": {"type": "integer"},
            "y": {"type": "integer"},
            "from_x": {"type": "integer"},
            "from_y": {"type": "integer"},
            "to_x": {"type": "integer"},
            "to_y": {"type": "integer"},
            "delta": {"type": "integer"},
            "button": {"type": "string", "enum": ["left", "right", "middle"]},
            "text": {"type": "string", "maxLength": 16384},
            "key": {"type": "string", "maxLength": 64},
            "modifiers": {"type": "array", "maxItems": 4, "items": {"type": "string"}},
            "timeout_ms": {"type": "integer", "minimum": 50, "maximum": 30000}
        },
        "required": ["action"]
    })
}

pub fn function_tool_definition() -> Value {
    json!({
        "type":"function", "name":"computer",
        "description":"Execute one guarded desktop action and return an observation.",
        "parameters": function_tool_parameters()
    })
}

/// Encode a screenshot as the provider's tool-result shape.
pub fn screenshot_result(
    provider: crate::ProviderKind,
    call_id: &str,
    screenshot: &Screenshot,
) -> Value {
    let data_url = format!(
        "data:{};base64,{}",
        screenshot.media_type, screenshot.bytes_base64
    );
    match provider {
        crate::ProviderKind::OpenAiResponses => json!({
            "type":"computer_call_output", "call_id":call_id,
            "output":{"type":"computer_screenshot", "image_url":data_url, "detail":"original"}
        }),
        crate::ProviderKind::ClaudeMessages => json!({
            "type":"tool_result", "tool_use_id":call_id,
            "content":[{"type":"image", "source":{"type":"base64", "media_type":screenshot.media_type, "data":screenshot.bytes_base64}}]
        }),
        crate::ProviderKind::OpenAiChatCompletions => json!({
            "role":"tool", "tool_call_id":call_id,
            "content":[{"type":"image_url", "image_url":{"url":data_url}}]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_batched_actions() {
        let response = json!({"output":[{"type":"computer_call","call_id":"call_1","actions":[
            {"type":"click","button":"left","x":4,"y":5},
            {"type":"type","text":"ok"}
        ]}]});
        let (id, actions) = parse_openai_responses_call(&response).unwrap();
        assert_eq!(id, "call_1");
        assert!(matches!(
            actions[0],
            ComputerAction::Click { x: 4, y: 5, .. }
        ));
        assert!(matches!(actions[1], ComputerAction::Type { .. }));
    }

    #[test]
    fn parses_provider_scroll_directions_and_rejects_invalid_coordinates() {
        let response = json!({"output":[{"type":"computer_call","call_id":"scroll-1","actions":[
            {"type":"scroll","x":800,"y":500,"scroll_y":240},
            {"type":"scroll","coordinate":[800,500],"scroll_y":-240},
            {"action":"scroll","x":800,"y":500,"delta":-120}
        ]}]});
        let (_, actions) = parse_openai_responses_call(&response).unwrap();
        assert_eq!(
            actions,
            [-240, 240, -120]
                .into_iter()
                .map(|delta| ComputerAction::Scroll {
                    x: 800,
                    y: 500,
                    delta
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            parse_anthropic_tool_input(
                &json!({"action":"scroll","coordinate":[800,500],"scroll_y":240})
            )
            .unwrap(),
            ComputerAction::Scroll {
                x: 800,
                y: 500,
                delta: -240
            }
        );

        let invalid = json!({"type":"scroll","x":"800","y":500,"scroll_y":240});
        let response = json!({"output":[{"type":"computer_call","call_id":"scroll-2","actions":[invalid.clone()]}]});
        assert!(matches!(
            parse_openai_responses_call(&response),
            Err(ComputerWireError::InvalidAction(ComputerError::Invalid(_)))
        ));
        assert!(matches!(
            parse_anthropic_tool_input(&invalid),
            Err(ComputerWireError::InvalidAction(ComputerError::Invalid(_)))
        ));
    }

    #[test]
    fn encodes_provider_specific_screenshot_results() {
        let screenshot = Screenshot {
            media_type: "image/png".into(),
            bytes_base64: "YWJj".into(),
            width: 1,
            height: 1,
            window_id: None,
        };
        let result = screenshot_result(crate::ProviderKind::OpenAiResponses, "c", &screenshot);
        assert_eq!(result["output"]["type"], "computer_screenshot");
        assert_eq!(result["output"]["image_url"], "data:image/png;base64,YWJj");
    }
}
