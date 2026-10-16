use super::{
    AgentEvent, ConfirmationRegistry, InteractionRegistry, PendingTurn, RetryTurn,
    SubagentConfirmationRegistry, TurnOptions,
};
use rivo_agent::{PendingConfirmation, PendingSubagentConfirmation};
use rivo_core::Message;
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub(super) struct TurnContext {
    pub(super) sender: tokio::sync::mpsc::Sender<AgentEvent>,
    pub(super) cancel_rx: tokio::sync::watch::Receiver<bool>,
    pub(super) active: Arc<AtomicBool>,
    pub(super) history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub(super) pending: Arc<Mutex<Option<PendingTurn>>>,
    pub(super) approved: HashSet<String>,
    pub(super) storage: Option<Arc<Mutex<rivo_storage::Store>>>,
    pub(super) retry: Arc<tokio::sync::Mutex<Option<RetryTurn>>>,
    pub(super) session_id: String,
    pub(super) turn_id: String,
    pub(super) sequence: u64,
    pub(super) turn_options: TurnOptions,
    pub(super) memory_ids: Vec<String>,
    pub(super) api_keys: Vec<String>,
    pub(super) browser_used: bool,
    pub(super) interactions: InteractionRegistry,
    pub(super) confirmation_tx: mpsc::Sender<PendingConfirmation>,
    pub(super) confirmation_rx: mpsc::Receiver<PendingConfirmation>,
    pub(super) confirmations: ConfirmationRegistry,
    pub(super) subagent_confirmation_tx: mpsc::Sender<PendingSubagentConfirmation>,
    pub(super) subagent_confirmation_rx: mpsc::Receiver<PendingSubagentConfirmation>,
    pub(super) subagent_confirmations: SubagentConfirmationRegistry,
}
