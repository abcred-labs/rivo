use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

#[cfg(windows)]
mod windows_backend;
#[cfg(windows)]
pub use windows_backend::WindowsComputerBackend;

pub const MAX_TEXT_CHARS: usize = 16 * 1024;
pub const MAX_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerAction {
    Screenshot {
        window_id: Option<String>,
    },
    ListWindows,
    UiSnapshot {
        window_id: String,
        max_nodes: usize,
    },
    FocusWindow {
        window_id: String,
    },
    MouseMove {
        x: i32,
        y: i32,
    },
    Click {
        x: i32,
        y: i32,
        button: MouseButton,
    },
    DoubleClick {
        x: i32,
        y: i32,
        button: MouseButton,
    },
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
    Scroll {
        x: i32,
        y: i32,
        /// Windows wheel delta: positive values scroll upward.
        delta: i32,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
        modifiers: Vec<String>,
    },
    Wait {
        timeout_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub process: String,
    pub process_id: u32,
    pub class_name: String,
    pub sensitive: bool,
    pub bounds: [i32; 4],
    pub foreground: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screenshot {
    pub media_type: String,
    pub bytes_base64: String,
    pub width: u32,
    pub height: u32,
    pub window_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiNode {
    pub runtime_id: String,
    pub name: String,
    pub control_type: String,
    pub bounds: [i32; 4],
    pub enabled: bool,
    pub password: bool,
}

#[derive(Debug, Error)]
pub enum ComputerError {
    #[error("computer action is unsupported on this platform")]
    Unsupported,
    #[error("computer action denied: {0}")]
    Denied(String),
    #[error("invalid computer action: {0}")]
    Invalid(String),
    #[error("computer backend failed: {0}")]
    Backend(String),
}

#[async_trait]
pub trait ComputerBackend: Send + Sync {
    async fn execute(&self, action: ComputerAction) -> Result<Value, ComputerError>;
}

#[derive(Debug, Default)]
pub struct UnavailableComputerBackend;
#[async_trait]
impl ComputerBackend for UnavailableComputerBackend {
    async fn execute(&self, _action: ComputerAction) -> Result<Value, ComputerError> {
        Err(ComputerError::Unsupported)
    }
}

#[derive(Debug, Default)]
pub struct MockComputerBackend {
    actions: std::sync::Mutex<Vec<ComputerAction>>,
}
impl MockComputerBackend {
    pub fn actions(&self) -> Vec<ComputerAction> {
        self.actions
            .lock()
            .expect("computer mock lock poisoned")
            .clone()
    }
}
#[async_trait]
impl ComputerBackend for MockComputerBackend {
    async fn execute(&self, action: ComputerAction) -> Result<Value, ComputerError> {
        validate_action(&action)?;
        self.actions
            .lock()
            .expect("computer mock lock poisoned")
            .push(action.clone());
        Ok(match action {
            ComputerAction::Screenshot { window_id } => json!(Screenshot {
                media_type: "image/png".into(),
                bytes_base64: "".into(),
                width: 1280,
                height: 720,
                window_id
            }),
            ComputerAction::ListWindows => json!([WindowInfo {
                id: "mock-window".into(),
                title: "Rivo test window".into(),
                process: "rivo-test".into(),
                process_id: 1,
                class_name: "mock".into(),
                sensitive: false,
                bounds: [0, 0, 1280, 720],
                foreground: true
            }]),
            ComputerAction::UiSnapshot { .. } => json!({"nodes": []}),
            _ => json!({"ok": true}),
        })
    }
}

pub fn validate_action(action: &ComputerAction) -> Result<(), ComputerError> {
    match action {
        ComputerAction::UiSnapshot { max_nodes, .. } if *max_nodes == 0 || *max_nodes > 500 => Err(
            ComputerError::Invalid("UI snapshot max_nodes must be 1..=500".into()),
        ),
        ComputerAction::Type { text } if text.chars().count() > MAX_TEXT_CHARS => {
            Err(ComputerError::Invalid("text exceeds 16 KiB limit".into()))
        }
        ComputerAction::Key { key, modifiers }
            if key.is_empty() || key.len() > 64 || modifiers.len() > 4 =>
        {
            Err(ComputerError::Invalid("invalid key or modifiers".into()))
        }
        ComputerAction::Wait { timeout_ms } if *timeout_ms > 30_000 => {
            Err(ComputerError::Invalid("wait exceeds 30 seconds".into()))
        }
        _ => Ok(()),
    }
}

/// Normalize the common Computer Use provider action envelope into Rivo's
/// backend-neutral action model. Provider-specific response envelopes remain
/// outside the desktop driver and can be adapted by the LLM connector.
pub fn from_provider_action(value: &Value) -> Result<ComputerAction, ComputerError> {
    let kind = value
        .get("action")
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .ok_or_else(|| ComputerError::Invalid("provider action is missing action/type".into()))?;
    let number = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| ComputerError::Invalid(format!("provider action missing {name}")))
    };
    let coordinate = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_array)
            .filter(|items| items.len() == 2)
            .and_then(|items| {
                Some((
                    i32::try_from(items[0].as_i64()?).ok()?,
                    i32::try_from(items[1].as_i64()?).ok()?,
                ))
            })
            .ok_or_else(|| ComputerError::Invalid(format!("provider action missing {name}")))
    };
    let point = |x_name: &str, y_name: &str, coordinate_name: &str| {
        if value.get(coordinate_name).is_some() {
            coordinate(coordinate_name)
        } else {
            Ok((number(x_name)?, number(y_name)?))
        }
    };
    let numeric_pair = |x_name: &str, y_name: &str| Ok((number(x_name)?, number(y_name)?));
    let button = match kind {
        "right_click" => MouseButton::Right,
        "middle_click" => MouseButton::Middle,
        _ => MouseButton::Left,
    };
    let action = match kind {
        "screenshot" => ComputerAction::Screenshot {
            window_id: value
                .get("window_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "mouse_move" | "move" => {
            let (x, y) = point("x", "y", "coordinate")?;
            ComputerAction::MouseMove { x, y }
        }
        "click" | "left_click" | "right_click" | "middle_click" => {
            let (x, y) = point("x", "y", "coordinate")?;
            ComputerAction::Click {
                x,
                y,
                button: value
                    .get("button")
                    .and_then(Value::as_str)
                    .map(|v| match v {
                        "right" => MouseButton::Right,
                        "middle" => MouseButton::Middle,
                        _ => MouseButton::Left,
                    })
                    .unwrap_or(button),
            }
        }
        "double_click" => {
            let (x, y) = point("x", "y", "coordinate")?;
            ComputerAction::DoubleClick {
                x,
                y,
                button: MouseButton::Left,
            }
        }
        "left_click_drag" | "drag" => {
            let (from_x, from_y) = if value.get("start_coordinate").is_some() {
                coordinate("start_coordinate")?
            } else if value.get("from_coordinate").is_some() {
                coordinate("from_coordinate")?
            } else if value.get("start_x").is_some() || value.get("start_y").is_some() {
                numeric_pair("start_x", "start_y")?
            } else {
                numeric_pair("from_x", "from_y")?
            };
            let (to_x, to_y) = if value.get("coordinate").is_some() {
                coordinate("coordinate")?
            } else if value.get("end_coordinate").is_some() {
                coordinate("end_coordinate")?
            } else if value.get("to_coordinate").is_some() {
                coordinate("to_coordinate")?
            } else if value.get("end_x").is_some() || value.get("end_y").is_some() {
                numeric_pair("end_x", "end_y")?
            } else {
                numeric_pair("to_x", "to_y")?
            };
            ComputerAction::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            }
        }
        "scroll" => {
            // Validate every supplied representation before selecting the point.
            let xy = if value.get("x").is_some() || value.get("y").is_some() {
                Some(numeric_pair("x", "y")?)
            } else {
                None
            };
            let coordinates = value
                .get("coordinate")
                .map(|_| coordinate("coordinate"))
                .transpose()?;
            let (x, y) = xy.or(coordinates).ok_or_else(|| {
                ComputerError::Invalid("provider scroll requires x/y or coordinate".into())
            })?;
            let delta = if value.get("delta").is_some() {
                number("delta")?
            } else {
                // Provider scroll_y grows downward; Windows wheel delta grows upward.
                number("scroll_y")?.checked_neg().ok_or_else(|| {
                    ComputerError::Invalid("provider scroll_y cannot be negated".into())
                })?
            };
            ComputerAction::Scroll { x, y, delta }
        }
        "type" => ComputerAction::Type {
            text: value
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| ComputerError::Invalid("provider action missing text".into()))?
                .to_owned(),
        },
        "key" | "keypress" => ComputerAction::Key {
            key: value
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| ComputerError::Invalid("provider action missing key".into()))?
                .to_owned(),
            modifiers: value
                .get("modifiers")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        },
        "wait" => ComputerAction::Wait {
            timeout_ms: value
                .get("duration_ms")
                .or_else(|| value.get("timeout_ms"))
                .and_then(Value::as_u64)
                .unwrap_or(1_000),
        },
        _ => {
            return Err(ComputerError::Invalid(format!(
                "unsupported provider action: {kind}"
            )))
        }
    };
    validate_action(&action)?;
    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mock_backend_records_and_returns_observation() {
        let backend = MockComputerBackend::default();
        let result = backend
            .execute(ComputerAction::Screenshot { window_id: None })
            .await
            .unwrap();
        assert_eq!(result["width"], 1280);
        assert_eq!(backend.actions().len(), 1);
    }
    #[test]
    fn validation_rejects_unbounded_input() {
        let action = ComputerAction::Type {
            text: "x".repeat(MAX_TEXT_CHARS + 1),
        };
        assert!(matches!(
            validate_action(&action),
            Err(ComputerError::Invalid(_))
        ));
    }

    #[test]
    fn provider_actions_normalize_to_backend_actions() {
        let action = from_provider_action(
            &json!({"action":"left_click_drag","start_x":1,"start_y":2,"end_x":3,"end_y":4}),
        )
        .unwrap();
        assert_eq!(
            action,
            ComputerAction::Drag {
                from_x: 1,
                from_y: 2,
                to_x: 3,
                to_y: 4
            }
        );
        let action = from_provider_action(&json!({"type":"type","text":"你好"})).unwrap();
        assert_eq!(
            action,
            ComputerAction::Type {
                text: "你好".into()
            }
        );
        assert_eq!(
            from_provider_action(&json!({"type":"move","x":9,"y":10})).unwrap(),
            ComputerAction::MouseMove { x: 9, y: 10 }
        );
        assert_eq!(
            from_provider_action(&json!({"type":"click","button":"right","x":9,"y":10})).unwrap(),
            ComputerAction::Click {
                x: 9,
                y: 10,
                button: MouseButton::Right
            }
        );
        assert_eq!(
            from_provider_action(&json!({"type":"double_click","coordinate":[11,12]})).unwrap(),
            ComputerAction::DoubleClick {
                x: 11,
                y: 12,
                button: MouseButton::Left
            }
        );
        assert_eq!(
            from_provider_action(&json!({
                "type":"left_click_drag",
                "start_coordinate":[1,2],
                "coordinate":[3,4]
            }))
            .unwrap(),
            ComputerAction::Drag {
                from_x: 1,
                from_y: 2,
                to_x: 3,
                to_y: 4
            }
        );
        assert_eq!(
            from_provider_action(&json!({"type":"scroll","coordinate":[11,12],"scroll_y":240}))
                .unwrap(),
            ComputerAction::Scroll {
                x: 11,
                y: 12,
                delta: -240
            }
        );
    }

    #[test]
    fn provider_scroll_converts_direction_and_preserves_explicit_delta() {
        for (scroll_y, delta) in [(240, -240), (-240, 240), (0, 0)] {
            assert_eq!(
                from_provider_action(&json!({"type":"scroll","x":800,"y":500,"scroll_y":scroll_y}))
                    .unwrap(),
                ComputerAction::Scroll {
                    x: 800,
                    y: 500,
                    delta
                }
            );
        }
        for delta in [240, -240, i32::MIN, i32::MAX] {
            assert_eq!(
                from_provider_action(
                    &json!({"type":"scroll","x":800,"y":500,"delta":delta,"scroll_y":240})
                )
                .unwrap(),
                ComputerAction::Scroll {
                    x: 800,
                    y: 500,
                    delta
                }
            );
        }
        for scroll_y in [json!(i32::MIN), json!(i64::MAX), json!("240")] {
            assert!(matches!(
                from_provider_action(&json!({"type":"scroll","x":800,"y":500,"scroll_y":scroll_y})),
                Err(ComputerError::Invalid(_))
            ));
        }
    }

    #[test]
    fn provider_scroll_requires_complete_valid_coordinates() {
        for fields in [
            json!({}),
            json!({"x":800}),
            json!({"y":500}),
            json!({"x":"800","y":500}),
            json!({"x":800,"y":500.5}),
            json!({"x":null,"y":500}),
            json!({"x":i64::MAX,"y":500}),
            json!({"x":800,"y":i64::MIN}),
            json!({"coordinate":null}),
            json!({"coordinate":{"0":800,"1":500}}),
            json!({"coordinate":[]}),
            json!({"coordinate":[800]}),
            json!({"coordinate":[800,500,1]}),
            json!({"coordinate":["800",500]}),
            json!({"coordinate":[800,i64::MAX]}),
            json!({"x":"800","y":500,"coordinate":[800,500]}),
            json!({"x":800,"coordinate":[800,500]}),
            json!({"x":800,"y":500,"coordinate":[800]}),
        ] {
            let mut action = fields;
            action["type"] = json!("scroll");
            action["delta"] = json!(-120);
            assert!(
                matches!(
                    from_provider_action(&action),
                    Err(ComputerError::Invalid(_))
                ),
                "invalid coordinates must be rejected: {action}"
            );
        }
        for fields in [
            json!({"x":0,"y":0}),
            json!({"coordinate":[0,0]}),
            json!({"x":0,"y":0,"coordinate":[800,500]}),
        ] {
            let mut action = fields;
            action["type"] = json!("scroll");
            action["delta"] = json!(-120);
            assert_eq!(
                from_provider_action(&action).unwrap(),
                ComputerAction::Scroll {
                    x: 0,
                    y: 0,
                    delta: -120
                }
            );
        }
    }
}
