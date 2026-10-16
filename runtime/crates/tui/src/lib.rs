use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Margin},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use rivo_core::HostCapabilitySummary;
#[cfg(test)]
use rivo_session::InteractionRequest;
use rivo_session::{
    resolve_available_route, AgentEvent, FileConfig, ModelRoutingDeletePlan, ModelSelection,
    ProviderSelection, ResolvedRoute, TurnOptions,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod layout;
pub use layout::{colors_enabled, display_width, layout_profile, LayoutProfile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputCursorLayout {
    line: usize,
    column: usize,
    horizontal_scroll: usize,
    vertical_scroll: usize,
}

/// Calculate the cursor position and horizontal viewport from the same display
/// columns used by the input Paragraph. `cursor` is a UTF-8 byte offset because
/// the editor stores cursor positions at grapheme boundaries in the source
/// string.
fn input_cursor_layout(
    input: &str,
    cursor: usize,
    viewport_width: usize,
    viewport_height: usize,
) -> InputCursorLayout {
    let mut cursor = cursor.min(input.len());
    while cursor > 0 && !input.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let prefix = &input[..cursor];
    let (line, line_prefix) = match prefix.rsplit_once('\n') {
        Some((before, after)) => (before.matches('\n').count() + 1, after),
        None => (0, prefix),
    };
    let column = display_width(line_prefix);
    let viewport_width = viewport_width.max(1);
    let viewport_height = viewport_height.max(1);
    let horizontal_scroll = column.saturating_sub(viewport_width.saturating_sub(1));
    let vertical_scroll = line.saturating_sub(viewport_height.saturating_sub(1));
    InputCursorLayout {
        line,
        column,
        horizontal_scroll,
        vertical_scroll,
    }
}
mod queue;
pub use queue::EventQueue;
mod tool_activity;
pub use tool_activity::{ToolActivity, ToolStatus};
mod history_view;
pub use history_view::{HistoryDestructiveAction, HistoryEntry};
mod view_state;
pub use view_state::{
    ActiveOverlay, ConversationBlock, DiagnosticRecord, FocusTarget, InteractionModalState,
    SessionSettingsDirection, SessionSettingsMode, SessionSettingsState, WorkspaceChoice,
    WorkspaceSelectorState,
};
mod sidebar;
pub use sidebar::SidebarView;
mod startup;
pub use startup::{catalog_models, StartupConfigState};
#[cfg(test)]
mod routing_tests;
pub type StartupState = StartupConfigState;
pub type TuiState = AppState;

/// Rivo 暖色主题；配色仅在 `colors_enabled()` 为真时生效，
/// `NO_COLOR` 下所有样式辅助函数都会降级为 `Style::default()`，保持纯文本语义。
mod theme {
    use ratatui::style::Color;

    /// Rivo 主橙色，用于品牌 Logo、聚焦边框与运行态强调。
    pub const BRAND: Color = Color::Rgb(0xF2, 0x6A, 0x3D);
    /// 次要信息与未聚焦边框的低调灰。
    pub const DIM: Color = Color::Rgb(0x8A, 0x8C, 0x91);
    /// 正向状态（网络就绪、完成）。
    pub const OK: Color = Color::Rgb(0x6C, 0xB6, 0x6C);
    /// 需要注意（待确认、接近预算上限）。
    pub const WARN: Color = Color::Rgb(0xD6, 0xB4, 0x5A);
    /// 错误与超限。
    pub const ERR: Color = Color::Rgb(0xD9, 0x5C, 0x5C);
}

/// 统一的配色开关：`NO_COLOR` 时返回默认样式，其余情况原样返回。
fn styled(style: Style) -> Style {
    if colors_enabled() {
        style
    } else {
        Style::default()
    }
}

fn brand_style() -> Style {
    styled(
        Style::default()
            .fg(theme::BRAND)
            .add_modifier(Modifier::BOLD),
    )
}

fn dim_style() -> Style {
    styled(Style::default().fg(theme::DIM))
}

/// 运行阶段对应的强调色。
fn phase_style(phase: TurnPhase) -> Style {
    let color = match phase {
        TurnPhase::Running => theme::BRAND,
        TurnPhase::WaitingConfirmation | TurnPhase::Cancelling => theme::WARN,
        TurnPhase::Finished => theme::OK,
        TurnPhase::Unknown => theme::ERR,
        TurnPhase::Idle => theme::DIM,
    };
    styled(Style::default().fg(color))
}

/// 运行阶段的圆点图标与简短标签（Claude 风格的 `● Running` 指示）。
fn phase_indicator(phase: TurnPhase) -> (&'static str, &'static str) {
    match phase {
        TurnPhase::Idle => ("○", "Idle"),
        TurnPhase::Running => ("●", "Running"),
        TurnPhase::WaitingConfirmation => ("◐", "Waiting for confirmation"),
        TurnPhase::Cancelling => ("◑", "Cancelling"),
        TurnPhase::Finished => ("●", "Complete"),
        TurnPhase::Unknown => ("✕", "Error"),
    }
}

fn localized(state: &AppState, id: &str, fallback: &str) -> String {
    state
        .messages
        .get(id)
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

/// context 占用比例，钳制在 0..=1。
fn context_ratio(used: u64, limit: u64) -> f64 {
    if limit == 0 {
        0.0
    } else {
        (used as f64 / limit as f64).clamp(0.0, 1.0)
    }
}

/// 依据占用比例返回 OK/WARN/ERR 样式。
fn budget_style(ratio: f64) -> Style {
    let color = if ratio >= 0.9 {
        theme::ERR
    } else if ratio >= 0.7 {
        theme::WARN
    } else {
        theme::OK
    };
    styled(Style::default().fg(color))
}

/// 纯字符进度条，`NO_COLOR` 下依然是可读的 `▓░` 文本。
fn inline_gauge(ratio: f64, width: usize) -> String {
    let width = width.max(1);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    format!(
        "{}{}",
        "▓".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

/// 圆角面板 Block，聚焦时用品牌色描边，其余用低调灰（Claude 圆角风格）。
fn panel_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    let border_style = if focused { brand_style() } else { dim_style() };
    Block::default()
        .title(Span::styled(title.into(), brand_style()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
}

const COMPACT_BRAND: &str = "Rivo";

/// 终端中与 SVG 图形标对应的四向汇流图案。
const UNICODE_BRAND_MARK: [&str; 7] = [
    "    │    ",
    "  │ └──  ",
    "         ",
    "──┘   ┌──",
    "      │  ",
    "  ──┐ │  ",
    "    │    ",
];

/// 不支持 Unicode 时使用的 ASCII 退化图案。
const ASCII_BRAND_MARK: [&str; 7] = [
    "    |    ",
    "  | +--  ",
    "         ",
    "--+   +--",
    "      |  ",
    "  --+ |  ",
    "    |    ",
];

/// 5x7 点阵字母表，用于在宽终端上拼出与 rivo-logo.svg 横向构图对应的 "RIVO" 大字。
const LETTER_R: [&str; 7] = [
    "████ ",
    "█   █",
    "█   █",
    "████ ",
    "█ █  ",
    "█  █ ",
    "█   █",
];
const LETTER_I: [&str; 7] = [
    "█████",
    "  █  ",
    "  █  ",
    "  █  ",
    "  █  ",
    "  █  ",
    "█████",
];
const LETTER_V: [&str; 7] = [
    "█   █",
    "█   █",
    "█   █",
    "█   █",
    "█   █",
    " █ █ ",
    "  █  ",
];
const LETTER_O: [&str; 7] = [
    " ███ ",
    "█   █",
    "█   █",
    "█   █",
    "█   █",
    "█   █",
    " ███ ",
];

/// 不支持 Unicode 时使用的 ASCII 版 "RIVO" 点阵字母表。
const LETTER_R_ASCII: [&str; 7] = [
    "#### ", "#   #", "#   #", "#### ", "# #  ", "#  # ", "#   #",
];
const LETTER_I_ASCII: [&str; 7] = [
    "#####", "  #  ", "  #  ", "  #  ", "  #  ", "  #  ", "#####",
];
const LETTER_V_ASCII: [&str; 7] = [
    "#   #", "#   #", "#   #", "#   #", "#   #", " # # ", "  #  ",
];
const LETTER_O_ASCII: [&str; 7] = [
    " ### ", "#   #", "#   #", "#   #", "#   #", "#   #", " ### ",
];

fn unicode_enabled() -> bool {
    std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LANG"))
        .map(|value| !value.eq_ignore_ascii_case("C"))
        .unwrap_or(true)
}

/// 拼出图标 + "RIVO" 大字的横排 wordmark（对应 rivo-logo.svg 图标在左、字标在右的构图）。
fn wordmark_rows(unicode: bool) -> [String; 7] {
    let mark: &[&str; 7] = if unicode {
        &UNICODE_BRAND_MARK
    } else {
        &ASCII_BRAND_MARK
    };
    let letters: [&[&str; 7]; 4] = if unicode {
        [&LETTER_R, &LETTER_I, &LETTER_V, &LETTER_O]
    } else {
        [
            &LETTER_R_ASCII,
            &LETTER_I_ASCII,
            &LETTER_V_ASCII,
            &LETTER_O_ASCII,
        ]
    };
    std::array::from_fn(|i| {
        let mut row = String::from(mark[i]);
        row.push_str("  ");
        for (j, letter) in letters.iter().enumerate() {
            if j > 0 {
                row.push(' ');
            }
            row.push_str(letter[i]);
        }
        row
    })
}

/// wordmark 横排 banner 的显示宽度（用于决定 `brand_banner_with_tagline` 的降级阈值）。
fn wordmark_display_width() -> usize {
    display_width(&wordmark_rows(true)[0])
}

fn brand_banner_with_tagline(width: usize, unicode: bool, tagline: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if width >= wordmark_display_width() + 2 {
        for row in wordmark_rows(unicode) {
            lines.push(Line::from(Span::styled(row, brand_style())));
        }
    } else if width >= 32 {
        let mark = if unicode {
            &UNICODE_BRAND_MARK
        } else {
            &ASCII_BRAND_MARK
        };
        for row in mark {
            lines.push(Line::from(Span::styled(*row, brand_style())));
        }
        lines.push(Line::from(Span::styled(COMPACT_BRAND, brand_style())));
    } else {
        lines.push(Line::from(Span::styled(COMPACT_BRAND, brand_style())));
    }
    lines.push(Line::from(Span::styled(tagline.to_owned(), dim_style())));
    lines
}

#[cfg(test)]
fn brand_banner_with_unicode(width: usize, unicode: bool) -> Vec<Line<'static>> {
    brand_banner_with_tagline(
        width,
        unicode,
        "让智能体在你的终端里工作 · 选择配置后按 Enter 开始",
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Idle,
    Running,
    Confirming,
    Finished,
    Error,
}

/// 运行生命周期与诊断分离；失败记录不会把整个工作台锁死在 Error 页面。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TurnPhase {
    #[default]
    Idle,
    Running,
    WaitingConfirmation,
    Cancelling,
    Finished,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSelection {
    pub selection: ModelSelection,
    pub turn_options: TurnOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProvider {
    pub id: String,
    pub status: String,
    pub priority: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub vendor: String,
    pub display_name: String,
    pub providers: Vec<CatalogProvider>,
    pub supported_reasoning_efforts: std::collections::BTreeSet<rivo_session::ReasoningEffort>,
}

fn provider_label(provider: &ProviderSelection) -> String {
    match provider {
        ProviderSelection::Auto => "Auto".into(),
        ProviderSelection::Explicit { provider_id } => format!("显式 {provider_id}"),
    }
}

/// UI-owned draft for the model routing management page.  The terminal UI does
/// not write configuration directly: its caller validates and commits this
/// draft through the session transaction service after an explicit confirm.
#[derive(Debug, Clone)]
pub struct ModelRoutingConfigState {
    pub revision: String,
    pub draft: FileConfig,
    pub selected_vendor: Option<String>,
    pub selected_model: Option<String>,
    pub selected_provider_id: Option<String>,
    pub preview: Option<ResolvedRoute>,
    pub delete_plan: Option<ModelRoutingDeletePlan>,
    pub notice: String,
}

impl ModelRoutingConfigState {
    pub fn new(revision: String, draft: FileConfig) -> Self {
        let selected_vendor = draft.defaults.vendor.clone();
        let selected_model = draft.defaults.model.clone();
        let selected_provider_id = draft.defaults.provider_id.clone();
        let result = resolve_available_route(
            &draft,
            selected_vendor.as_deref(),
            selected_model.as_deref(),
            selected_provider_id.as_deref(),
        );
        let notice = result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default();
        Self {
            revision,
            draft,
            selected_vendor,
            selected_model,
            selected_provider_id,
            preview: result.ok(),
            delete_plan: None,
            notice,
        }
    }

    pub fn select(&mut self, vendor: Option<String>, model: Option<String>) {
        self.selected_vendor = vendor;
        self.selected_model = model;
        let result = resolve_available_route(
            &self.draft,
            self.selected_vendor.as_deref(),
            self.selected_model.as_deref(),
            self.selected_provider_id.as_deref(),
        );
        self.notice = result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default();
        self.preview = result.ok();
    }

    pub fn set_default_route(&mut self) -> Result<(), String> {
        self.preview = None;
        let vendor = self
            .selected_vendor
            .clone()
            .ok_or_else(|| "Select a vendor".to_owned())?;
        let model = self.selected_model.clone();
        let route = resolve_available_route(
            &self.draft,
            Some(&vendor),
            model.as_deref(),
            self.selected_provider_id.as_deref(),
        )
        .map_err(|error| {
            self.notice = error.to_string();
            self.notice.clone()
        })?;
        self.draft.defaults.vendor = Some(route.vendor.clone());
        self.draft.defaults.model = Some(route.model.clone());
        self.draft.defaults.provider_id = self.selected_provider_id.clone();
        self.preview = Some(route);
        self.notice.clear();
        Ok(())
    }

    pub fn route_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "Provider 选择: {}",
            self.selected_provider_id
                .as_ref()
                .map_or_else(|| "Auto".into(), |id| format!("显式 {id}"))
        )];
        lines.extend(match &self.preview {
            Some(route) => vec![
                format!("Provider: {} ({})", route.provider_id, route.protocol),
                format!("Model: {} / {}", route.vendor, route.model),
                format!("Priority: {}", route.priority),
            ],
            None => vec![format!("Provider: 未解析；{}", self.notice)],
        });
        lines
    }
}

#[derive(Debug, Default)]
pub struct AppState {
    /// Present only before the first session is opened.
    pub startup: Option<StartupConfigState>,
    pub session_id: Option<String>,
    pub workspace_name: Option<String>,
    pub workspace_root: Option<String>,
    /// Host capabilities are supplied by the CLI/Sidecar boundary. The TUI
    /// must consume this projection instead of inferring permissions from the
    /// platform name.
    pub sidecar_host_capabilities: Option<HostCapabilitySummary>,
    pub selection: Option<ModelSelection>,
    pub route: Option<ResolvedRoute>,
    pub catalog_models: Vec<CatalogModel>,
    /// UI locale selected by the CLI-owned language runtime.
    pub current_language: Option<String>,
    /// Valid locales from the latest immutable discovery snapshot.
    pub available_languages: Vec<String>,
    /// Resolved UI messages from the active immutable catalog. Unknown keys fall back
    /// to the code-owned English text at each render site.
    pub messages: BTreeMap<String, String>,
    /// Enabled MCP server names copied from the resolved local allowlist.
    /// Commands use this for immediate feedback; Session repeats the check as
    /// the authoritative enforcement point.
    pub available_mcp_servers: Vec<String>,
    /// Only trusted local A2A peer IDs are exposed to the settings editor.
    pub available_a2a_peers: Vec<String>,
    /// Configured MCP toolsets, keyed by their safe server name.
    pub available_mcp_toolsets: BTreeMap<String, BTreeSet<String>>,
    pub mode: Option<UiMode>,
    /// Session 最后报告的运行模型；不是用户选择，也不用于推导 Provider。
    pub model: Option<String>,
    pub input: String,
    pub input_cursor: usize,
    pub answer: String,
    pub conversation: Vec<ConversationBlock>,
    pub selected_conversation: usize,
    /// 当前 Turn 正在接收流式文本的 assistant 消息块。
    pub active_assistant_block: Option<usize>,
    pub notice: String,
    pub pending_request_id: Option<String>,
    pub pending_tool: Option<String>,
    pub pending_arguments_summary: Option<String>,
    pub pending_side_effects: Vec<String>,
    pub pending_confirmation_details: Option<String>,
    pub pending_session_scope_available: bool,
    pub pending_session_scope_category: Option<String>,
    pub pending_memory_candidate: Option<(String, String)>,
    pub last_retry_step_id: Option<String>,
    pub turn_options: TurnOptions,
    pub tools: Vec<ToolActivity>,
    pub tool_details: HashMap<String, String>,
    pub expanded_tools: HashSet<String>,
    pub trace: Vec<String>,
    pub evidence: Vec<String>,
    pub history: Vec<HistoryEntry>,
    pub selected_history: usize,
    pub pending_history_action: Option<HistoryDestructiveAction>,
    pub sidebar: SidebarView,
    pub main_scroll: u16,
    pub sidebar_scroll: u16,
    pub main_horizontal: u16,
    pub sidebar_horizontal: u16,
    pub selected_tool: usize,
    pub follow_bottom: bool,
    pub command_palette: bool,
    pub help_overlay: bool,
    pub command_input: String,
    pub command_index: usize,
    pub session_settings: Option<SessionSettingsState>,
    pub workspace_selector: Option<WorkspaceSelectorState>,
    pub turn_phase: TurnPhase,
    pub focus: FocusTarget,
    pub diagnostics: Vec<DiagnosticRecord>,
    pub selected_diagnostic: usize,
    pub diagnostic_filter: String,
    pub diagnostic_detail_open: bool,
    /// Vertical scroll offset for the active modal/drawer detail view.
    pub overlay_scroll: u16,
    pub interaction: Option<InteractionModalState>,
    pub used_tokens: u64,
    pub context_limit_tokens: u64,
    pub steps: usize,
    pub max_steps: usize,
}

impl AppState {
    pub fn reduce(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::PlanCreated { plan }
            | AgentEvent::PlanUpdated { plan }
            | AgentEvent::PlanApprovalRequired { plan, .. } => {
                self.notice = format!(
                    "{} {} revision={} status={:?}",
                    localized(self, "notice-plan-updated", "Plan updated"),
                    plan.plan_id,
                    plan.revision,
                    plan.status
                );
                self.record_trace(format!(
                    "plan updated id={} revision={} status={:?}",
                    plan.plan_id, plan.revision, plan.status
                ));
            }
            AgentEvent::PlanApproved {
                plan_id, revision, ..
            } => {
                self.notice = format!(
                    "{}: {plan_id} (revision {revision})",
                    localized(self, "notice-plan-approved", "Plan approved")
                )
            }
            AgentEvent::PlanRejected {
                plan_id,
                revision,
                reason,
            } => {
                self.notice = format!(
                    "{}: {plan_id} (revision {revision}) {}",
                    localized(self, "notice-plan-rejected", "Plan rejected"),
                    reason.unwrap_or_default()
                )
            }
            AgentEvent::PlanExecutionStarted { plan_id, revision } => {
                self.notice = format!(
                    "{}: {plan_id} (revision {revision})",
                    localized(self, "notice-plan-started", "Plan execution started")
                )
            }
            AgentEvent::PlanStepStarted {
                plan_id, step_id, ..
            } => {
                self.notice = format!(
                    "{}: {plan_id}/{step_id}",
                    localized(self, "notice-step-started", "Plan step started")
                )
            }
            AgentEvent::PlanStepCompleted {
                plan_id, step_id, ..
            } => {
                self.notice = format!(
                    "{}: {plan_id}/{step_id}",
                    localized(self, "notice-step-completed", "Plan step completed")
                )
            }
            AgentEvent::PlanStepBlocked {
                plan_id,
                step_id,
                reason,
                ..
            } => {
                self.notice = format!(
                    "{}: {plan_id}/{step_id}: {reason}",
                    localized(self, "notice-step-blocked", "Plan step blocked")
                )
            }
            AgentEvent::PlanCompleted { plan_id, revision } => {
                self.notice = format!(
                    "{}: {plan_id} (revision {revision})",
                    localized(self, "notice-plan-completed", "Plan completed")
                )
            }
            AgentEvent::GoalUpdated { goal } => {
                self.notice = format!(
                    "{} {:?}: {} (revision {})",
                    localized(self, "notice-goal-updated", "Goal updated"),
                    goal.status,
                    goal.objective,
                    goal.revision
                );
                self.record_trace(format!(
                    "goal updated id={} status={:?} revision={}",
                    goal.goal_id, goal.status, goal.revision
                ));
            }
            AgentEvent::GoalContinuationScheduled { goal_id, revision } => {
                self.notice = format!(
                    "{}: {goal_id} (revision {revision})",
                    localized(
                        self,
                        "notice-goal-continuation",
                        "Goal continuation scheduled"
                    )
                );
            }
            AgentEvent::GoalAudit {
                goal_id,
                evidence,
                complete,
            } => {
                self.notice = format!(
                    "{} {}: {}",
                    localized(self, "notice-goal-audit", "Goal audit"),
                    goal_id,
                    if complete {
                        localized(self, "goal-audit-complete", "satisfied")
                    } else {
                        localized(self, "goal-audit-continue", "continue")
                    }
                );
                self.record_trace(format!("goal audit evidence={evidence}"));
            }
            AgentEvent::GoalCompleted { goal_id, revision } => {
                self.notice = format!(
                    "{}: {goal_id} (revision {revision})",
                    localized(self, "notice-goal-completed", "Goal completed")
                );
            }
            AgentEvent::GoalBlocked {
                goal_id,
                reason,
                revision,
            } => {
                self.notice = format!(
                    "{}: {goal_id}, {reason} (revision {revision})",
                    localized(self, "notice-goal-blocked", "Goal blocked")
                );
            }
            AgentEvent::SubagentConfirmationRequired {
                agent_id,
                task_id,
                request_id,
                call_id,
                turn_id,
                step_id,
                tool_name,
                arguments_summary,
                targets,
                program,
                side_effects,
                capability_profile,
                policy_profile,
                constraints,
                session_scope_available,
                session_scope_category,
            } => {
                self.reduce(AgentEvent::ConfirmationRequired {
                    request_id,
                    call_id,
                    turn_id,
                    step_id,
                    tool_name: tool_name.clone(),
                    arguments_summary,
                    targets,
                    program,
                    side_effects,
                    capability_profile,
                    policy_profile,
                    constraints,
                    session_scope_available,
                    session_scope_category,
                });
                self.notice = format!(
                    "{} {agent_id:?}: {tool_name}",
                    localized(
                        self,
                        "notice-subagent-confirmation",
                        "Subagent confirmation requested"
                    )
                );
                let source_agent = localized(self, "label-source-agent", "Source agent");
                let task_label = localized(self, "label-task", "task");
                if let Some(details) = &mut self.pending_confirmation_details {
                    *details = format!(
                        "{source_agent}: {agent_id:?} / {task_label}: {task_id:?}\n{details}"
                    );
                }
                self.record_trace(format!(
                    "subagent confirmation requested agent={agent_id:?} task={task_id:?}"
                ));
            }
            AgentEvent::ModelSelected { selection, route } => {
                self.model = Some(route.model.clone());
                self.notice = format!(
                    "{}: {} / {} · {} · {}: {} ({})",
                    localized(self, "notice-model-selected", "Model selected"),
                    selection.vendor,
                    selection.model,
                    provider_label(&selection.provider),
                    localized(self, "label-resolved-provider", "Resolved provider"),
                    route.provider_id,
                    route.protocol
                );
                self.record_trace(self.notice.clone());
                self.selection = Some(selection);
                self.route = Some(route);
            }
            AgentEvent::SessionSettingsApplied {
                selection,
                route,
                options,
            } => {
                self.turn_options = options;
                self.model = Some(route.model.clone());
                self.selection = Some(selection);
                self.route = Some(route);
                self.session_settings = None;
                self.notice = localized(
                    self,
                    "notice-session-settings-applied",
                    "Session settings applied; the next turn will use them.",
                );
            }
            AgentEvent::SessionLocaleApplied { locale } => {
                self.turn_options.locale = locale;
            }
            AgentEvent::Started {
                model,
                selection,
                route,
                ..
            } => {
                self.mode = Some(UiMode::Running);
                self.turn_phase = TurnPhase::Running;
                self.model = Some(model.clone());
                // 只采用 Session 的冻结快照；preflight/旧事件清除上次路由，
                // 但不丢弃用户选择，也不从实际 Provider 反推显式选择。
                if let Some(selection) = selection {
                    self.selection = Some(selection);
                }
                self.route = route;
                self.answer.clear();
                self.active_assistant_block = None;
                self.tools.clear();
                self.tool_details.clear();
                self.expanded_tools.clear();
                self.evidence.clear();
                self.selected_tool = 0;
                self.sidebar_scroll = 0;
                self.used_tokens = 0;
                self.steps = 0;
                self.last_retry_step_id = None;
                self.pending_request_id = None;
                self.pending_tool = None;
                self.pending_arguments_summary = None;
                self.pending_side_effects.clear();
                self.pending_confirmation_details = None;
                self.pending_session_scope_available = false;
                self.pending_session_scope_category = None;
                self.pending_memory_candidate = None;
                self.record_trace(format!("started model={model}"));
                self.main_scroll = 0;
                self.follow_bottom = true;
            }
            AgentEvent::PromptPrepared {
                revision,
                instruction_hashes,
                instructions_truncated,
            } => {
                self.notice = format!(
                    "{} revision={revision}; {} {}{}",
                    localized(self, "notice-prompt-prepared", "Prompt prepared"),
                    localized(
                        self,
                        "label-persistent-instructions",
                        "persistent instructions"
                    ),
                    instruction_hashes.len(),
                    if instructions_truncated {
                        localized(self, "label-truncated", " (truncated)")
                    } else {
                        String::new()
                    }
                );
                self.record_trace(format!(
                    "prompt prepared revision={revision} instruction_hashes={} truncated={instructions_truncated}",
                    instruction_hashes.join(",")
                ));
            }
            AgentEvent::ModelDelta { text } => {
                let text_len = text.len();
                self.answer.push_str(&text);
                let block = self.active_assistant_block();
                self.conversation[block].text.push_str(&text);
                self.record_trace(format!("model delta: {} bytes", text_len));
            }
            AgentEvent::ToolRequested { call_id, name, .. } => {
                self.notice = format!(
                    "{}: {name}",
                    localized(self, "notice-tool-requested", "Tool requested")
                );
                self.tools.push(ToolActivity {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    status: ToolStatus::Requested,
                    summary: localized(
                        self,
                        "tool-summary-awaiting-policy",
                        "Awaiting policy evaluation",
                    ),
                });
                self.selected_tool = self.tools.len().saturating_sub(1);
                self.tool_details.insert(call_id, String::new());
                if self.tools.len() > 128 {
                    if let Some(removed) = self.tools.first().cloned() {
                        self.tool_details.remove(&removed.call_id);
                        self.expanded_tools.remove(&removed.call_id);
                    }
                    self.tools.remove(0);
                }
                self.record_trace(format!("tool requested: {name}"));
            }
            AgentEvent::ToolObservation {
                call_id,
                name,
                result,
            } => {
                self.notice = format!(
                    "{}: {name}",
                    localized(self, "notice-tool-completed", "Tool completed")
                );
                let summary = observation_summary(&result);
                self.tool_details
                    .insert(call_id.clone(), truncate(&result.to_string(), 4096));
                if let Some(tool) = self.tools.iter_mut().find(|tool| tool.call_id == call_id) {
                    tool.status = if result_is_partial(&result) {
                        ToolStatus::Partial
                    } else {
                        ToolStatus::Completed
                    };
                    tool.summary = summary.clone();
                }
                let mut evidence = Vec::new();
                collect_evidence(&result, &mut evidence);
                for (id, excerpt) in evidence {
                    self.evidence
                        .push(format!("{id}: {}", truncate(&excerpt, 96)));
                    if self.evidence.len() > 128 {
                        self.evidence.remove(0);
                    }
                }
                self.record_trace(format!("tool observed: {name} ({summary})"));
            }
            AgentEvent::MediaStarted { operation, .. } => {
                self.notice = format!(
                    "{} {operation:?}",
                    localized(self, "notice-media-started", "Media operation in progress:")
                );
                self.record_trace(self.notice.clone());
            }
            AgentEvent::MediaReady { result, .. } => {
                self.notice = match result.delivery {
                    rivo_core::MediaDelivery::LocalPath { path } => {
                        format!(
                            "{}: {path}",
                            localized(self, "notice-media-ready", "Media ready")
                        )
                    }
                    rivo_core::MediaDelivery::SignedUrl { url, .. } => {
                        format!(
                            "{}: {url}",
                            localized(self, "notice-media-ready", "Media ready")
                        )
                    }
                };
                self.record_trace(self.notice.clone());
            }
            AgentEvent::MediaFailed { message, .. } => {
                self.notice = format!(
                    "{}: {message}",
                    localized(self, "notice-media-failed", "Media operation failed")
                );
                self.record_trace(self.notice.clone());
            }
            AgentEvent::BudgetUpdated {
                used_tokens,
                context_limit_tokens,
                steps,
                max_steps,
            } => {
                self.notice = format!(
                    "{}: {used_tokens}/{context_limit_tokens} tokens, {} {steps}/{max_steps}",
                    localized(self, "label-budget", "Budget"),
                    localized(self, "label-steps", "steps"),
                );
                self.used_tokens = used_tokens;
                self.context_limit_tokens = context_limit_tokens;
                self.steps = steps;
                self.max_steps = max_steps;
                self.record_trace(self.notice.clone());
            }
            AgentEvent::ContextCompressed {
                omitted_start,
                omitted_end,
            } => {
                self.notice = format!(
                    "{} {omitted_start}..{omitted_end}",
                    localized(
                        self,
                        "notice-context-compressed",
                        "Context compressed; omitted messages"
                    )
                );
                self.record_trace(self.notice.clone());
            }
            AgentEvent::SubagentQueued {
                agent_id,
                task_id,
                label,
                ..
            } => {
                self.notice = format!(
                    "{}: {label}",
                    localized(self, "notice-subagent-queued", "Subagent queued")
                );
                self.record_trace(format!(
                    "subagent queued agent={agent_id:?} task={task_id:?}"
                ));
            }
            AgentEvent::SubagentActivity {
                agent_id,
                task_id,
                activity,
            } => {
                self.record_trace(format!(
                    "subagent activity agent={agent_id:?} task={task_id:?} {activity:?}"
                ));
            }
            AgentEvent::SubagentStarted { agent_id, task_id } => {
                self.notice = localized(self, "notice-subagent-started", "Subagent started");
                self.record_trace(format!(
                    "subagent started agent={agent_id:?} task={task_id:?}"
                ));
            }
            AgentEvent::SubagentCompleted { result } => {
                self.notice = format!(
                    "{}: {:?}",
                    localized(self, "notice-subagent-completed", "Subagent completed"),
                    result.state
                );
                self.record_trace(format!(
                    "subagent completed agent={:?} task={:?} state={:?} steps={}",
                    result.agent_id, result.task_id, result.state, result.usage.steps
                ));
            }
            AgentEvent::ConfirmationResolved {
                request_id,
                call_id,
                allowed,
                ..
            } => {
                self.record_trace(format!(
                    "confirmation resolved request={request_id} call={call_id} allowed={allowed}"
                ));
            }
            AgentEvent::ConfirmationRequired {
                request_id,
                tool_name,
                arguments_summary,
                targets,
                program,
                side_effects,
                capability_profile,
                policy_profile,
                constraints,
                session_scope_available,
                session_scope_category,
                ..
            } => {
                self.dismiss_transient_overlays();
                self.notice = format!(
                    "{}: {tool_name}",
                    localized(
                        self,
                        "notice-confirmation-required",
                        "Confirmation required"
                    )
                );
                self.mode = Some(UiMode::Confirming);
                self.turn_phase = TurnPhase::WaitingConfirmation;
                self.focus = FocusTarget::Drawer;
                self.pending_request_id = Some(request_id);
                self.pending_tool = Some(tool_name);
                self.pending_arguments_summary = Some(arguments_summary);
                self.pending_side_effects = side_effects;
                self.pending_session_scope_available = session_scope_available;
                self.pending_session_scope_category = session_scope_category;
                let target = if targets.is_empty() {
                    localized(self, "label-none", "None")
                } else {
                    targets.join("；")
                };
                self.pending_confirmation_details = Some(format!(
                    "{}: {target}\n{}: {}\n{}: {capability_profile}\n{}: {policy_profile}\n{}: {}={:?}, {}={:?}, {}={:?}, {}={}ms, {}={}B",
                    localized(self, "label-targets", "Targets"),
                    localized(self, "label-program", "Program"),
                    program.unwrap_or_else(|| localized(self, "label-none", "None")),
                    localized(self, "label-capability", "Capability"),
                    localized(self, "label-policy", "Policy"),
                    localized(self, "label-constraints", "Constraints"),
                    localized(self, "label-read", "read"),
                    constraints.read_path_scope,
                    localized(self, "label-write", "write"),
                    constraints.mutate_path_scope,
                    localized(self, "label-network", "network"),
                    constraints.network_scope,
                    localized(self, "label-timeout", "timeout"),
                    constraints.timeout_ms,
                    localized(self, "label-output-limit", "output limit"),
                    constraints.output_limit,
                ));
                let needs_confirmation = localized(
                    self,
                    "tool-summary-needs-confirmation",
                    "Requires user confirmation",
                );
                if let Some(tool) = self
                    .tools
                    .iter_mut()
                    .rev()
                    .find(|tool| tool.name == self.pending_tool.as_deref().unwrap_or_default())
                {
                    tool.status = ToolStatus::WaitingConfirmation;
                    tool.summary = needs_confirmation;
                }
                self.record_trace("confirmation requested".into());
            }
            AgentEvent::InteractionRequired { request } => {
                self.dismiss_transient_overlays();
                self.notice = format!(
                    "{}: {}",
                    localized(self, "notice-interaction-required", "Interaction required"),
                    request.title
                );
                self.turn_phase = TurnPhase::WaitingConfirmation;
                self.focus = FocusTarget::Drawer;
                self.overlay_scroll = 0;
                self.interaction = Some(InteractionModalState {
                    request,
                    selected: 0,
                    freeform: String::new(),
                });
            }
            AgentEvent::MemoryCandidate {
                candidate_id,
                body_sha256,
            } => {
                self.dismiss_transient_overlays();
                self.notice = format!(
                    "{}: {candidate_id}; {}",
                    localized(self, "notice-memory-candidate", "Memory candidate"),
                    localized(
                        self,
                        "memory-candidate-actions",
                        "press y to save or n to reject"
                    )
                );
                self.pending_memory_candidate = Some((candidate_id, body_sha256));
                self.mode = Some(UiMode::Confirming);
                self.turn_phase = TurnPhase::WaitingConfirmation;
            }
            AgentEvent::MemoryRecalled { memory_ids } => {
                self.notice = format!(
                    "{}: {}",
                    localized(self, "notice-memory-recalled", "Memories recalled"),
                    memory_ids.len()
                )
            }
            AgentEvent::MemoryInjected { memory_ids } => {
                self.notice = format!(
                    "{}: {}",
                    localized(self, "notice-memory-injected", "Memories injected"),
                    memory_ids.len()
                )
            }
            AgentEvent::MemoryConfirmed { memory_id } => {
                self.notice = format!(
                    "{}: {memory_id}",
                    localized(self, "notice-memory-saved", "Memory saved")
                );
                self.pending_memory_candidate = None;
            }
            AgentEvent::MemoryRejected { candidate_id } => {
                self.notice = format!(
                    "{}: {candidate_id}",
                    localized(self, "notice-memory-rejected", "Memory candidate rejected")
                );
                self.pending_memory_candidate = None;
            }
            AgentEvent::MemoryDeleted { memory_id } => {
                self.notice = format!(
                    "{}: {memory_id}",
                    localized(self, "notice-memory-deleted", "Memory deleted")
                );
            }
            AgentEvent::FinalAnswer { text, .. } => {
                let block = self.active_assistant_block();
                self.conversation[block].text = text.clone();
                self.answer = text;
                self.active_assistant_block = None;
                self.mode = Some(UiMode::Finished);
                self.turn_phase = TurnPhase::Finished;
                self.focus = FocusTarget::Input;
                // A confirmation overlay belongs to the turn that just ended.
                // Clear its request and details with the terminal answer so a
                // completed TUI turn cannot remain trapped behind a stale
                // overlay (for example after an approved tool call resumes).
                self.interaction = None;
                self.pending_request_id = None;
                self.pending_tool = None;
                self.pending_arguments_summary = None;
                self.pending_side_effects.clear();
                self.pending_confirmation_details = None;
                self.pending_session_scope_available = false;
                self.pending_session_scope_category = None;
                self.pending_memory_candidate = None;
            }
            AgentEvent::Failure { message, details } => {
                self.notice = message.clone();
                self.diagnostics.push(DiagnosticRecord {
                    stage: format!("{:?}", details.stage).to_lowercase(),
                    code: details.code,
                    retryable: details.retryable,
                    recovery: details.recovery,
                    message,
                    cause: details.cause,
                });
                self.selected_diagnostic = self.diagnostics.len().saturating_sub(1);
                self.record_trace("structured failure".into());
            }
            AgentEvent::Error { message } => {
                self.notice = message;
                self.diagnostics.push(DiagnosticRecord {
                    stage: "unknown".into(),
                    code: "unknown".into(),
                    retryable: false,
                    recovery: localized(
                        self,
                        "diagnostic-recovery",
                        "Review diagnostics, then retry or start a new session",
                    ),
                    message: self.notice.clone(),
                    cause: self.notice.clone(),
                });
                self.selected_diagnostic = self.diagnostics.len().saturating_sub(1);
                if let Some(tool) =
                    self.tools.iter_mut().rev().find(|tool| {
                        matches!(tool.status, ToolStatus::Requested | ToolStatus::Running)
                    })
                {
                    tool.status = ToolStatus::Failed;
                    tool.summary = self.notice.clone();
                }
                self.record_trace("error".into());
            }
            AgentEvent::RetryAvailable { step_id } => {
                self.notice = format!(
                    "{}: {step_id}",
                    localized(self, "notice-retry-available", "Retry available")
                );
                self.last_retry_step_id = Some(step_id);
            }
            AgentEvent::Stopped { reason } => {
                self.notice = reason;
                self.active_assistant_block = None;
                self.interaction = None;
                self.mode = Some(UiMode::Finished);
                self.turn_phase = TurnPhase::Finished;
                self.focus = FocusTarget::Input;
                self.pending_request_id = None;
                self.pending_tool = None;
                self.pending_arguments_summary = None;
                self.pending_side_effects.clear();
                self.pending_confirmation_details = None;
                self.pending_session_scope_available = false;
                self.pending_session_scope_category = None;
                self.pending_memory_candidate = None;
                for tool in &mut self.tools {
                    if matches!(tool.status, ToolStatus::Requested | ToolStatus::Running) {
                        tool.status = ToolStatus::Cancelled;
                    }
                }
                self.record_trace("stopped".into());
            }
        }
    }

    fn active_assistant_block(&mut self) -> usize {
        if let Some(index) = self.active_assistant_block.filter(|index| {
            self.conversation
                .get(*index)
                .is_some_and(|block| block.role == "assistant")
        }) {
            return index;
        }

        let index = self.conversation.len();
        self.conversation.push(ConversationBlock {
            id: index,
            role: "assistant".into(),
            text: String::new(),
            collapsed: false,
        });
        self.active_assistant_block = Some(index);
        self.selected_conversation = index;
        index
    }

    pub fn scroll_main(&mut self, delta: i16) {
        self.main_scroll = if delta.is_negative() {
            self.main_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.main_scroll.saturating_add(delta as u16)
        };
        self.follow_bottom = false;
    }

    pub fn scroll_horizontal(&mut self, delta: i16) {
        let target = match self.focus {
            FocusTarget::Conversation => &mut self.main_horizontal,
            FocusTarget::Activity | FocusTarget::Drawer => &mut self.sidebar_horizontal,
            FocusTarget::Input => return,
        };
        *target = if delta.is_negative() {
            target.saturating_sub(delta.unsigned_abs())
        } else {
            target.saturating_add(delta as u16)
        };
    }

    pub fn scroll_focused(&mut self, delta: i16) {
        match self.focus {
            FocusTarget::Conversation => self.scroll_main(delta),
            FocusTarget::Activity | FocusTarget::Drawer => self.scroll_sidebar(delta),
            FocusTarget::Input => {}
        }
    }

    pub fn select_conversation(&mut self, delta: i16) {
        if self.conversation.is_empty() {
            return;
        }
        let max = self.conversation.len().saturating_sub(1);
        self.selected_conversation = if delta.is_negative() {
            self.selected_conversation
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected_conversation
                .saturating_add(delta as usize)
                .min(max)
        };
    }

    pub fn toggle_selected_conversation(&mut self) {
        if let Some(block) = self.conversation.get_mut(self.selected_conversation) {
            block.collapsed = !block.collapsed;
        }
    }

    pub fn select_diagnostic(&mut self, delta: i16) {
        let visible = self
            .diagnostics
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                self.diagnostic_filter.is_empty()
                    || item.stage.contains(&self.diagnostic_filter)
                    || item.code.contains(&self.diagnostic_filter)
                    || item.message.contains(&self.diagnostic_filter)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return;
        }
        let current = visible
            .iter()
            .position(|index| *index == self.selected_diagnostic)
            .unwrap_or(0) as i16;
        let next = (current + delta).clamp(0, visible.len().saturating_sub(1) as i16) as usize;
        self.selected_diagnostic = visible[next];
    }

    /// Select a recoverable history entry without ever selecting an expired
    /// index. The caller may use [`selected_history_session`] to resume it.
    pub fn select_history(&mut self, delta: i16) {
        let entries = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.expired)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return;
        }
        let current = entries
            .iter()
            .position(|index| *index == self.selected_history)
            .unwrap_or(0) as i16;
        let next = (current + delta).clamp(0, entries.len().saturating_sub(1) as i16) as usize;
        self.selected_history = entries[next];
    }

    /// Keep the default history cursor actionable when the most recent session
    /// has expired under local retention.
    pub fn normalize_history_selection(&mut self) {
        if self
            .history
            .get(self.selected_history)
            .is_some_and(|entry| !entry.expired)
        {
            return;
        }
        self.select_history(0);
    }

    pub fn selected_history_session(&self) -> Option<&str> {
        self.history
            .get(self.selected_history)
            .filter(|entry| !entry.expired)
            .map(|entry| entry.session_id.as_str())
    }

    /// Return true only when the user repeats the exact destructive history
    /// action. A different request replaces the pending confirmation.
    pub fn confirm_history_action(&mut self, action: HistoryDestructiveAction) -> bool {
        if self.pending_history_action.as_ref() == Some(&action) {
            self.pending_history_action = None;
            true
        } else {
            self.pending_history_action = Some(action);
            false
        }
    }

    pub fn toggle_diagnostic_detail(&mut self) {
        if self.has_blocking_overlay() {
            return;
        }
        if !self.diagnostics.is_empty() {
            self.diagnostic_detail_open = !self.diagnostic_detail_open;
            if self.diagnostic_detail_open {
                self.command_palette = false;
                self.help_overlay = false;
            }
            self.overlay_scroll = 0;
        }
    }

    pub fn append_user_prompt(&mut self, text: String) {
        self.active_assistant_block = None;
        self.conversation.push(ConversationBlock {
            id: self.conversation.len(),
            role: "user".into(),
            text,
            collapsed: false,
        });
        self.selected_conversation = self.conversation.len().saturating_sub(1);
    }

    pub fn jump_to_bottom(&mut self) {
        self.main_scroll = 0;
        self.follow_bottom = true;
    }

    pub fn scroll_sidebar(&mut self, delta: i16) {
        self.sidebar_scroll = if delta.is_negative() {
            self.sidebar_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.sidebar_scroll.saturating_add(delta as u16)
        };
    }

    pub fn toggle_command_palette(&mut self) {
        if self.has_blocking_overlay() {
            return;
        }
        self.command_palette = !self.command_palette;
        if self.command_palette {
            self.help_overlay = false;
            self.diagnostic_detail_open = false;
            self.overlay_scroll = 0;
        }
        if !self.command_palette {
            self.command_input.clear();
        }
        self.command_index = 0;
    }

    pub fn toggle_help_overlay(&mut self) {
        if self.has_blocking_overlay() {
            return;
        }
        self.help_overlay = !self.help_overlay;
        if self.help_overlay {
            self.command_palette = false;
            self.diagnostic_detail_open = false;
        }
        self.overlay_scroll = 0;
    }

    pub fn active_overlay(&self) -> Option<ActiveOverlay> {
        if self.interaction.is_some() {
            Some(ActiveOverlay::Interaction)
        } else if self.pending_request_id.is_some() || self.pending_memory_candidate.is_some() {
            Some(ActiveOverlay::Confirmation)
        } else if self.diagnostic_detail_open {
            Some(ActiveOverlay::DiagnosticDetail)
        } else if self.help_overlay {
            Some(ActiveOverlay::Help)
        } else if self.command_palette {
            Some(ActiveOverlay::CommandPalette)
        } else if self.session_settings.is_some() {
            Some(ActiveOverlay::SessionSettings)
        } else if self.workspace_selector.is_some() {
            Some(ActiveOverlay::WorkspaceSelector)
        } else {
            None
        }
    }

    pub fn dismiss_transient_overlays(&mut self) {
        self.command_palette = false;
        self.help_overlay = false;
        self.diagnostic_detail_open = false;
        self.command_input.clear();
        self.command_index = 0;
        self.workspace_selector = None;
        self.overlay_scroll = 0;
    }

    pub fn open_workspace_selector(&mut self, workspaces: Vec<WorkspaceChoice>) -> bool {
        if self.has_blocking_overlay() {
            self.notice = localized(
                self,
                "error-workspace-switch-blocked",
                "Finish the pending interaction before switching workspace",
            );
            return false;
        }
        self.command_palette = false;
        self.help_overlay = false;
        self.diagnostic_detail_open = false;
        self.session_settings = None;
        self.workspace_selector = Some(WorkspaceSelectorState::new(workspaces));
        self.overlay_scroll = 0;
        true
    }

    pub fn close_workspace_selector(&mut self) {
        self.workspace_selector = None;
    }

    pub fn open_session_settings(&mut self) -> bool {
        if self.has_blocking_overlay() {
            self.notice = localized(
                self,
                "error-settings-blocked",
                "Finish the pending interaction before opening session settings",
            );
            return false;
        }
        let Some(selection) = self.selection.clone() else {
            self.notice = localized(
                self,
                "error-route-unresolved",
                "The current session has no resolved model route",
            );
            return false;
        };
        self.command_palette = false;
        self.help_overlay = false;
        self.session_settings = Some(SessionSettingsState::new(
            selection,
            self.turn_options.clone(),
        ));
        true
    }

    pub fn close_session_settings(&mut self) {
        self.session_settings = None;
    }

    pub fn push_command_input(&mut self, text: &str) {
        self.command_input.push_str(text);
        self.command_index = 0;
    }

    pub fn backspace_command_input(&mut self) {
        self.command_input.pop();
        self.command_index = 0;
    }

    pub fn command_suggestions(&self) -> Vec<String> {
        const COMMANDS: &[&str] = &[
            "exit",
            "quit",
            "cancel",
            "retry",
            "sessions",
            "sessions resume",
            "workspace",
            "workspace list",
            "workspace open",
            "new",
            "errors",
            "errors clear",
            "help",
            "language",
            "language reload",
            "provider",
            "model",
            "settings",
            "network on",
            "network off",
            "browser on",
            "browser off",
            "browser status",
            "mcp",
            "a2a",
            "a2a off",
            "trace",
            "memory",
        ];
        COMMANDS
            .iter()
            .filter(|command| command.starts_with(&self.command_input))
            .map(|command| (*command).into())
            .collect()
    }

    pub fn move_command_selection(&mut self, delta: i16) {
        let count = self.command_suggestions().len();
        if count == 0 {
            self.command_index = 0;
            return;
        }
        self.command_index = self.command_index.min(count - 1);
        self.command_index = if delta.is_negative() {
            self.command_index
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.command_index
                .saturating_add(delta as usize)
                .min(count - 1)
        };
    }

    pub fn complete_command(&mut self) {
        let suggestions = self.command_suggestions();
        if suggestions.is_empty() {
            self.command_index = 0;
            return;
        }
        self.command_index = self.command_index.min(suggestions.len() - 1);
        if let Some(value) = suggestions.get(self.command_index).cloned() {
            self.command_input = value;
        }
    }

    pub fn command_suggestion_window(&self, max_items: usize) -> (usize, Vec<String>) {
        let suggestions = self.command_suggestions();
        if suggestions.is_empty() || max_items == 0 {
            return (0, Vec::new());
        }
        let selected = self.command_index.min(suggestions.len() - 1);
        let start = selected
            .saturating_sub(max_items.saturating_sub(1))
            .min(suggestions.len().saturating_sub(max_items));
        (
            start,
            suggestions
                .into_iter()
                .skip(start)
                .take(max_items)
                .collect(),
        )
    }

    fn has_blocking_overlay(&self) -> bool {
        self.interaction.is_some()
            || self.pending_request_id.is_some()
            || self.pending_memory_candidate.is_some()
    }

    fn confirmation_detail(&self) -> Option<String> {
        if let Some(request_id) = self.pending_request_id.as_deref() {
            let effects = if self.pending_side_effects.is_empty() {
                localized(
                    self,
                    "confirmation-no-side-effects",
                    "No additional side effects",
                )
            } else {
                self.pending_side_effects.join("; ")
            };
            let actions = if self.pending_session_scope_available {
                let category = self
                    .pending_session_scope_category
                    .as_deref()
                    .unwrap_or("current risk category");
                format!(
                    "{} ({category})",
                    localized(
                        self,
                        "confirmation-actions-session",
                        "y 允许一次 · a 本会话允许 · n/Esc 拒绝"
                    )
                )
            } else {
                localized(self, "confirmation-actions-once", "y 允许一次 · n/Esc 拒绝")
            };
            return Some(format!(
                "[{}]\n{}: {}\n{}: {}\n{}\n{}: {}\nRequest: {}\n\n{actions}",
                localized(self, "approval-title", "需要确认"),
                localized(self, "label-tool", "Tool"),
                self.pending_tool.as_deref().unwrap_or("Unknown"),
                localized(self, "label-summary", "Summary"),
                self.pending_arguments_summary
                    .as_deref()
                    .unwrap_or("No summary"),
                self.pending_confirmation_details
                    .as_deref()
                    .unwrap_or("Confirmation context is unavailable"),
                localized(self, "label-side-effects", "Side effects"),
                effects,
                short_request_id(request_id)
            ));
        }
        self.pending_memory_candidate
            .as_ref()
            .map(|(candidate_id, body_sha256)| {
                format!(
                    "[{}]\n{}: {}\n{}: {}\n\n{}",
                    localized(self, "memory-candidate-title", "Memory candidate"),
                    localized(self, "label-candidate", "Candidate"),
                    candidate_id,
                    localized(self, "label-summary-hash", "Summary hash"),
                    short_request_id(body_sha256),
                    localized(
                        self,
                        "confirmation-actions-once",
                        "y allow once · n/Esc deny"
                    )
                )
            })
    }

    pub fn push_input(&mut self, text: &str) {
        self.input
            .insert_str(self.input_cursor.min(self.input.len()), text);
        self.input_cursor = self.input_cursor.saturating_add(text.len());
    }

    pub fn backspace_input(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let before = &self.input[..self.input_cursor];
        let start = before
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.input.replace_range(start..self.input_cursor, "");
        self.input_cursor = start;
    }

    pub fn move_input_cursor(&mut self, right: bool) {
        let cursor = self.input_cursor.min(self.input.len());
        if right {
            if let Some((index, grapheme)) = self.input[cursor..].grapheme_indices(true).next() {
                self.input_cursor = cursor.saturating_add(index).saturating_add(grapheme.len());
            }
        } else if cursor > 0 {
            let start = self.input[..cursor]
                .grapheme_indices(true)
                .next_back()
                .map(|(index, _)| index)
                .unwrap_or(0);
            self.input_cursor = start;
        }
    }

    pub fn move_input_line_boundary(&mut self, end: bool) {
        let cursor = self.input_cursor.min(self.input.len());
        let line_start = self.input[..cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let line_end = self.input[cursor..]
            .find('\n')
            .map(|index| cursor + index)
            .unwrap_or(self.input.len());
        self.input_cursor = if end { line_end } else { line_start };
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
    }

    pub fn cycle_focus(&mut self, reverse: bool) {
        self.focus = match (self.focus, reverse) {
            (FocusTarget::Conversation, false) => FocusTarget::Activity,
            (FocusTarget::Activity, false) => FocusTarget::Input,
            (FocusTarget::Input, false) => FocusTarget::Drawer,
            (FocusTarget::Drawer, false) => FocusTarget::Conversation,
            (FocusTarget::Conversation, true) => FocusTarget::Drawer,
            (FocusTarget::Activity, true) => FocusTarget::Conversation,
            (FocusTarget::Input, true) => FocusTarget::Activity,
            (FocusTarget::Drawer, true) => FocusTarget::Input,
        };
    }

    pub fn toggle_tool_expanded(&mut self, call_id: &str) {
        if !self.tools.iter().any(|tool| tool.call_id == call_id) {
            return;
        }
        if !self.expanded_tools.insert(call_id.to_owned()) {
            self.expanded_tools.remove(call_id);
        }
    }

    fn record_trace(&mut self, entry: String) {
        self.trace.push(entry);
        if self.trace.len() > 256 {
            let excess = self.trace.len() - 256;
            self.trace.drain(0..excess);
        }
    }
}

fn observation_summary(result: &Value) -> String {
    if let Some(message) = result.get("message").and_then(Value::as_str) {
        return truncate(message, 96);
    }
    if let Some(excerpt) = result.get("excerpt").and_then(Value::as_str) {
        return truncate(excerpt, 96);
    }
    if let Some(fields) = result.as_object() {
        let mut summary = format!("对象，{} 个字段", fields.len());
        if result_is_partial(result) {
            summary.push_str("，部分结果");
        }
        return summary;
    }
    if let Some(items) = result.as_array() {
        return format!("数组，{} 项", items.len());
    }
    let serialized = result.to_string();
    truncate(&serialized, 96)
}

fn result_is_partial(result: &Value) -> bool {
    match result {
        Value::Array(items) => items.iter().any(result_is_partial),
        Value::Object(fields) => {
            ["partial", "truncated"]
                .into_iter()
                .any(|field| fields.get(field).and_then(Value::as_bool).unwrap_or(false))
                || fields.values().any(result_is_partial)
        }
        _ => false,
    }
}

fn collect_evidence(result: &Value, output: &mut Vec<(String, String)>) {
    fn visit(
        value: &Value,
        output: &mut Vec<(String, String)>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        if let Some(object) = value.as_object() {
            if let (Some(id), Some(excerpt)) = (
                object.get("id").and_then(Value::as_str),
                object.get("excerpt").and_then(Value::as_str),
            ) {
                if seen.insert(id.into()) {
                    output.push((id.into(), excerpt.into()));
                }
            }
            object.values().for_each(|child| visit(child, output, seen));
        } else if let Some(items) = value.as_array() {
            items.iter().for_each(|child| visit(child, output, seen));
        }
    }
    visit(result, output, &mut std::collections::HashSet::new());
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut value = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        value.push('…');
    }
    value
}

