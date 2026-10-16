use rivo_session::AgentEvent;
use std::collections::VecDeque;

/// 有界的前端事件队列。
///
/// 连续文本增量在入队时合并；普通事件挤满队列时优先淘汰普通事件，
/// 从而保留错误、确认和终态。队列只负责调度，不改变保留事件的顺序。
#[derive(Debug)]
pub struct EventQueue {
    events: VecDeque<AgentEvent>,
    capacity: usize,
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new(256)
    }
}

impl EventQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, event: AgentEvent) {
        if let (Some(AgentEvent::ModelDelta { text: existing }), AgentEvent::ModelDelta { text }) =
            (self.events.back_mut(), &event)
        {
            existing.push_str(text);
            return;
        }

        if self.events.len() >= self.capacity {
            if let Some(index) = self.events.iter().position(is_droppable_event) {
                self.events.remove(index);
            } else if !is_droppable_event(&event) {
                self.events.pop_front();
            } else {
                return;
            }
        }
        self.events.push_back(event);
    }

    pub fn drain(&mut self) -> impl Iterator<Item = AgentEvent> + '_ {
        self.events.drain(..)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn has_urgent(&self) -> bool {
        self.events.iter().any(|event| !is_droppable_event(event))
    }
}

fn is_droppable_event(event: &AgentEvent) -> bool {
    !matches!(
        event,
        AgentEvent::FinalAnswer { .. }
            | AgentEvent::Error { .. }
            | AgentEvent::Failure { .. }
            | AgentEvent::SubagentQueued { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentCompleted { .. }
            | AgentEvent::SubagentConfirmationRequired { .. }
            | AgentEvent::ConfirmationRequired { .. }
            | AgentEvent::InteractionRequired { .. }
            | AgentEvent::Stopped { .. }
    )
}
