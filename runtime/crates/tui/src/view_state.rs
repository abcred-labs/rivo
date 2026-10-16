use rivo_session::{InteractionRequest, ModelSelection, TurnOptions};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusTarget {
    Conversation,
    Activity,
    #[default]
    Input,
    Drawer,
}

/// The single visible keyboard layer. A higher layer always receives input
/// before the focused pane beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveOverlay {
    Interaction,
    Confirmation,
    DiagnosticDetail,
    Help,
    CommandPalette,
    SessionSettings,
    WorkspaceSelector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceChoice {
    pub id: String,
    pub display_name: String,
    pub root_path: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSelectorState {
    pub workspaces: Vec<WorkspaceChoice>,
    pub selected: usize,
}

impl WorkspaceSelectorState {
    pub fn new(mut workspaces: Vec<WorkspaceChoice>) -> Self {
        workspaces.sort_by(|left, right| {
            right
                .is_default
                .cmp(&left.is_default)
                .then_with(|| left.display_name.cmp(&right.display_name))
                .then_with(|| left.id.cmp(&right.id))
        });
        let selected = workspaces
            .iter()
            .position(|workspace| workspace.is_default)
            .unwrap_or(0);
        Self {
            workspaces,
            selected,
        }
    }

    pub fn move_selection(&mut self, delta: i16) {
        if self.workspaces.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected
                .saturating_add(delta as usize)
                .min(self.workspaces.len() - 1)
        };
    }

    pub fn selected_workspace(&self) -> Option<&WorkspaceChoice> {
        self.workspaces.get(self.selected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSettingsState {
    pub selection: ModelSelection,
    pub options: TurnOptions,
    pub field: usize,
    pub mode: SessionSettingsMode,
    pub layout_columns: usize,
    pub search: String,
    /// The value being edited is kept separate from the applied draft until
    /// Enter confirms the field.
    pub edit_buffer: String,
    pub edit_cursor: usize,
    pub candidate_index: usize,
    pub multi_items: Vec<String>,
    pub multi_selected: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionSettingsMode {
    #[default]
    Browse,
    EditField,
    EditText,
    MultiSelect,
}

impl SessionSettingsState {
    pub const FIELD_COUNT: usize = 15;

    pub fn new(selection: ModelSelection, options: TurnOptions) -> Self {
        Self {
            selection,
            options,
            field: 0,
            mode: SessionSettingsMode::Browse,
            layout_columns: 4,
            search: String::new(),
            edit_buffer: String::new(),
            edit_cursor: 0,
            candidate_index: 0,
            multi_items: Vec::new(),
            multi_selected: BTreeSet::new(),
        }
    }

    pub fn with_field(mut self, field: usize) -> Self {
        self.field = field.min(Self::FIELD_COUNT - 1);
        self
    }

    pub fn move_field(&mut self, delta: i16) {
        if delta < 0 {
            self.field = self.field.saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.field = self
                .field
                .saturating_add(delta as usize)
                .min(Self::FIELD_COUNT - 1);
        }
    }

    pub fn cycle_field(&mut self, delta: i16) {
        self.field = if delta < 0 {
            self.field.checked_sub(1).unwrap_or(Self::FIELD_COUNT - 1)
        } else {
            (self.field + 1) % Self::FIELD_COUNT
        };
    }

    /// Move within the wide form's visual grid. Compact layouts use the same
    /// order for Left/Right and still get predictable Up/Down movement.
    pub fn move_spatial(&mut self, direction: SessionSettingsDirection) {
        if self.layout_columns == 1 {
            match direction {
                SessionSettingsDirection::Up | SessionSettingsDirection::Left => {
                    self.move_field(-1)
                }
                SessionSettingsDirection::Down | SessionSettingsDirection::Right => {
                    self.move_field(1)
                }
            }
            return;
        }
        if self.layout_columns == 2 {
            match direction {
                SessionSettingsDirection::Left => self.field = self.field.saturating_sub(1),
                SessionSettingsDirection::Right => {
                    self.field = self.field.saturating_add(1).min(Self::FIELD_COUNT - 1)
                }
                SessionSettingsDirection::Up => self.field = self.field.saturating_sub(2),
                SessionSettingsDirection::Down => {
                    self.field = self.field.saturating_add(2).min(Self::FIELD_COUNT - 1)
                }
            }
            return;
        }
        let rows: [&[usize]; 4] = [&[0, 1, 2, 3], &[4, 5, 6, 7, 8], &[9, 10, 11, 12], &[13, 14]];
        let (row, column) = rows
            .iter()
            .enumerate()
            .find_map(|(row, fields)| {
                fields
                    .iter()
                    .position(|field| *field == self.field)
                    .map(|column| (row, column))
            })
            .unwrap_or((0, 0));
        match direction {
            SessionSettingsDirection::Left => {
                self.field = rows[row][column.saturating_sub(1)];
            }
            SessionSettingsDirection::Right => {
                self.field = rows[row][(column + 1).min(rows[row].len() - 1)];
            }
            SessionSettingsDirection::Up => {
                let target_row = row.saturating_sub(1);
                let target_column = column.min(rows[target_row].len() - 1);
                self.field = rows[target_row][target_column];
            }
            SessionSettingsDirection::Down => {
                let target_row = (row + 1).min(rows.len() - 1);
                let target_column = column.min(rows[target_row].len() - 1);
                self.field = rows[target_row][target_column];
            }
        }
    }

    pub fn set_layout_columns(&mut self, columns: usize) {
        self.layout_columns = columns.clamp(1, 4);
    }

    pub fn begin_edit(&mut self, mode: SessionSettingsMode) {
        self.mode = mode;
        self.edit_buffer = self.value(self.field);
        self.edit_cursor = self.edit_buffer.len();
        self.candidate_index = 0;
        self.multi_items.clear();
        self.multi_selected.clear();
    }

    pub fn cancel_edit(&mut self) {
        self.mode = SessionSettingsMode::Browse;
        self.edit_buffer.clear();
        self.edit_cursor = 0;
        self.candidate_index = 0;
        self.multi_items.clear();
        self.multi_selected.clear();
    }

    pub fn field_label(field: usize) -> &'static str {
        [
            "Vendor",
            "Model",
            "Provider",
            "Reasoning",
            "Mode",
            "Policy",
            "Network",
            "Browser",
            "Shell",
            "MCP servers/toolsets",
            "A2A peers",
            "Memory recall",
            "Trace",
            "Subagents",
            "Max subagents",
        ][field.min(Self::FIELD_COUNT - 1)]
    }

    pub fn value(&self, field: usize) -> String {
        match field {
            0 => self.selection.vendor.clone(),
            1 => self.selection.model.clone(),
            2 => match &self.selection.provider {
                rivo_session::ProviderSelection::Auto => "Auto".into(),
                rivo_session::ProviderSelection::Explicit { provider_id } => provider_id.clone(),
            },
            3 => self.options.reasoning_effort.as_str().into(),
            4 => self.options.capability_profile.to_string(),
            5 => self.options.policy_profile.to_string(),
            6 => on_off(self.options.network),
            7 => on_off(self.options.browser),
            8 => on_off(self.options.shell),
            9 => {
                if self.options.mcp_servers.is_empty() {
                    "off".into()
                } else {
                    let toolsets = self
                        .options
                        .mcp_toolsets
                        .iter()
                        .map(|(server, values)| {
                            format!(
                                "{server}/{}",
                                values.iter().cloned().collect::<Vec<_>>().join("/")
                            )
                        })
                        .collect::<Vec<_>>();
                    self.options
                        .mcp_servers
                        .iter()
                        .cloned()
                        .chain(toolsets)
                        .collect::<Vec<_>>()
                        .join(",")
                }
            }
            10 => {
                if self.options.a2a_peers.is_empty() {
                    "off".into()
                } else {
                    self.options
                        .a2a_peers
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")
                }
            }
            11 => on_off(self.options.memory_recall),
            12 => on_off(self.options.trace),
            13 => on_off(self.options.subagents_enabled),
            14 => self.options.max_subagents.to_string(),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSettingsDirection {
    Up,
    Down,
    Left,
    Right,
}

fn on_off(value: bool) -> String {
    if value {
        "on".into()
    } else {
        "off".into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticRecord {
    pub stage: String,
    pub code: String,
    pub retryable: bool,
    pub recovery: String,
    pub message: String,
    pub cause: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionModalState {
    pub request: InteractionRequest,
    pub selected: usize,
    pub freeform: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationBlock {
    pub id: usize,
    pub role: String,
    pub text: String,
    pub collapsed: bool,
}