/// 将模型输出转换为受终端宽度约束的基础 Markdown block 文本。
/// 不引入 ANSI 样式，因此 `NO_COLOR` 下仍保留完整语义。
pub fn render_markdown_blocks(markdown: &str, width: usize) -> String {
    let mut in_code = false;
    let mut lines = Vec::new();
    for raw in markdown.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            lines.push(format!("  {line}"));
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            lines.push(trimmed.trim_start_matches('#').trim_start().to_string());
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            lines.push(format!("• {item}"));
        } else if width < 80 && trimmed.contains('|') {
            let cells = trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .collect::<Vec<_>>();
            if cells.len() >= 2 && !cells.iter().all(|cell| cell.chars().all(|ch| ch == '-')) {
                lines.push(format!("{}: {}", cells[0], cells[1..].join(" / ")));
            }
        } else {
            lines.push(line.to_string());
        }
    }
    lines.join("\n")
}

/// 按终端显示宽度换行，避免 CJK/emoji 或长路径把边框撑出窗口。
pub fn wrap_display_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut output = Vec::new();
    for raw in text.lines() {
        let mut line = String::new();
        let mut used = 0usize;
        for grapheme in raw.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            if used > 0 && used + grapheme_width > width {
                output.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push_str(grapheme);
            used += grapheme_width;
        }
        output.push(line);
    }
    if output.is_empty() {
        output.push(String::new());
    }
    output
}

