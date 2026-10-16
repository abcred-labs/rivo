use crate::{StorageError, Store, StoredEvent, StoredMessage};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

enum Request {
    Append(StoredEvent, SyncSender<Result<(), StorageError>>),
    Replay(String, SyncSender<Result<Vec<StoredEvent>, StorageError>>),
    AppendMessage(StoredMessage, SyncSender<Result<(), StorageError>>),
    ReplayMessages(String, SyncSender<Result<Vec<StoredMessage>, StorageError>>),
    NextEventSequence(String, SyncSender<Result<u64, StorageError>>),
    NextMessageSequence(String, SyncSender<Result<u64, StorageError>>),
    Shutdown(SyncSender<Result<(), StorageError>>),
}

/// A bounded, single-owner execution boundary for SQLite operations. This
/// prototype is intentionally not wired into Session until RF-08 migration
/// evidence covers every caller and shutdown path.
pub struct StorageWorker {
    sender: SyncSender<Request>,
    join: Option<JoinHandle<()>>,
}

impl StorageWorker {
    pub fn start(store: Store, capacity: usize) -> Result<Self, StorageError> {
        let (sender, receiver) = mpsc::sync_channel(capacity.max(1));
        let join = thread::Builder::new()
            .name("rivo-storage-worker".into())
            .spawn(move || run(store, receiver))
            .map_err(|error| {
                StorageError::Unavailable(format!("无法启动历史存储 worker: {error}"))
            })?;
        Ok(Self {
            sender,
            join: Some(join),
        })
    }

    pub fn append_event(&self, event: StoredEvent) -> Result<(), StorageError> {
        let (reply, receiver) = mpsc::sync_channel(0);
        self.sender
            .send(Request::Append(event, reply))
            .map_err(|_| worker_closed())?;
        receiver.recv().map_err(|_| worker_closed())?
    }

    pub fn replay(&self, session_id: impl Into<String>) -> Result<Vec<StoredEvent>, StorageError> {
        let (reply, receiver) = mpsc::sync_channel(0);
        self.sender
            .send(Request::Replay(session_id.into(), reply))
            .map_err(|_| worker_closed())?;
        receiver.recv().map_err(|_| worker_closed())?
    }

    pub fn append_message(&self, message: StoredMessage) -> Result<(), StorageError> {
        let (reply, receiver) = mpsc::sync_channel(0);
        self.sender
            .send(Request::AppendMessage(message, reply))
            .map_err(|_| worker_closed())?;
        receiver.recv().map_err(|_| worker_closed())?
    }

    pub fn replay_messages(
        &self,
        session_id: impl Into<String>,
    ) -> Result<Vec<StoredMessage>, StorageError> {
        let (reply, receiver) = mpsc::sync_channel(0);
        self.sender
            .send(Request::ReplayMessages(session_id.into(), reply))
            .map_err(|_| worker_closed())?;
        receiver.recv().map_err(|_| worker_closed())?
    }

    pub fn next_event_sequence(&self, session_id: impl Into<String>) -> Result<u64, StorageError> {
        let (reply, receiver) = mpsc::sync_channel(0);
        self.sender
            .send(Request::NextEventSequence(session_id.into(), reply))
            .map_err(|_| worker_closed())?;
        receiver.recv().map_err(|_| worker_closed())?
    }

    pub fn next_message_sequence(
        &self,
        session_id: impl Into<String>,
    ) -> Result<u64, StorageError> {
        let (reply, receiver) = mpsc::sync_channel(0);
        self.sender
            .send(Request::NextMessageSequence(session_id.into(), reply))
            .map_err(|_| worker_closed())?;
        receiver.recv().map_err(|_| worker_closed())?
    }

    pub fn shutdown(mut self) -> Result<(), StorageError> {
        let (reply, receiver) = mpsc::sync_channel(0);
        self.sender
            .send(Request::Shutdown(reply))
            .map_err(|_| worker_closed())?;
        let result = receiver.recv().map_err(|_| worker_closed())?;
        if let Some(join) = self.join.take() {
            join.join().map_err(|_| worker_closed())?;
        }
        result
    }
}

impl Drop for StorageWorker {
    fn drop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        let (reply, receiver) = mpsc::sync_channel(0);
        if self.sender.send(Request::Shutdown(reply)).is_ok() {
            let _ = receiver.recv();
        }
        let _ = join.join();
    }
}

fn run(store: Store, receiver: Receiver<Request>) {
    while let Ok(request) = receiver.recv() {
        match request {
            Request::Append(event, reply) => {
                let _ = reply.send(store.append_event(&event));
            }
            Request::Replay(session_id, reply) => {
                let _ = reply.send(store.replay(&session_id));
            }
            Request::AppendMessage(message, reply) => {
                let _ = reply.send(store.append_message(&message));
            }
            Request::ReplayMessages(session_id, reply) => {
                let _ = reply.send(store.replay_messages(&session_id));
            }
            Request::NextEventSequence(session_id, reply) => {
                let _ = reply.send(store.next_event_sequence(&session_id));
            }
            Request::NextMessageSequence(session_id, reply) => {
                let _ = reply.send(store.next_message_sequence(&session_id));
            }
            Request::Shutdown(reply) => {
                let _ = reply.send(store.close());
                break;
            }
        }
    }
}

fn worker_closed() -> StorageError {
    StorageError::Unavailable("历史存储 worker 已关闭".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64) -> StoredEvent {
        StoredEvent {
            session_id: "worker".into(),
            turn_id: "turn".into(),
            sequence,
            kind: "message".into(),
            body: sequence.to_string(),
        }
    }

    fn message(sequence: u64) -> StoredMessage {
        StoredMessage {
            session_id: "worker".into(),
            turn_id: "turn".into(),
            sequence,
            role: "user".into(),
            body: sequence.to_string(),
        }
    }

    #[test]
    fn worker_preserves_request_order_and_shutdown() {
        let worker = StorageWorker::start(Store::open_memory().unwrap(), 4).unwrap();
        assert_eq!(worker.next_event_sequence("worker").unwrap(), 0);
        worker.append_event(event(1)).unwrap();
        worker.append_event(event(2)).unwrap();
        assert_eq!(worker.next_event_sequence("worker").unwrap(), 3);
        assert_eq!(
            worker
                .replay("worker")
                .unwrap()
                .iter()
                .map(|item| item.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        worker.shutdown().unwrap();
    }

    #[test]
    fn worker_preserves_message_order_and_round_trip() {
        let worker = StorageWorker::start(Store::open_memory().unwrap(), 2).unwrap();
        assert_eq!(worker.next_message_sequence("worker").unwrap(), 0);
        worker.append_message(message(0)).unwrap();
        worker.append_message(message(1)).unwrap();
        assert_eq!(worker.next_message_sequence("worker").unwrap(), 2);
        assert_eq!(
            worker
                .replay_messages("worker")
                .unwrap()
                .iter()
                .map(|item| item.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        worker.shutdown().unwrap();
    }

    #[test]
    fn worker_matches_direct_store_event_message_projection() {
        let direct = Store::open_memory().unwrap();
        direct.append_event(&event(0)).unwrap();
        direct
            .append_message(&message(0))
            .expect("direct message write should succeed");

        let worker = StorageWorker::start(Store::open_memory().unwrap(), 2).unwrap();
        worker.append_event(event(0)).unwrap();
        worker.append_message(message(0)).unwrap();

        assert_eq!(
            worker.replay("worker").unwrap(),
            direct.replay("worker").unwrap()
        );
        assert_eq!(
            worker.replay_messages("worker").unwrap(),
            direct.replay_messages("worker").unwrap()
        );
        worker.shutdown().unwrap();
    }

    #[test]
    fn worker_maps_sequence_conflict_without_losing_prior_events() {
        let worker = StorageWorker::start(Store::open_memory().unwrap(), 2).unwrap();
        worker.append_event(event(0)).unwrap();
        let error = worker.append_event(event(0)).unwrap_err();
        assert!(matches!(error, StorageError::Sqlite(_)));
        assert_eq!(worker.replay("worker").unwrap(), vec![event(0)]);
        worker.shutdown().unwrap();
    }

    #[test]
    fn worker_reports_closed_boundary_structurally() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let worker = StorageWorker { sender, join: None };
        assert!(matches!(
            worker.append_event(event(1)),
            Err(StorageError::Unavailable(_))
        ));
    }

    #[test]
    fn bounded_request_queue_rejects_overflow_without_growth() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (first_reply, _first_result) = mpsc::sync_channel(0);
        let (second_reply, _second_result) = mpsc::sync_channel(0);
        sender
            .try_send(Request::Append(event(0), first_reply))
            .expect("first request fits the bounded queue");
        assert!(matches!(
            sender.try_send(Request::Append(event(1), second_reply)),
            Err(mpsc::TrySendError::Full(_))
        ));
        drop(receiver);
    }

    #[test]
    fn dropping_worker_closes_store_for_reopen() {
        let path = tempfile::tempdir().unwrap();
        let database = path.path().join("history.db");
        {
            let worker = StorageWorker::start(Store::open(&database).unwrap(), 2).unwrap();
            worker.append_event(event(0)).unwrap();
        }
        let reopened = Store::open(&database).unwrap();
        assert_eq!(reopened.replay("worker").unwrap(), vec![event(0)]);
    }

    #[test]
    fn cancelled_reply_does_not_interrupt_started_request() {
        let worker = StorageWorker::start(Store::open_memory().unwrap(), 2).unwrap();
        let (reply, receiver) = mpsc::sync_channel(0);
        worker
            .sender
            .send(Request::Append(event(0), reply))
            .unwrap();
        drop(receiver);

        worker.append_event(event(1)).unwrap();
        assert_eq!(
            worker
                .replay("worker")
                .unwrap()
                .iter()
                .map(|item| item.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        worker.shutdown().unwrap();
    }

    #[test]
    fn worker_propagates_readonly_write_failure_without_partial_event() {
        let store = Store::open_memory().unwrap();
        store
            .connection
            .execute_batch("PRAGMA query_only = ON;")
            .unwrap();
        let worker = StorageWorker::start(store, 2).unwrap();

        let error = worker.append_event(event(0)).unwrap_err();
        assert!(matches!(error, StorageError::Sqlite(_)));
        assert!(worker.replay("worker").unwrap().is_empty());
        worker.shutdown().unwrap();
    }

    #[test]
    fn worker_surfaces_corrupt_history_without_skipping_records() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("corrupt-history.db");
        let store = Store::open(&database).unwrap();
        store.append_event(&event(0)).unwrap();
        store.close().unwrap();

        let store = Store::open(&database).unwrap();
        store
            .connection
            .execute(
                "UPDATE events SET body=?1 WHERE session_id=?2 AND sequence=?3",
                rusqlite::params!["not-json", "worker", 0u64],
            )
            .unwrap();
        store.close().unwrap();

        let worker = StorageWorker::start(Store::open(&database).unwrap(), 2).unwrap();
        assert!(matches!(
            worker.replay("worker"),
            Err(StorageError::CorruptEvent {
                session_id,
                sequence: 0,
                ..
            }) if session_id == "worker"
        ));
        worker.shutdown().unwrap();
    }
}