pub fn truncate_display(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".into();
    }
    let mut output = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme).max(1);
        if used + w + 1 > width {
            break;
        }
        output.push_str(grapheme);
        used += w;
    }
    output.push('…');
    output
}

fn fit_panel_text(text: String, width: usize) -> String {
    text.lines()
        .flat_map(|line| wrap_display_text(line, width.max(1)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn conversation_text(state: &AppState, width: usize) -> String {
    let blocks = if state.conversation.is_empty() {
        if state.answer.is_empty() {
            vec![ConversationBlock {
                id: 0,
                role: "assistant".into(),
                text: String::new(),
                collapsed: false,
            }]
        } else {
            vec![ConversationBlock {
                id: 0,
                role: "assistant".into(),
                text: state.answer.clone(),
                collapsed: false,
            }]
        }
    } else {
        state.conversation.clone()
    };
    let mut lines = Vec::new();
    for block in blocks {
        let wrapped = wrap_display_text(&block.text, width.saturating_sub(4).max(1));
        // Claude 风格的内联气泡：助手 ●、用户 >、其它 •；当前选中项用 ❯ 高亮。
        let bullet = match block.role.as_str() {
            "user" => ">",
            "assistant" => "●",
            _ => "•",
        };
        let selected = block.id == state.selected_conversation;
        lines.push(format!(
            "{} {}",
            if selected { "❯" } else { bullet },
            block.role
        ));
        if block.collapsed {
            let collapsed = localized(
                state,
                "conversation-collapsed",
                "… Collapsed · Enter expand",
            );
            lines.push(format!("  {collapsed} ({} lines)", wrapped.len()));
        } else {
            lines.extend(wrapped.into_iter().map(|line| format!("  {line}")));
        }
    }
    lines.join("\n")
}

pub fn main_scroll_offset(
    text: &str,
    viewport_height: u16,
    viewport_width: u16,
    follow_bottom: bool,
    current: u16,
) -> u16 {
    if !follow_bottom {
        return current;
    }
    let width = usize::from(viewport_width.max(1));
    let content_lines = text
        .lines()
        .map(|line| display_width(line).max(1).div_ceil(width))
        .sum::<usize>()
        .max(1);
    content_lines
        .saturating_sub(usize::from(viewport_height.max(1)))
        .min(usize::from(u16::MAX)) as u16
}

pub fn render(frame: &mut Frame<'_>, state: &AppState) {
    if layout_profile(frame.size().width, frame.size().height) == LayoutProfile::TooSmall {
        let size = frame.size();
        frame.render_widget(
            Paragraph::new(format!(
                "{} {}x{}; {} 60x20. {}",
                localized(state, "error-terminal-too-small", "Terminal is too small:"),
                size.width,
                size.height,
                localized(state, "label-minimum", "minimum"),
                localized(
                    state,
                    "terminal-resize-hint",
                    "Resize the terminal to continue."
                )
            ))
            .block(panel_block(COMPACT_BRAND, true)),
            size,
        );
        return;
    }
    if let Some(startup) = &state.startup {
        let area = centered_rect(82, 82, frame.size());
        frame.render_widget(ratatui::widgets::Clear, area);
        frame.render_widget(
            panel_block(
                localized(state, "startup-title", "Rivo · Startup Configuration"),
                true,
            ),
            area,
        );
        let inner = area.inner(Margin {
            vertical: 1,
            horizontal: 2,
        });
        let banner = brand_banner_with_tagline(
            if inner.height < 26 {
                0
            } else {
                inner.width as usize
            },
            unicode_enabled(),
            &localized(
                state,
                "startup-tagline",
                "让智能体在你的终端里工作 · 选择配置后按 Enter 开始",
            ),
        );
        let banner_height = banner.len() as u16;
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(banner_height + 1), Constraint::Min(1)])
            .split(inner);
        frame.render_widget(Paragraph::new(banner).alignment(Alignment::Center), rows[0]);
        let text = fit_panel_text(
            startup.lines_with_messages(&state.messages).join("\n"),
            rows[1].width as usize,
        );
        let selected_line = text
            .lines()
            .position(|line| line.starts_with("> "))
            .unwrap_or(0);
        let scroll = selected_line
            .saturating_sub(usize::from(rows[1].height.saturating_sub(1)))
            .min(usize::from(u16::MAX)) as u16;
        frame.render_widget(Paragraph::new(text).scroll((scroll, 0)), rows[1]);
        return;
    }
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(frame.size());
    // 顶部品牌 header：左侧 Rivo，右侧会话 / 模型 / context / 阶段指示。
    let brand = COMPACT_BRAND;
    let header_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(display_width(brand) as u16 + 2),
            Constraint::Min(1),
        ])
        .split(areas[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(brand, brand_style()))),
        header_split[0],
    );
    let (dot, phase_label) = phase_indicator(state.turn_phase);
    let phase_label = match state.turn_phase {
        TurnPhase::Idle => localized(state, "phase-idle", phase_label),
        TurnPhase::Running => localized(state, "phase-running", phase_label),
        TurnPhase::WaitingConfirmation => localized(state, "phase-waiting", phase_label),
        TurnPhase::Cancelling => localized(state, "phase-cancelling", phase_label),
        TurnPhase::Finished => localized(state, "phase-finished", phase_label),
        TurnPhase::Unknown => localized(state, "phase-error", phase_label),
    };
    let ratio = context_ratio(state.used_tokens, state.context_limit_tokens);
    let mut status_spans = vec![
        Span::styled(
            format!(
                "{} · ",
                state.workspace_name.as_deref().unwrap_or("Workspace")
            ),
            dim_style(),
        ),
        Span::styled(
            format!(
                "{} · {} · ",
                state.session_id.as_deref().unwrap_or("new"),
                state.model.as_deref().unwrap_or("unselected"),
            ),
            dim_style(),
        ),
        Span::styled(
            format!("⛁ {:.0}% context", ratio * 100.0),
            budget_style(ratio),
        ),
    ];
    if !state.diagnostics.is_empty() {
        status_spans.push(Span::styled(
            format!(
                " · ⚠ {} {}",
                state.diagnostics.len(),
                localized(state, "label-errors", "errors")
            ),
            styled(Style::default().fg(theme::ERR)),
        ));
    }
    status_spans.push(Span::styled(" · ", dim_style()));
    status_spans.push(Span::styled(
        format!("{} {}", dot, phase_label),
        phase_style(state.turn_phase),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(status_spans)).alignment(Alignment::Right),
        header_split[1],
    );
    let confirmation_detail = state.confirmation_detail();
    let confirmation = String::new();
    let main_width = match layout_profile(frame.size().width, frame.size().height) {
        LayoutProfile::Wide | LayoutProfile::Overlay => frame.size().width.saturating_sub(28),
        _ => frame.size().width,
    } as usize;
    let answer = render_markdown_blocks(
        &conversation_text(state, main_width.saturating_sub(2)),
        main_width.saturating_sub(2),
    );
    let main_text = format!(
        "{}{}{}",
        answer,
        confirmation,
        if state.notice.is_empty() {
            String::new()
        } else {
            format!("\n\n{}", state.notice)
        }
    );
    let main_scroll = main_scroll_offset(
        &main_text,
        areas[1].height.saturating_sub(2),
        main_width.min(u16::MAX as usize) as u16,
        state.follow_bottom,
        state.main_scroll,
    );
    // Claude 风格：会话区去边框，靠内联气泡（●/>/⏺/⎿）与留白区分角色。
    let sidebar_focused = matches!(state.focus, FocusTarget::Activity | FocusTarget::Drawer);
    let main = Paragraph::new(main_text).scroll((main_scroll, state.main_horizontal));
    if layout_profile(frame.size().width, frame.size().height) == LayoutProfile::Wide {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(28)])
            .split(areas[1]);
        frame.render_widget(main, split[0]);
        frame.render_widget(
            Paragraph::new(fit_panel_text(
                sidebar_text(state),
                split[1].width.saturating_sub(2) as usize,
            ))
            .scroll((state.sidebar_scroll, state.sidebar_horizontal))
            .block(panel_block(
                localized(state, "sidebar-title", "● Status"),
                sidebar_focused,
            )),
            split[1],
        );
    } else {
        frame.render_widget(main, areas[1]);
        if layout_profile(frame.size().width, frame.size().height) == LayoutProfile::Overlay
            && state.focus == FocusTarget::Drawer
        {
            let drawer = centered_rect(72, 70, areas[1]);
            frame.render_widget(ratatui::widgets::Clear, drawer);
            frame.render_widget(
                Paragraph::new(fit_panel_text(
                    sidebar_text(state),
                    drawer.width.saturating_sub(2) as usize,
                ))
                .scroll((state.sidebar_scroll, state.sidebar_horizontal))
                .block(panel_block(
                    localized(state, "drawer-title", "● Activity / Errors · Esc Back"),
                    true,
                )),
                drawer,
            );
        }
    }
    let input_viewport_width = areas[2].width.saturating_sub(2) as usize;
    let input_viewport_height = areas[2].height.saturating_sub(2) as usize;
    let input_cursor = input_cursor_layout(
        &state.input,
        state.input_cursor,
        input_viewport_width,
        input_viewport_height,
    );
    frame.render_widget(
        Paragraph::new(if state.input.is_empty() {
            Span::styled(
                localized(state, "input-placeholder", "› Enter a task or /help"),
                dim_style(),
            )
        } else {
            Span::raw(state.input.as_str())
        })
        .scroll((
            input_cursor.vertical_scroll.min(u16::MAX as usize) as u16,
            input_cursor.horizontal_scroll.min(u16::MAX as usize) as u16,
        ))
        .block(panel_block(
            localized(
                state,
                "input-title",
                "› Input · Enter Submit · Ctrl+J New Line",
            ),
            state.focus == FocusTarget::Input,
        )),
        areas[2],
    );
    if state.focus == FocusTarget::Input
        && state.active_overlay() != Some(ActiveOverlay::CommandPalette)
    {
        let x = areas[2]
            .x
            .saturating_add(1)
            .saturating_add(
                input_cursor
                    .column
                    .saturating_sub(input_cursor.horizontal_scroll)
                    .min(u16::MAX as usize) as u16,
            )
            .min(areas[2].right().saturating_sub(1));
        let y = areas[2]
            .y
            .saturating_add(1)
            .saturating_add(
                input_cursor
                    .line
                    .saturating_sub(input_cursor.vertical_scroll)
                    .min(u16::MAX as usize) as u16,
            )
            .min(areas[2].bottom().saturating_sub(1));
        frame.set_cursor(x, y);
    }
    // 底部 dim 快捷键提示条，随当前焦点/弹层给出上下文快捷键。
    let keyhint = match state.active_overlay() {
        Some(ActiveOverlay::CommandPalette) => localized(
            state,
            "palette-keyhint",
            "↑/↓ Select · Tab Complete · Enter Run · Esc Close",
        ),
        Some(ActiveOverlay::SessionSettings) => localized(
            state,
            "settings-keyhint",
            "Arrow/Tab Browse · Enter Edit · Space Toggle · Ctrl+Enter Apply · Esc Cancel",
        ),
        Some(ActiveOverlay::WorkspaceSelector) => localized(
            state,
            "workspace-keyhint",
            "↑/↓ Select · Enter Open · Esc Close",
        ),
        Some(ActiveOverlay::Help) => {
            localized(state, "help-keyhint", "↑/↓ Scroll · Esc Close Help")
        }
        Some(ActiveOverlay::Confirmation) => localized(
            state,
            "confirm-keyhint",
            "y Accept · n Deny · Enter Confirm · Esc Close",
        ),
        Some(ActiveOverlay::Interaction) => localized(
            state,
            "interaction-keyhint",
            "↑/↓ Select · 1-9 Quick Select · Enter Submit",
        ),
        Some(ActiveOverlay::DiagnosticDetail) => {
            localized(state, "diagnostic-keyhint", "r Retry · Enter/Esc Back")
        }
        None => localized(
            state,
            "default-keyhint",
            "/ 命令 · F6 设置 · Tab 切换焦点 · ↑/↓ 滚动 · ? 帮助 · Esc 清空",
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {keyhint}"), dim_style()))),
        areas[3],
    );
    let active_overlay = state.active_overlay();
    if active_overlay == Some(ActiveOverlay::CommandPalette) {
        let popup = ratatui::widgets::Clear;
        frame.render_widget(popup, frame.size());
        let (start, suggestions) = state.command_suggestion_window(8);
        let selected = state
            .command_index
            .min(state.command_suggestions().len().saturating_sub(1));
        let suggestions = suggestions
            .into_iter()
            .enumerate()
            .map(|(index, command)| {
                format!(
                    "{} /{}",
                    if start + index == selected {
                        "> "
                    } else {
                        "  "
                    },
                    command
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(
            Paragraph::new(format!(
                "> {}\n{}\n{}",
                state.command_input,
                suggestions,
                localized(
                    state,
                    "palette-keyhint",
                    "↑/↓ Select · Tab Complete · Enter Run · Esc Close"
                )
            ))
            .block(panel_block(
                localized(state, "command-palette-title", "✻ Command Palette"),
                true,
            )),
            centered_rect(60, 30, frame.size()),
        );
    }
    if active_overlay == Some(ActiveOverlay::SessionSettings) {
        let settings = state
            .session_settings
            .as_ref()
            .expect("active settings overlay");
        let profile = layout_profile(frame.size().width, frame.size().height);
        let columns = match profile {
            LayoutProfile::Wide => 4,
            LayoutProfile::Overlay => 2,
            LayoutProfile::Compact | LayoutProfile::TooSmall => 1,
        };
        let mut lines = vec![
            localized(
                state,
                "settings-scope",
                "Changes affect subsequent turns in this session only.",
            ),
            String::new(),
        ];
        let groups: &[(&str, &str, &[usize])] = &[
            ("settings-group-routing", "MODEL ROUTING", &[0, 1, 2, 3]),
            ("settings-group-runtime", "RUNTIME", &[4, 5, 6, 7, 8]),
            ("settings-group-context", "CONTEXT", &[9, 10, 11, 12]),
            ("settings-group-agents", "AGENTS", &[13, 14]),
        ];
        for (title_id, fallback_title, fields) in groups {
            lines.push(format!("[{}]", localized(state, title_id, fallback_title)));
            for chunk in fields.chunks(columns) {
                lines.push(
                    chunk
                        .iter()
                        .map(|field| {
                            let marker = if *field == settings.field { "❯" } else { " " };
                            format!(
                                "{marker} {}: {}",
                                SessionSettingsState::field_label(*field),
                                session_setting_render_value(state, settings, *field)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("    "),
                );
            }
            lines.push(String::new());
        }
        lines.push(format!(
            "{}: {}",
            localized(state, "settings-status", "Status"),
            match settings.mode {
                SessionSettingsMode::Browse => localized(
                    state,
                    "settings-mode-browse",
                    "Browse · Enter edit · Space toggle"
                ),
                SessionSettingsMode::EditField => localized(
                    state,
                    "settings-mode-edit-field",
                    "Edit field · Arrow preview · Enter confirm · Esc cancel"
                ),
                SessionSettingsMode::EditText => localized(
                    state,
                    "settings-mode-edit-text",
                    "Edit text · Arrow move cursor · Enter confirm · Esc cancel"
                ),
                SessionSettingsMode::MultiSelect => localized(
                    state,
                    "settings-mode-multiselect",
                    "Multi-select · Space toggle · Enter confirm · Esc cancel"
                ),
            }
        ));
        if settings.mode != SessionSettingsMode::Browse {
            lines.push(String::new());
            lines.extend(session_setting_editor_lines(state, settings));
        }
        if !state.notice.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "{}: {}",
                localized(state, "settings-hint", "Notice"),
                state.notice
            ));
        }
        let detail = lines.join("\n");
        let popup = centered_rect(94, 88, frame.size());
        frame.render_widget(ratatui::widgets::Clear, popup);
        frame.render_widget(
            Paragraph::new(fit_panel_text(
                detail,
                popup.width.saturating_sub(2) as usize,
            ))
            .scroll((state.overlay_scroll, 0))
            .block(panel_block(
                localized(state, "settings-title", "✻ Session Settings"),
                true,
            )),
            popup,
        );
    }
    if active_overlay == Some(ActiveOverlay::WorkspaceSelector) {
        let selector = state
            .workspace_selector
            .as_ref()
            .expect("active workspace selector overlay");
        let body = if selector.workspaces.is_empty() {
            localized(state, "workspace-empty", "No registered workspaces")
        } else {
            selector
                .workspaces
                .iter()
                .enumerate()
                .map(|(index, workspace)| {
                    format!(
                        "{} {}{}\n  {}\n  {}",
                        if index == selector.selected {
                            "❯"
                        } else {
                            " "
                        },
                        workspace.display_name,
                        if workspace.is_default {
                            " [default]"
                        } else {
                            ""
                        },
                        workspace.id,
                        workspace.root_path
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let popup = centered_rect(82, 68, frame.size());
        frame.render_widget(ratatui::widgets::Clear, popup);
        frame.render_widget(
            Paragraph::new(fit_panel_text(body, popup.width.saturating_sub(2) as usize)).block(
                panel_block(localized(state, "workspace-title", "✻ Workspace"), true),
            ),
            popup,
        );
    }
    if active_overlay == Some(ActiveOverlay::Help) {
        let help = localized(state, "help-body", "/ or F1 Command Palette\nF6 Session Settings\nUse arrows or Tab to navigate; Enter confirms or opens details; Esc closes overlays.\n\nCommands: /exit /cancel /retry /sessions /new /errors /permissions /language /settings /provider /model /network /browser /mcp /a2a /subagents /trace /memory");
        let popup = centered_rect(76, 72, frame.size());
        frame.render_widget(ratatui::widgets::Clear, popup);
        frame.render_widget(
            Paragraph::new(fit_panel_text(help, popup.width.saturating_sub(2) as usize))
                .scroll((state.overlay_scroll, 0))
                .block(panel_block(localized(state, "help-title", "Help"), true)),
            popup,
        );
    }
    if active_overlay == Some(ActiveOverlay::Confirmation) {
        let detail = confirmation_detail.unwrap_or_default();
        let popup = centered_rect(76, 58, frame.size());
        frame.render_widget(ratatui::widgets::Clear, popup);
        frame.render_widget(
            Paragraph::new(fit_panel_text(
                detail,
                popup.width.saturating_sub(2) as usize,
            ))
            .scroll((state.overlay_scroll, 0))
            .block(panel_block(
                localized(state, "approval-title", "Approval required"),
                true,
            )),
            popup,
        );
    }
    if active_overlay == Some(ActiveOverlay::DiagnosticDetail) {
        if let Some(error) = state.diagnostics.get(state.selected_diagnostic) {
            let detail = format!(
                "{}: {}\n{}: {}\n{}: {}\n{}: {}\n\n{}:\n{}\n\n{}:\n{}\n\n{}",
                localized(state, "label-stage", "Stage"),
                error.stage,
                localized(state, "label-error-code", "Error code"),
                error.code,
                localized(state, "label-retryable", "Retryable"),
                error.retryable,
                localized(state, "label-recovery", "Recovery"),
                error.recovery,
                localized(state, "label-message", "Message"),
                error.message,
                localized(state, "label-full-cause", "Full cause"),
                error.cause,
                localized(state, "diagnostic-keyhint", "r Retry · Enter/Esc Back")
            );
            let popup = centered_rect(82, 72, frame.size());
            frame.render_widget(ratatui::widgets::Clear, popup);
            frame.render_widget(
                Paragraph::new(fit_panel_text(
                    detail,
                    popup.width.saturating_sub(2) as usize,
                ))
                .scroll((state.overlay_scroll, 0))
                .block(panel_block(
                    localized(state, "diagnostic-title", "Error detail"),
                    true,
                )),
                popup,
            );
        }
    }
    if active_overlay == Some(ActiveOverlay::Interaction) {
        let interaction = state
            .interaction
            .as_ref()
            .expect("active interaction overlay");
        let mut lines = vec![
            interaction.request.title.clone(),
            String::new(),
            interaction.request.body.clone(),
            String::new(),
        ];
        for (index, option) in interaction.request.options.iter().enumerate() {
            let marker = if index == interaction.selected {
                "❯"
            } else {
                " "
            };
            lines.push(format!(
                "{marker} {}. {} — {}",
                index + 1,
                option.label,
                option.description
            ));
        }
        if interaction.request.allow_freeform {
            lines.push(format!(
                "{}: {}",
                localized(state, "label-freeform", "Free-form input"),
                interaction.freeform
            ));
        }
        lines.push(String::new());
        lines.push(localized(
            state,
            "interaction-detail-keyhint",
            "↑/↓ select · 1-9 quick select · Enter submit · Esc requires an explicit response",
        ));
        let popup = centered_rect(82, 72, frame.size());
        frame.render_widget(ratatui::widgets::Clear, popup);
        frame.render_widget(
            Paragraph::new(fit_panel_text(
                lines.join("\n"),
                popup.width.saturating_sub(2) as usize,
            ))
            .scroll((state.overlay_scroll, 0))
            .block(panel_block(
                localized(
                    state,
                    "interaction-title",
                    "✻ Interaction · response required",
                ),
                true,
            )),
            popup,
        );
    }
}

fn session_setting_candidates_for_view(
    state: &AppState,
    settings: &SessionSettingsState,
    field: usize,
) -> Vec<String> {
    match field {
        0 => {
            let mut values = state
                .catalog_models
                .iter()
                .map(|m| m.vendor.clone())
                .collect::<Vec<_>>();
            values.sort();
            values.dedup();
            values
        }
        1 => state
            .catalog_models
            .iter()
            .filter(|m| m.vendor == settings.selection.vendor)
            .map(|m| m.id.clone())
            .collect(),
        2 => {
            let mut values = vec!["Auto".into()];
            if let Some(model) = state
                .catalog_models
                .iter()
                .find(|m| m.id == settings.selection.model)
            {
                values.extend(model.providers.iter().map(|p| p.id.clone()));
            }
            values
        }
        3 => {
            let mut values = vec!["default".into()];
            if let Some(model) = state
                .catalog_models
                .iter()
                .find(|m| m.id == settings.selection.model)
            {
                values.extend(
                    model
                        .supported_reasoning_efforts
                        .iter()
                        .map(|v| v.as_str().into()),
                );
            }
            values
        }
        4 => ["general", "research", "plan", "coding", "review"]
            .into_iter()
            .map(String::from)
            .collect(),
        5 => ["strict", "medium", "full_control"]
            .into_iter()
            .map(String::from)
            .collect(),
        6..=8 | 11..=13 => vec!["off".into(), "on".into()],
        _ => Vec::new(),
    }
}

fn session_setting_render_value(
    state: &AppState,
    settings: &SessionSettingsState,
    field: usize,
) -> String {
    if field == settings.field {
        match settings.mode {
            SessionSettingsMode::EditField => {
                if let Some(value) = session_setting_candidates_for_view(state, settings, field)
                    .get(settings.candidate_index)
                {
                    return format!("[{value}]");
                }
            }
            SessionSettingsMode::EditText => return format!("[{}]", settings.edit_buffer),
            SessionSettingsMode::MultiSelect => {
                return format!("[{} selected]", settings.multi_selected.len())
            }
            SessionSettingsMode::Browse => {}
        }
    }
    settings.value(field)
}

fn session_setting_editor_lines(state: &AppState, settings: &SessionSettingsState) -> Vec<String> {
    match settings.mode {
        SessionSettingsMode::EditText => vec![
            format!(
                "{}: {}▏",
                SessionSettingsState::field_label(settings.field),
                settings.edit_buffer
            ),
            localized(
                state,
                "settings-edit-number",
                "Enter a number, then press Enter to confirm",
            ),
        ],
        SessionSettingsMode::MultiSelect => {
            if settings.multi_items.is_empty() {
                return vec![localized(
                    state,
                    "settings-no-options",
                    "No options available; create configuration with the corresponding command first.",
                )];
            }
            let mut lines = vec![SessionSettingsState::field_label(settings.field).into()];
            lines.extend(
                settings
                    .multi_items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        format!(
                            "{} [{}] {}",
                            if index == settings.candidate_index {
                                "❯"
                            } else {
                                " "
                            },
                            if settings.multi_selected.contains(item) {
                                "x"
                            } else {
                                " "
                            },
                            item
                        )
                    }),
            );
            lines
        }
        SessionSettingsMode::EditField => {
            let values = session_setting_candidates_for_view(state, settings, settings.field);
            let mut lines = vec![format!(
                "编辑 {}",
                SessionSettingsState::field_label(settings.field)
            )];
            if settings.field == 1 {
                lines.extend(values.iter().enumerate().filter_map(|(index, value)| {
                    state
                        .catalog_models
                        .iter()
                        .find(|model| &model.id == value)
                        .map(|model| {
                            let providers = if model.providers.is_empty() {
                                "discovery: none".into()
                            } else {
                                model
                                    .providers
                                    .iter()
                                    .map(|p| format!("{}:{}", p.id, p.status))
                                    .collect::<Vec<_>>()
                                    .join(",")
                            };
                            let reasoning = if model.supported_reasoning_efforts.is_empty() {
                                "default".into()
                            } else {
                                model
                                    .supported_reasoning_efforts
                                    .iter()
                                    .map(|e| e.as_str())
                                    .collect::<Vec<_>>()
                                    .join("/")
                            };
                            format!(
                                "{} {} ({}) · providers:{} · reasoning:{}",
                                if index == settings.candidate_index {
                                    "❯"
                                } else {
                                    " "
                                },
                                model.display_name,
                                model.id,
                                providers,
                                reasoning
                            )
                        })
                }));
            } else {
                lines.extend(values.iter().enumerate().map(|(index, value)| {
                    format!(
                        "{} {}",
                        if index == settings.candidate_index {
                            "❯"
                        } else {
                            " "
                        },
                        value
                    )
                }));
            }
            lines
        }
        SessionSettingsMode::Browse => Vec::new(),
    }
}

fn sidebar_text(state: &AppState) -> String {
    let ratio = context_ratio(state.used_tokens, state.context_limit_tokens);
    let mut lines = vec![
        localized(state, "sidebar-session", "── 会话 ──"),
        format!(
            "{} · {}",
            localized(state, "sidebar-user-selection", "用户选择"),
            state.selection.as_ref().map_or_else(
                || localized(state, "label-unselected", "未选择"),
                |selection| format!(
                    "{} / {} · {}",
                    selection.vendor,
                    selection.model,
                    provider_label(&selection.provider)
                )
            )
        ),
        format!(
            "{} · {}",
            localized(state, "sidebar-resolved-route", "已确认路由"),
            state.route.as_ref().map_or_else(
                || localized(
                    state,
                    "sidebar-route-pending",
                    "未确认（等待 Session 路由）"
                ),
                |route| format!(
                    "{} / {} → {} ({}, priority={})",
                    route.vendor, route.model, route.provider_id, route.protocol, route.priority
                )
            )
        ),
        format!(
            "{} · {}",
            localized(state, "label-model", "模型"),
            state.model.as_deref().unwrap_or("未启动")
        ),
        format!(
            "{} · {:?}",
            localized(state, "label-runtime-status", "运行状态"),
            state.mode.unwrap_or(UiMode::Idle)
        ),
        String::new(),
        localized(state, "sidebar-policy", "── 策略 ──"),
        format!(
            "{}: {}",
            localized(state, "label-capability-mode", "能力模式"),
            state.turn_options.capability_profile
        ),
        format!(
            "{}: {}",
            localized(state, "label-safety-policy", "安全策略"),
            state.turn_options.policy_profile
        ),
        String::new(),
        localized(state, "sidebar-runtime", "── 运行时 ──"),
        format!(
            "{}: {}",
            localized(state, "label-network", "网络"),
            state.turn_options.network
        ),
        format!(
            "{}: {}",
            localized(state, "label-browser", "浏览器"),
            state.turn_options.browser
        ),
        format!(
            "MCP: {}",
            if state.turn_options.mcp_servers.is_empty() {
                localized(state, "label-off", "关闭")
            } else {
                state.turn_options.mcp_servers.join(", ")
            }
        ),
        format!(
            "{}: {}",
            localized(state, "label-memory", "记忆"),
            state.turn_options.memory_recall
        ),
        format!("Trace: {}", state.turn_options.trace),
        String::new(),
        localized(state, "sidebar-budget", "── 预算 ──"),
        format!("{} {:.0}%", inline_gauge(ratio, 12), ratio * 100.0),
        format!(
            "{}: {}/{} tokens, {} {}/{}",
            localized(state, "label-budget", "预算"),
            state.used_tokens,
            state.context_limit_tokens,
            localized(state, "label-steps", "步骤"),
            state.steps,
            state.max_steps
        ),
        String::new(),
    ];
    match state.sidebar {
        SidebarView::Tools => {
            lines.insert(0, "[tools]".into());
            for tool in &state.tools {
                lines.push(format!("{} {}", tool.status.label(), tool.name));
                lines.push(tool.summary.clone());
                if state.expanded_tools.contains(&tool.call_id) {
                    if let Some(details) = state.tool_details.get(&tool.call_id) {
                        if !details.is_empty() {
                            lines.push(format!("  {details}"));
                        }
                    }
                }
            }
        }
        SidebarView::Trace => {
            lines.insert(0, "[trace]".into());
            lines.extend(state.trace.iter().rev().take(32).cloned());
        }
        SidebarView::Evidence => {
            lines.insert(0, "[evidence]".into());
            if state.evidence.is_empty() {
                lines.push("暂无证据".into());
            } else {
                lines.extend(state.evidence.iter().cloned());
            }
        }
        SidebarView::History => {
            lines.insert(0, "[history]".into());
            if let Some(workspace) = &state.workspace_name {
                lines.push(format!("Workspace: {workspace}"));
            }
            if state.history.is_empty() {
                lines.push("暂无历史".into());
            } else {
                lines.extend(state.history.iter().enumerate().flat_map(|(index, entry)| {
                    let status = if entry.expired {
                        format!(
                            "已过期：{}",
                            entry.expiration_reason.as_deref().unwrap_or("本地留存策略")
                        )
                    } else {
                        "可恢复".into()
                    };
                    let summary = entry
                        .task_summary
                        .as_deref()
                        .unwrap_or("尚无已持久化任务")
                        .replace(['\n', '\r', '\t'], " ");
                    vec![
                        format!(
                            "{} {} · {}",
                            if index == state.selected_history {
                                ">"
                            } else {
                                " "
                            },
                            entry.created_at,
                            summary
                        ),
                        format!(
                            "  {} e={} m={} · {}",
                            entry.session_id, entry.event_count, entry.message_count, status
                        ),
                    ]
                }));
                lines.push("↑/↓ 选择 · Enter 恢复 · /new 新建会话".into());
            }
        }
        SidebarView::Errors => {
            lines.insert(0, "[errors]".into());
            if state.diagnostics.is_empty() {
                lines.push("暂无错误记录".into());
            } else {
                for (index, error) in state
                    .diagnostics
                    .iter()
                    .enumerate()
                    .rev()
                    .filter(|(_, item)| {
                        state.diagnostic_filter.is_empty()
                            || item.stage.contains(&state.diagnostic_filter)
                            || item.code.contains(&state.diagnostic_filter)
                            || item.message.contains(&state.diagnostic_filter)
                    })
                    .take(64)
                {
                    let marker = if index == state.selected_diagnostic {
                        ">"
                    } else {
                        " "
                    };
                    lines.push(format!(
                        "{marker} [{}:{}] {}",
                        error.stage, error.code, error.message
                    ));
                    lines.push(format!(
                        "  retry={} next={}",
                        error.retryable, error.recovery
                    ));
                }
            }
        }
    }
    lines.join("\n")
}

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    area: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn short_request_id(request_id: &str) -> String {
    request_id.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivo_policy::{CapabilityProfile, PolicyProfile};
    use rivo_policy::{ExecutionConstraints, NetworkScope, PathScope};
    use rivo_session::{ProviderKind, StopReason, Usage};

    fn started(turn_id: &str) -> AgentEvent {
        AgentEvent::Started {
            provider: ProviderKind::OpenAiChatCompletions,
            model: "m".into(),
            selection: None,
            route: None,
            turn_id: turn_id.into(),
            capability_profile: CapabilityProfile::General,
            policy_profile: PolicyProfile::Workspace,
        }
    }

    fn final_answer(text: &str) -> AgentEvent {
        AgentEvent::FinalAnswer {
            text: text.into(),
            stop_reason: StopReason::Final,
            usage: Usage::default(),
        }
    }

    #[test]
    fn event_queue_merges_deltas_and_preserves_terminal_events() {
        let mut queue = EventQueue::new(4);
        queue.push(AgentEvent::ModelDelta { text: "a".into() });
        queue.push(AgentEvent::ModelDelta { text: "b".into() });
        assert_eq!(queue.len(), 1);
        queue.push(AgentEvent::ToolRequested {
            call_id: "call".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({}),
        });
        queue.push(AgentEvent::Error {
            message: "failed".into(),
        });
        queue.push(AgentEvent::FinalAnswer {
            text: "done".into(),
            stop_reason: StopReason::Final,
            usage: Usage::default(),
        });
        let events = queue.drain().collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error { message } if message == "failed"
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::FinalAnswer { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ModelDelta { text } if text == "ab"
        )));
    }
    #[test]
    fn subagent_lifecycle_is_urgent_and_uses_only_bounded_trace_metadata() {
        let result = rivo_core::SubagentResult {
            agent_id: rivo_core::AgentId("agent-1".into()),
            task_id: rivo_core::AgentTaskId("task-1".into()),
            state: rivo_core::AgentTaskState::Completed,
            summary: "provider-private-summary".into(),
            evidence: Vec::new(),
            truncated: false,
            usage: rivo_core::SubagentUsage {
                input_tokens: 2,
                output_tokens: 3,
                steps: 1,
            },
        };
        let mut queue = EventQueue::new(3);
        queue.push(AgentEvent::ModelDelta {
            text: "dropped".into(),
        });
        queue.push(AgentEvent::SubagentQueued {
            agent_id: result.agent_id.clone(),
            task_id: result.task_id.clone(),
            call_id: "call-audit".into(),
            label: "audit".into(),
        });
        queue.push(AgentEvent::SubagentStarted {
            agent_id: result.agent_id.clone(),
            task_id: result.task_id.clone(),
        });
        queue.push(AgentEvent::SubagentCompleted {
            result: result.clone(),
        });
        assert!(queue.has_urgent());
        assert_eq!(queue.drain().count(), 3);

        let mut state = AppState::default();
        state.reduce(AgentEvent::SubagentQueued {
            agent_id: result.agent_id.clone(),
            task_id: result.task_id.clone(),
            call_id: "call-audit".into(),
            label: "audit".into(),
        });
        state.reduce(AgentEvent::SubagentStarted {
            agent_id: result.agent_id.clone(),
            task_id: result.task_id.clone(),
        });
        state.reduce(AgentEvent::SubagentCompleted { result });
        assert!(state.notice.contains("Completed"));
        assert!(state
            .trace
            .iter()
            .any(|item| item.contains("subagent completed")));
        assert!(!state.trace.join("\n").contains("provider-private-summary"));
    }

    #[test]
    fn reducer_keeps_stream_text() {
        let mut s = AppState::default();
        s.reduce(started("turn-1"));
        s.reduce(AgentEvent::ModelDelta {
            text: "你好".into(),
        });
        s.reduce(final_answer("你好"));
        assert_eq!(s.answer, "你好");
        assert_eq!(s.mode, Some(UiMode::Finished));
    }

    #[test]
    fn reducer_preserves_multiple_turns_and_keeps_deltas_in_their_turn() {
        let mut state = AppState::default();
        state.append_user_prompt("第一问".into());
        state.reduce(started("turn-1"));
        assert_eq!(state.conversation[0].text, "第一问");
        state.reduce(AgentEvent::ModelDelta {
            text: "第一".into(),
        });
        state.reduce(AgentEvent::ModelDelta { text: "答".into() });
        state.reduce(final_answer("第一答"));

        state.append_user_prompt("第二问".into());
        state.reduce(started("turn-2"));
        state.reduce(AgentEvent::ModelDelta {
            text: "第二".into(),
        });
        state.reduce(AgentEvent::ModelDelta { text: "答".into() });
        state.reduce(final_answer("第二答"));

        let messages = state
            .conversation
            .iter()
            .map(|block| (block.role.as_str(), block.text.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            vec![
                ("user", "第一问"),
                ("assistant", "第一答"),
                ("user", "第二问"),
                ("assistant", "第二答"),
            ]
        );
        assert_eq!(state.selected_conversation, 3);
        assert!(state.active_assistant_block.is_none());
    }

    #[test]
    fn retry_without_a_new_user_message_gets_a_distinct_assistant_block() {
        let mut state = AppState::default();
        state.append_user_prompt("请处理".into());
        state.reduce(started("turn-1"));
        state.reduce(AgentEvent::ModelDelta {
            text: "未完成".into(),
        });
        state.reduce(AgentEvent::Stopped {
            reason: "连接中断".into(),
        });

        state.reduce(started("turn-retry"));
        state.reduce(AgentEvent::ModelDelta {
            text: "已完成".into(),
        });
        state.reduce(final_answer("已完成"));

        assert_eq!(state.conversation.len(), 3);
        assert_eq!(state.conversation[1].text, "未完成");
        assert_eq!(state.conversation[2].text, "已完成");
        assert_eq!(state.conversation[1].role, "assistant");
        assert_eq!(state.conversation[2].role, "assistant");
    }

    #[test]
    fn final_answer_without_deltas_creates_the_assistant_message() {
        let mut state = AppState::default();
        state.append_user_prompt("直接回答".into());
        state.reduce(started("turn-1"));
        state.reduce(final_answer("最终答案"));

        assert_eq!(state.conversation.len(), 2);
        assert_eq!(state.conversation[1].role, "assistant");
        assert_eq!(state.conversation[1].text, "最终答案");
        assert_eq!(state.answer, "最终答案");
    }

    #[test]
    fn render_shows_messages_from_multiple_turns() {
        let mut state = AppState::default();
        state.append_user_prompt("第一轮问题".into());
        state.reduce(started("turn-1"));
        state.reduce(final_answer("第一轮回答"));
        state.append_user_prompt("第二轮问题".into());
        state.reduce(started("turn-2"));
        state.reduce(final_answer("第二轮回答"));

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = output.split_whitespace().collect::<String>();
        assert!(compact.contains("第一轮问题"));
        assert!(compact.contains("第一轮回答"));
        assert!(compact.contains("第二轮问题"));
        assert!(compact.contains("第二轮回答"));
    }

    #[test]
    fn header_shows_brand_logo_and_context_gauge() {
        let state = AppState {
            answer: "回答".into(),
            used_tokens: 50,
            context_limit_tokens: 100,
            ..AppState::default()
        };
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = output.split_whitespace().collect::<String>();
        // 左上角品牌 logo、context gauge 与底部快捷键提示条都应出现。
        assert!(compact.contains("Rivo"));
        assert!(!output.contains("✻ Rivo"));
        assert!(output.contains("context"));
        assert!(output.contains("50% context"));
        assert!(compact.contains("?帮助"));
    }

    #[test]
    fn app_state_keeps_the_sidecar_host_capability_projection() {
        let state = AppState {
            sidecar_host_capabilities: Some(HostCapabilitySummary {
                capabilities: vec!["process.timeout".into()],
                unsupported: vec!["terminal.pty".into()],
            }),
            ..AppState::default()
        };
        let capabilities = state.sidecar_host_capabilities.as_ref().unwrap();
        assert_eq!(capabilities.capabilities, ["process.timeout"]);
        assert_eq!(capabilities.unsupported, ["terminal.pty"]);
    }

    #[test]
    fn layout_and_cjk_width_follow_contract() {
        assert_eq!(layout_profile(120, 24), LayoutProfile::Wide);
        assert_eq!(layout_profile(100, 24), LayoutProfile::Overlay);
        assert_eq!(layout_profile(70, 24), LayoutProfile::Compact);
        assert_eq!(layout_profile(59, 24), LayoutProfile::TooSmall);
        assert_eq!(display_width("你好🙂"), 6);
    }

    #[test]
    fn input_cursor_layout_uses_display_columns_for_unicode_and_scroll() {
        let input = format!("{}你好🙂", "a".repeat(40));
        let layout = input_cursor_layout(&input, input.len(), 20, 2);

        assert_eq!(layout.line, 0);
        assert_eq!(layout.column, 46);
        assert_eq!(layout.horizontal_scroll, 27);
        assert_eq!(layout.vertical_scroll, 0);

        let middle = input_cursor_layout(&input, 40, 20, 2);
        assert_eq!(middle.column, 40);
        assert_eq!(middle.horizontal_scroll, 21);

        let multiline =
            input_cursor_layout("第一行很长\n第二🙂", "第一行很长\n第二🙂".len(), 20, 2);
        assert_eq!(multiline.line, 1);
        assert_eq!(multiline.column, 6);
        assert_eq!(multiline.horizontal_scroll, 0);
        assert_eq!(multiline.vertical_scroll, 0);

        let scrolled = input_cursor_layout("第一\n第二\n第三🙂", "第一\n第二\n第三🙂".len(), 20, 2);
        assert_eq!(scrolled.line, 2);
        assert_eq!(scrolled.vertical_scroll, 1);
    }

    #[test]
    fn input_render_cursor_tracks_scrolled_unicode_text() {
        let mut state = AppState {
            focus: FocusTarget::Input,
            input: format!("{}你好🙂", "a".repeat(100)),
            ..AppState::default()
        };
        state.input_cursor = state.input.len();

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();

        // The input panel has a 78-cell inner width. The cursor is kept in the
        // final visible cell after the same 29-cell scroll used by the text.
        assert_eq!(terminal.get_cursor().unwrap(), (78, 20));
    }

    #[test]
    fn input_render_cursor_tracks_the_visible_multiline_row() {
        let mut state = AppState {
            focus: FocusTarget::Input,
            input: "第一\n第二\n第三🙂".into(),
            ..AppState::default()
        };
        state.input_cursor = state.input.len();

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();

        // The two-row input viewport scrolls down one row so the third row's
        // cursor remains on the visible bottom row.
        assert_eq!(terminal.get_cursor().unwrap(), (7, 21));
    }

    #[test]
    fn session_settings_keeps_catalog_details_inside_the_edit_layer() {
        let mut state = AppState {
            selection: Some(ModelSelection {
                vendor: "vendor-a".into(),
                model: "model-a".into(),
                provider: ProviderSelection::Auto,
            }),
            catalog_models: vec![
                CatalogModel {
                    id: "model-a".into(),
                    vendor: "vendor-a".into(),
                    display_name: "Model A".into(),
                    providers: vec![],
                    supported_reasoning_efforts: Default::default(),
                },
                CatalogModel {
                    id: "model-b".into(),
                    vendor: "vendor-a".into(),
                    display_name: "Model B".into(),
                    providers: vec![],
                    supported_reasoning_efforts: Default::default(),
                },
            ],
            ..AppState::default()
        };
        assert!(state.open_session_settings());
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let browse = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(browse.contains("model-a"));
        assert!(!browse.contains("Model B"));

        state.session_settings.as_mut().unwrap().field = 1;
        state
            .session_settings
            .as_mut()
            .unwrap()
            .begin_edit(SessionSettingsMode::EditField);
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let edit = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(edit.contains("Model B"));
    }

    #[test]
    fn brand_banner_draws_horizontal_wordmark_on_wide_terminals() {
        let unicode = brand_banner_with_unicode(64, true)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(unicode.contains("└"));
        assert!(unicode.contains('█'));
        assert!(!unicode.contains("MAGENTS"));
        // wordmark 行本身不再单独重复 "Rivo" 文本，字母由方块字符拼成。
        assert_eq!(unicode.lines().filter(|line| *line == "Rivo").count(), 0);

        let ascii = brand_banner_with_unicode(64, false)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(ascii.contains("+--"));
        assert!(ascii.contains('#'));
        assert!(!ascii.chars().any(|character| "│└┘─┌┐".contains(character)));
    }

    #[test]
    fn brand_banner_falls_back_to_small_mark_then_text_on_narrow_terminals() {
        let mid = brand_banner_with_unicode(35, true)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(mid.contains("Rivo"));
        assert!(mid.contains("└"));
        assert!(!mid.contains('█'));

        let compact = brand_banner_with_unicode(31, true)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            compact,
            "Rivo\n让智能体在你的终端里工作 · 选择配置后按 Enter 开始"
        );
    }

    #[test]
    fn wordmark_rows_are_uniform_width() {
        for unicode in [true, false] {
            let rows = wordmark_rows(unicode);
            let width = display_width(&rows[0]);
            for row in &rows {
                assert_eq!(display_width(row), width);
            }
        }
    }

    #[test]
    fn markdown_blocks_render_without_syntax_and_narrow_tables_degrade() {
        let rendered = render_markdown_blocks(
            "# 标题\n- 条目\n```text\ncode()\n```\n| 名称 | 值 |\n| --- | --- |\n| a | b |",
            70,
        );
        assert!(rendered.contains("标题"));
        assert!(rendered.contains("• 条目"));
        assert!(rendered.contains("  code()"));
        assert!(rendered.contains("名称: 值"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn follow_bottom_scrolls_long_content_to_the_last_visible_lines() {
        assert_eq!(main_scroll_offset("one\ntwo\nthree", 2, 20, true, 0), 1);
        assert_eq!(main_scroll_offset("one\ntwo\nthree", 2, 20, false, 4), 4);
    }

    #[test]
    fn render_has_small_terminal_fallback_and_wide_status_panel() {
        use ratatui::{backend::TestBackend, Terminal};
        let backend = TestBackend::new(50, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &AppState::default()))
            .unwrap();
        let small = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(small.contains("50x10"));
        assert!(small.contains("60x20"));

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = AppState {
            answer: "answer".into(),
            notice: "notice".into(),
            ..AppState::default()
        };
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let wide = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(wide.contains("answer"));
        assert!(wide.contains("false"));
    }

    #[test]
    fn reducer_tracks_tool_status_trace_and_evidence() {
        let mut state = AppState::default();
        state.reduce(AgentEvent::Started {
            provider: ProviderKind::OpenAiChatCompletions,
            model: "m".into(),
            selection: None,
            route: None,
            turn_id: "turn-1".into(),
            capability_profile: CapabilityProfile::Research,
            policy_profile: PolicyProfile::Workspace,
        });
        state.reduce(AgentEvent::ToolRequested {
            call_id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path":"README.md"}),
        });
        state.reduce(AgentEvent::ConfirmationRequired {
            request_id: "req-1".into(),
            call_id: "call-1".into(),
            turn_id: "turn-1".into(),
            step_id: "call-1".into(),
            tool_name: "read_file".into(),
            arguments_summary: "读取 README.md".into(),
            targets: vec!["README.md".into()],
            program: None,
            side_effects: vec!["无副作用".into()],
            capability_profile: CapabilityProfile::Research,
            policy_profile: PolicyProfile::Workspace,
            constraints: ExecutionConstraints {
                workspace_root: std::env::temp_dir(),
                read_path_scope: PathScope::WorkspaceOnly,
                mutate_path_scope: PathScope::WorkspaceOnly,
                timeout_ms: 120_000,
                output_limit: 1_048_576,
                network_scope: NetworkScope::PublicHttpsOnly,
                redact_environment: true,
            },
            session_scope_available: false,
            session_scope_category: None,
        });
        assert_eq!(state.tools[0].status, ToolStatus::WaitingConfirmation);
        state.reduce(AgentEvent::ToolObservation {
            call_id: "call-1".into(),
            name: "read_file".into(),
            result: serde_json::json!({"id":"e1","excerpt":"bounded"}),
        });
        assert_eq!(state.tools[0].status, ToolStatus::Completed);
        assert!(state.evidence.iter().any(|item| item.starts_with("e1:")));
        assert!(!state.trace.is_empty());
    }

    #[test]
    fn reducer_finds_nested_evidence_and_truncated_results() {
        let mut state = AppState::default();
        state.reduce(AgentEvent::ToolRequested {
            call_id: "call-1".into(),
            name: "web_fetch".into(),
            arguments: serde_json::json!({"url":"https://example.com"}),
        });
        state.reduce(AgentEvent::ToolObservation {
            call_id: "call-1".into(),
            name: "web_fetch".into(),
            result: serde_json::json!({
                "content":"body",
                "truncated":true,
                "evidence":{"id":"web-e1","excerpt":"verified source"}
            }),
        });
        assert_eq!(state.tools[0].status, ToolStatus::Partial);
        assert!(state
            .evidence
            .iter()
            .any(|item| item.starts_with("web-e1:")));
    }

    #[test]
    fn tool_observation_is_collapsed_by_default_and_expands_bounded_details() {
        let mut state = AppState::default();
        state.reduce(AgentEvent::ToolRequested {
            call_id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({}),
        });
        state.reduce(AgentEvent::ToolObservation {
            call_id: "call-1".into(),
            name: "read_file".into(),
            result: serde_json::json!({"content":"bounded detail"}),
        });
        assert!(!sidebar_text(&state).contains("bounded detail"));
        state.toggle_tool_expanded("call-1");
        assert!(sidebar_text(&state).contains("bounded detail"));
        assert!(state.tool_details["call-1"].chars().count() <= 4096);
    }

    #[test]
    fn confirmation_panel_renders_summary_effects_and_request_id() {
        let mut state = AppState::default();
        state.reduce(AgentEvent::ConfirmationRequired {
            request_id: "request-123456789".into(),
            call_id: "call-1".into(),
            turn_id: "turn-1".into(),
            step_id: "call-1".into(),
            tool_name: "write_file".into(),
            arguments_summary: "写入 notes.txt".into(),
            targets: vec!["notes.txt".into()],
            program: None,
            side_effects: vec!["修改工作区文件".into()],
            capability_profile: CapabilityProfile::Coding,
            policy_profile: PolicyProfile::Workspace,
            constraints: ExecutionConstraints {
                workspace_root: std::env::temp_dir(),
                read_path_scope: PathScope::WorkspaceOnly,
                mutate_path_scope: PathScope::WorkspaceOnly,
                timeout_ms: 120_000,
                output_limit: 1_048_576,
                network_scope: NetworkScope::PublicHttpsOnly,
                redact_environment: true,
            },
            session_scope_available: true,
            session_scope_category: Some("workspace_file_mutation".into()),
        });
        assert!(state
            .confirmation_detail()
            .is_some_and(|detail| detail.contains("本会话允许")));
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = output.split_whitespace().collect::<String>();
        assert!(compact.contains("需要确认"));
        assert!(compact.contains("写入notes.txt"));
        assert!(compact.contains("修改工作区文件"));
        assert!(output.contains("request-123"));
    }

    #[test]
    fn memory_candidate_enters_confirmation_state() {
        let mut state = AppState::default();
        state.reduce(AgentEvent::MemoryCandidate {
            candidate_id: "memory-1".into(),
            body_sha256: "hash".into(),
        });
        assert_eq!(state.mode, Some(UiMode::Confirming));
        assert_eq!(
            state.pending_memory_candidate,
            Some(("memory-1".into(), "hash".into()))
        );
        state.reduce(AgentEvent::MemoryRejected {
            candidate_id: "memory-1".into(),
        });
        assert!(state.pending_memory_candidate.is_none());
    }

    #[test]
    fn navigation_and_command_palette_are_bounded_and_rendered() {
        let mut state = AppState::default();
        for index in 0..400 {
            state.reduce(AgentEvent::ModelDelta {
                text: format!("d{index}"),
            });
        }
        assert_eq!(state.trace.len(), 256);
        state.scroll_main(4);
        assert_eq!(state.main_scroll, 4);
        state.jump_to_bottom();
        assert!(state.follow_bottom);
        state.toggle_command_palette();
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(output.contains("cancel"));
    }

    #[test]
    fn history_navigation_skips_expired_sessions_and_marks_the_selection() {
        let mut state = AppState {
            sidebar: SidebarView::History,
            history: vec![
                HistoryEntry {
                    session_id: "expired".into(),
                    created_at: "2026-07-19".into(),
                    task_summary: Some("过期任务".into()),
                    expiration_reason: Some("留存天数上限".into()),
                    event_count: 1,
                    message_count: 1,
                    expired: true,
                },
                HistoryEntry {
                    session_id: "recoverable".into(),
                    created_at: "2026-07-20".into(),
                    task_summary: Some("可恢复任务".into()),
                    expiration_reason: None,
                    event_count: 2,
                    message_count: 2,
                    expired: false,
                },
            ],
            ..AppState::default()
        };
        state.normalize_history_selection();
        assert_eq!(state.selected_history_session(), Some("recoverable"));
        assert!(sidebar_text(&state).contains("  recoverable e=2 m=2 · 可恢复"));
        assert!(sidebar_text(&state).contains("2026-07-20 · 可恢复任务"));
        assert!(sidebar_text(&state).contains("已过期：留存天数上限"));
    }

    #[test]
    fn history_deletion_requires_the_same_action_twice() {
        let mut state = AppState::default();
        let delete = HistoryDestructiveAction::Delete("session-1".into());
        assert!(!state.confirm_history_action(delete.clone()));
        assert!(!state.confirm_history_action(HistoryDestructiveAction::Clear));
        assert!(!state.confirm_history_action(delete.clone()));
        assert!(state.confirm_history_action(delete));
        assert!(state.pending_history_action.is_none());
    }

    #[test]
    fn command_palette_uses_unprefixed_values_and_renders_slash_suggestions() {
        let mut state = AppState::default();
        state.toggle_command_palette();
        state.command_input = "network".into();
        assert_eq!(
            state.command_suggestions(),
            vec!["network on", "network off"]
        );
        state.complete_command();
        assert_eq!(state.command_input, "network on");
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("/network on"));
        state.command_input = "a2a".into();
        assert_eq!(state.command_suggestions(), vec!["a2a", "a2a off"]);
        state.command_input = "e".into();
        assert_eq!(
            state.command_suggestions(),
            vec!["exit", "errors", "errors clear"]
        );
    }

    #[test]
    fn workspace_selector_keeps_default_selected_and_switchable() {
        let mut state = AppState::default();
        assert!(state.open_workspace_selector(vec![
            WorkspaceChoice {
                id: "workspace-b".into(),
                display_name: "Beta".into(),
                root_path: "C:/beta".into(),
                is_default: false,
            },
            WorkspaceChoice {
                id: "workspace-a".into(),
                display_name: "Alpha".into(),
                root_path: "C:/alpha".into(),
                is_default: true,
            },
        ]));
        assert_eq!(
            state.active_overlay(),
            Some(ActiveOverlay::WorkspaceSelector)
        );
        let selector = state.workspace_selector.as_ref().unwrap();
        assert_eq!(selector.selected_workspace().unwrap().id, "workspace-a");
        assert_eq!(selector.workspaces[1].id, "workspace-b");
        state.workspace_selector.as_mut().unwrap().move_selection(1);
        assert_eq!(
            state
                .workspace_selector
                .as_ref()
                .unwrap()
                .selected_workspace()
                .unwrap()
                .root_path,
            "C:/beta"
        );
        state.close_workspace_selector();
        assert_eq!(state.active_overlay(), None);
    }

    #[test]
    fn command_palette_clamps_filtering_and_keeps_selected_item_visible() {
        let mut state = AppState::default();
        state.toggle_command_palette();
        for _ in 0..10 {
            state.move_command_selection(1);
        }
        assert_eq!(state.command_index, 10);
        let suggestions = state.command_suggestions();
        let (start, visible) = state.command_suggestion_window(8);
        assert_eq!(start, 3);
        assert_eq!(visible, suggestions[start..=state.command_index]);

        state.push_command_input("new");
        assert_eq!(state.command_index, 0);
        state.complete_command();
        assert_eq!(state.command_input, "new");
        state.push_command_input("-unknown");
        state.complete_command();
        assert_eq!(state.command_input, "new-unknown");
    }

    #[test]
    fn interaction_overlay_renders_without_diagnostic_detail() {
        let mut state = AppState::default();
        state.reduce(AgentEvent::InteractionRequired {
            request: InteractionRequest {
                request_id: "interaction-1".into(),
                title: "选择操作".into(),
                body: "请选择下一步".into(),
                options: vec![rivo_session::InteractionOption {
                    id: "continue".into(),
                    label: "继续".into(),
                    description: "继续执行".into(),
                }],
                allow_freeform: false,
            },
        });
        assert_eq!(state.active_overlay(), Some(ActiveOverlay::Interaction));
        assert!(!state.diagnostic_detail_open);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Interaction"));
        assert!(rendered.contains("Esc"));

        state.reduce(AgentEvent::Stopped {
            reason: "cancelled".into(),
        });
        assert_eq!(state.active_overlay(), None);
    }

    #[test]
    fn help_overlay_and_modal_scroll_are_renderable_on_small_backend() {
        let mut state = AppState {
            help_overlay: true,
            overlay_scroll: 4,
            ..AppState::default()
        };
        let backend = ratatui::backend::TestBackend::new(52, 18);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        state.help_overlay = false;
        state.diagnostic_detail_open = true;
        state.diagnostics.push(DiagnosticRecord {
            stage: "stream".into(),
            code: "long_error".into(),
            retryable: true,
            recovery: "retry".into(),
            message: "x".repeat(500),
            cause: "y".repeat(500),
        });
        state.overlay_scroll = 6;
        terminal.draw(|frame| render(frame, &state)).unwrap();
    }

    #[test]
    fn input_is_grapheme_safe_and_structured_failures_remain_actionable() {
        let mut state = AppState::default();
        state.push_input("你好🙂");
        state.backspace_input();
        assert_eq!(state.input, "你好");
        state.move_input_cursor(false);
        state.move_input_cursor(true);
        assert_eq!(&state.input[..state.input_cursor], "你好");
        state.push_input("\n第二行");
        state.move_input_line_boundary(false);
        assert_eq!(&state.input[..state.input_cursor], "你好\n");
        state.move_input_line_boundary(true);
        assert_eq!(&state.input[state.input_cursor..], "");
        state.reduce(AgentEvent::Failure {
            message: "provider disconnected".into(),
            details: rivo_session::FailureDetails {
                stage: rivo_session::FailureStage::Stream,
                code: "provider_stream_failed".into(),
                retryable: true,
                recovery: "retry".into(),
                cause: "connection reset".into(),
            },
        });
        assert_eq!(state.turn_phase, TurnPhase::Idle);
        assert_eq!(state.diagnostics.len(), 1);
        assert!(sidebar_text(&AppState {
            sidebar: SidebarView::Errors,
            diagnostics: state.diagnostics,
            ..AppState::default()
        })
        .contains("provider_stream_failed"));
    }

    #[test]
    fn width_safe_rendering_and_conversation_collapse_are_bounded() {
        let wrapped = wrap_display_text("这是一个很长的中文行🙂abcdef", 8);
        assert!(wrapped.iter().all(|line| display_width(line) <= 8));
        assert_eq!(
            truncate_display("abcdefghijklmnopqrstuvwxyz", 8),
            "abcdefg…"
        );
        let mut state = AppState::default();
        state.append_user_prompt("用户输入".into());
        state.reduce(AgentEvent::ModelDelta {
            text: "模型回答很长".into(),
        });
        state.focus = FocusTarget::Conversation;
        state.toggle_selected_conversation();
        assert!(state.conversation.iter().any(|block| block.collapsed));
        state.scroll_horizontal(4);
        assert_eq!(state.main_horizontal, 4);
    }

    fn startup_state() -> StartupConfigState {
        StartupConfigState::new(
            crate::routing_tests::routing_config(),
            vec!["docs".into(), "code".into()],
            None,
            TurnOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn startup_screen_renders_before_the_workspace() {
        use ratatui::{backend::TestBackend, Terminal};
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &AppState {
                        startup: Some(startup_state()),
                        ..AppState::default()
                    },
                )
            })
            .unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(output.contains("alpha"));
        assert!(output.contains("MCP docs"));
        assert!(!output.contains("Conversation"));
    }
}
