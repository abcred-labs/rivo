//! Durable, redacted SQLite projections for A2A task and owner-approval state.

use crate::pairing::valid_sha256;
use crate::protocol::unix_now;
use crate::{
    LocalApproval, LocalApprovalRequest, PairingState, PeerGrant, PendingPairing, ServerError,
};
use rivo_a2a::{A2aPeerId, A2aRequestId, A2aTask, A2aTaskId, A2aTaskState};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA_VERSION: u64 = 2;

/// Server 独立 SQLite state。它不保存正文、token、CSR 或私钥；仅保存可恢复的
/// task 投影和本机 pairings list 所需的脱敏字段。
pub struct ServerStateStore {
    pub(super) connection: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPairingSummary {
    pub request_id: A2aRequestId,
    pub client_public_key_sha256: String,
    pub expires_unix: u64,
    pub state: PairingState,
}

/// Server 重启后重新建立 A2A task 隔离与幂等映射所需的最小投影。请求正文、
/// Session/Turn 标识、本机路径、证书材料与 task 输出均不落入该表。
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedServerTask {
    pub profile_name: String,
    pub peer_id: A2aPeerId,
    pub message_id: String,
    pub request_hash: String,
    pub task: A2aTask,
}

/// 只投影给 Server 所在本机 owner 的待审批项。它不保存原始工具 JSON、凭据或
/// workspace 正文；`canonical_args_sha256` 将本次一次性许可绑定到实际参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLocalApproval {
    pub approval_id: String,
    pub task_id: A2aTaskId,
    pub peer_id: A2aPeerId,
    pub skill_id: String,
    pub turn_id: String,
    pub request_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub canonical_args_sha256: String,
    pub arguments_summary: String,
    pub targets: Vec<String>,
    pub program: Option<String>,
    pub side_effects: Vec<String>,
    pub expires_unix: u64,
    pub status: LocalApprovalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalApprovalStatus {
    Pending,
    Approved,
    Rejected,
    TimedOut,
    Consumed,
    Invalidated,
}

impl ServerStateStore {
    pub fn open(path: &Path) -> Result<Self, ServerError> {
        let connection = Connection::open(path).map_err(|_| ServerError::InvalidRequest)?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
            .map_err(|_| ServerError::InvalidRequest)?;
        let has_schema: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='a2a_server_schema_version')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| ServerError::InvalidRequest)?;
        if has_schema {
            let version: u64 = connection
                .query_row(
                    "SELECT version FROM a2a_server_schema_version LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| ServerError::UnsupportedStateSchema)?;
            if version != SCHEMA_VERSION {
                return Err(ServerError::UnsupportedStateSchema);
            }
            validate_current_schema(&connection)?;
            return Ok(Self { connection });
        }
        let has_tables: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| ServerError::InvalidRequest)?;
        if has_tables {
            return Err(ServerError::UnsupportedStateSchema);
        }
        let transaction = connection
            .unchecked_transaction()
            .map_err(|_| ServerError::InvalidRequest)?;
        transaction.execute_batch("CREATE TABLE a2a_server_schema_version(version INTEGER NOT NULL); INSERT INTO a2a_server_schema_version(version) VALUES(2); CREATE TABLE a2a_server_tasks(task_id TEXT PRIMARY KEY, profile_name TEXT NOT NULL, peer_id TEXT NOT NULL, message_id TEXT NOT NULL, request_hash TEXT NOT NULL, state TEXT NOT NULL, revision INTEGER NOT NULL, updated_unix INTEGER NOT NULL); CREATE UNIQUE INDEX a2a_server_tasks_idempotency ON a2a_server_tasks(profile_name, peer_id, message_id); CREATE TABLE a2a_server_peer_grants(profile_name TEXT NOT NULL, peer_id TEXT NOT NULL, issuer_ref TEXT NOT NULL, certificate_serial TEXT NOT NULL, certificate_fingerprint TEXT NOT NULL, expires_unix INTEGER NOT NULL, revoked INTEGER NOT NULL, PRIMARY KEY(profile_name, peer_id)); CREATE UNIQUE INDEX a2a_server_peer_grants_fingerprint ON a2a_server_peer_grants(profile_name, certificate_fingerprint); CREATE TABLE a2a_server_pairings(request_id TEXT PRIMARY KEY, client_public_key_sha256 TEXT NOT NULL, expires_unix INTEGER NOT NULL, state TEXT NOT NULL); CREATE TABLE a2a_server_approvals(approval_id TEXT PRIMARY KEY, task_id TEXT NOT NULL, peer_id TEXT NOT NULL, skill_id TEXT NOT NULL, turn_id TEXT NOT NULL, request_id TEXT NOT NULL, call_id TEXT NOT NULL, tool_name TEXT NOT NULL, canonical_args_sha256 TEXT NOT NULL, arguments_summary TEXT NOT NULL, targets_json TEXT NOT NULL, program TEXT, side_effects_json TEXT NOT NULL, expires_unix INTEGER NOT NULL, status TEXT NOT NULL, decided_unix INTEGER); CREATE INDEX a2a_server_approvals_pending ON a2a_server_approvals(status, expires_unix);")
            .map_err(|_| ServerError::InvalidRequest)?;
        transaction
            .commit()
            .map_err(|_| ServerError::InvalidRequest)?;
        Ok(Self { connection })
    }

    pub fn record_task(
        &self,
        task: &PersistedServerTask,
        now_unix: u64,
    ) -> Result<(), ServerError> {
        if task.profile_name.is_empty()
            || task.message_id.len() > 256
            || !valid_sha256(&task.request_hash)
            || task.task.id.0.is_empty()
            || task.task.id.0.len() > 128
        {
            return Err(ServerError::InvalidRequest);
        }
        self.connection.execute("INSERT INTO a2a_server_tasks(task_id,profile_name,peer_id,message_id,request_hash,state,revision,updated_unix) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(task_id) DO UPDATE SET state=excluded.state,revision=excluded.revision,updated_unix=excluded.updated_unix WHERE a2a_server_tasks.profile_name=excluded.profile_name AND a2a_server_tasks.peer_id=excluded.peer_id AND a2a_server_tasks.message_id=excluded.message_id AND a2a_server_tasks.request_hash=excluded.request_hash AND excluded.revision >= a2a_server_tasks.revision", params![task.task.id.0, task.profile_name, task.peer_id.0, task.message_id, task.request_hash, task_state_name(task.task.state), task.task.revision.unwrap_or(0), now_unix]).map_err(|_| ServerError::InvalidRequest)?;
        Ok(())
    }

    pub fn recover_unfinished(
        &self,
        profile_name: &str,
        now_unix: u64,
    ) -> Result<usize, ServerError> {
        self.connection.execute("UPDATE a2a_server_tasks SET state='FAILED', revision=revision+1, updated_unix=?1 WHERE profile_name=?2 AND state IN ('SUBMITTED','WORKING','INPUT_REQUIRED')", params![now_unix, profile_name]).map_err(|_| ServerError::InvalidRequest)
    }

    pub fn load_tasks(&self, profile_name: &str) -> Result<Vec<PersistedServerTask>, ServerError> {
        let mut statement = self.connection.prepare("SELECT task_id,peer_id,message_id,request_hash,state,revision FROM a2a_server_tasks WHERE profile_name=?1 ORDER BY task_id").map_err(|_| ServerError::InvalidRequest)?;
        let rows = statement
            .query_map(params![profile_name], |row| {
                let state = task_state_from_name(&row.get::<_, String>(4)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                let request_hash: String = row.get(3)?;
                if !valid_sha256(&request_hash) {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                Ok(PersistedServerTask {
                    profile_name: profile_name.into(),
                    peer_id: A2aPeerId(row.get(1)?),
                    message_id: row.get(2)?,
                    request_hash,
                    task: A2aTask {
                        id: A2aTaskId(row.get(0)?),
                        state,
                        revision: Some(row.get(5)?),
                        message: None,
                        artifacts: Vec::new(),
                    },
                })
            })
            .map_err(|_| ServerError::InvalidRequest)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| ServerError::InvalidRequest)
    }

    pub fn save_peer_grant(&self, grant: &PeerGrant) -> Result<(), ServerError> {
        if grant.profile_name.is_empty()
            || grant.issuer_ref.is_empty()
            || grant.certificate_serial.is_empty()
            || !valid_sha256(&grant.certificate_fingerprint)
        {
            return Err(ServerError::InvalidRequest);
        }
        self.connection.execute("INSERT INTO a2a_server_peer_grants(profile_name,peer_id,issuer_ref,certificate_serial,certificate_fingerprint,expires_unix,revoked) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(profile_name,peer_id) DO UPDATE SET issuer_ref=excluded.issuer_ref,certificate_serial=excluded.certificate_serial,certificate_fingerprint=excluded.certificate_fingerprint,expires_unix=excluded.expires_unix,revoked=excluded.revoked", params![grant.profile_name, grant.peer_id.0, grant.issuer_ref, grant.certificate_serial, grant.certificate_fingerprint, grant.expires_unix, u8::from(grant.revoked)]).map_err(|_| ServerError::InvalidRequest)?;
        Ok(())
    }

    pub fn load_peer_grants(&self, profile_name: &str) -> Result<Vec<PeerGrant>, ServerError> {
        let mut statement = self.connection.prepare("SELECT peer_id,issuer_ref,certificate_serial,certificate_fingerprint,expires_unix,revoked FROM a2a_server_peer_grants WHERE profile_name=?1 ORDER BY peer_id").map_err(|_| ServerError::InvalidRequest)?;
        let rows = statement
            .query_map(params![profile_name], |row| {
                let certificate_fingerprint: String = row.get(3)?;
                if !valid_sha256(&certificate_fingerprint) {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                Ok(PeerGrant {
                    profile_name: profile_name.into(),
                    peer_id: A2aPeerId(row.get(0)?),
                    issuer_ref: row.get(1)?,
                    certificate_serial: row.get(2)?,
                    certificate_fingerprint,
                    expires_unix: row.get(4)?,
                    revoked: row.get::<_, u8>(5)? != 0,
                })
            })
            .map_err(|_| ServerError::InvalidRequest)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| ServerError::InvalidRequest)
    }

    pub fn revoke_peer_grant(
        &self,
        profile_name: &str,
        peer_id: &A2aPeerId,
    ) -> Result<bool, ServerError> {
        let changed = self.connection.execute(
            "UPDATE a2a_server_peer_grants SET revoked=1 WHERE profile_name=?1 AND peer_id=?2 AND revoked=0",
            params![profile_name, peer_id.0],
        ).map_err(|_| ServerError::InvalidRequest)?;
        self.connection.execute(
            "UPDATE a2a_server_tasks SET state='CANCELED', revision=revision+1, updated_unix=?1 WHERE profile_name=?2 AND peer_id=?3 AND state NOT IN ('COMPLETED','CANCELED','FAILED','REJECTED')",
            params![unix_now(), profile_name, peer_id.0],
        ).map_err(|_| ServerError::InvalidRequest)?;
        Ok(changed > 0)
    }

    pub fn save_pairing_summary(&self, pairing: &PendingPairing) -> Result<(), ServerError> {
        self.connection.execute("INSERT INTO a2a_server_pairings(request_id,client_public_key_sha256,expires_unix,state) VALUES(?1,?2,?3,?4) ON CONFLICT(request_id) DO UPDATE SET expires_unix=excluded.expires_unix,state=excluded.state", params![pairing.request_id.0, pairing.client_public_key_sha256, pairing.expires_unix, pairing_state_name(pairing.state)]).map_err(|_| ServerError::InvalidRequest)?;
        Ok(())
    }

    pub fn pairing_summaries(&self) -> Result<Vec<StoredPairingSummary>, ServerError> {
        let mut statement = self.connection.prepare("SELECT request_id,client_public_key_sha256,expires_unix,state FROM a2a_server_pairings ORDER BY request_id").map_err(|_| ServerError::InvalidRequest)?;
        let rows = statement
            .query_map([], |row| {
                Ok(StoredPairingSummary {
                    request_id: A2aRequestId(row.get(0)?),
                    client_public_key_sha256: row.get(1)?,
                    expires_unix: row.get(2)?,
                    state: pairing_state_from_name(&row.get::<_, String>(3)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })
            .map_err(|_| ServerError::InvalidRequest)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| ServerError::InvalidRequest)
    }

    /// SQLite 从不保存 CSR 或 claim token；listener 领取时读取这项 owner 裁决。
    pub fn set_pairing_state(
        &self,
        request_id: &A2aRequestId,
        next: PairingState,
        now_unix: u64,
    ) -> Result<(), ServerError> {
        if !matches!(next, PairingState::Approved | PairingState::Rejected) {
            return Err(ServerError::InvalidRequest);
        }
        let changed = self.connection.execute(
            "UPDATE a2a_server_pairings SET state=?1 WHERE request_id=?2 AND state='pending' AND expires_unix>?3",
            params![pairing_state_name(next), request_id.0, now_unix],
        ).map_err(|_| ServerError::InvalidRequest)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(ServerError::InvalidPairing)
        }
    }

    pub fn pairing_state(
        &self,
        request_id: &A2aRequestId,
        now_unix: u64,
    ) -> Result<PairingState, ServerError> {
        let result = self.connection.query_row(
            "SELECT state, expires_unix FROM a2a_server_pairings WHERE request_id=?1",
            params![request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        );
        let (state, expires) = result.map_err(|_| ServerError::InvalidPairing)?;
        if expires <= now_unix && state == "pending" {
            Ok(PairingState::Expired)
        } else {
            pairing_state_from_name(&state)
        }
    }

    /// Restart 后没有 CSR/token，旧 pending pairing 必须失效而不能被重新领取。
    pub fn expire_pending_pairings_after_restart(&self) -> Result<usize, ServerError> {
        self.connection
            .execute(
                "UPDATE a2a_server_pairings SET state='expired' WHERE state='pending'",
                [],
            )
            .map_err(|_| ServerError::InvalidRequest)
    }

    pub fn take_local_approval(
        &self,
        approval_id: &str,
        now_unix: u64,
    ) -> Result<Option<LocalApproval>, ServerError> {
        self.expire_local_approvals(now_unix)?;
        let status: Option<String> = self
            .connection
            .query_row(
                "SELECT status FROM a2a_server_approvals WHERE approval_id=?1",
                params![approval_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| ServerError::InvalidRequest)?;
        let Some(status) = status else {
            return Ok(None);
        };
        let outcome = match status.as_str() {
            "approved" => LocalApproval::ApprovedOnce,
            "rejected" | "invalidated" => LocalApproval::Rejected,
            "timed_out" => LocalApproval::TimedOut,
            "pending" => return Ok(None),
            "consumed" => return Ok(Some(LocalApproval::Unavailable)),
            _ => return Err(ServerError::InvalidRequest),
        };
        let changed = self.connection.execute(
            "UPDATE a2a_server_approvals SET status='consumed' WHERE approval_id=?1 AND status=?2",
            params![approval_id, status],
        ).map_err(|_| ServerError::InvalidRequest)?;
        Ok((changed == 1).then_some(outcome))
    }

    pub fn enqueue_local_approval(
        &self,
        request: &LocalApprovalRequest,
    ) -> Result<String, ServerError> {
        request.validate()?;
        let approval_id = request.approval_id();
        let targets =
            serde_json::to_string(&request.targets).map_err(|_| ServerError::InvalidRequest)?;
        let side_effects = serde_json::to_string(&request.side_effects)
            .map_err(|_| ServerError::InvalidRequest)?;
        self.connection.execute("INSERT INTO a2a_server_approvals(approval_id,task_id,peer_id,skill_id,turn_id,request_id,call_id,tool_name,canonical_args_sha256,arguments_summary,targets_json,program,side_effects_json,expires_unix,status,decided_unix) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'pending',NULL) ON CONFLICT(approval_id) DO NOTHING", params![approval_id, request.task_id.0, request.peer_id.0, request.skill_id, request.turn_id, request.request_id, request.call_id, request.tool_name, request.canonical_args_sha256, request.arguments_summary, targets, request.program, side_effects, request.expires_unix]).map_err(|_| ServerError::InvalidRequest)?;
        let count: u64 = self.connection.query_row("SELECT COUNT(*) FROM a2a_server_approvals WHERE approval_id=?1 AND task_id=?2 AND peer_id=?3 AND turn_id=?4 AND request_id=?5 AND call_id=?6 AND tool_name=?7 AND canonical_args_sha256=?8", params![approval_id, request.task_id.0, request.peer_id.0, request.turn_id, request.request_id, request.call_id, request.tool_name, request.canonical_args_sha256], |row| row.get(0)).map_err(|_| ServerError::InvalidRequest)?;
        if count != 1 {
            return Err(ServerError::InvalidRequest);
        }
        Ok(approval_id)
    }

    pub fn pending_local_approvals(
        &self,
        now_unix: u64,
    ) -> Result<Vec<StoredLocalApproval>, ServerError> {
        self.expire_local_approvals(now_unix)?;
        self.local_approvals("WHERE status='pending'")
    }

    pub fn decide_local_approval(
        &self,
        approval_id: &str,
        approve: bool,
        now_unix: u64,
    ) -> Result<(), ServerError> {
        if !valid_local_approval_id(approval_id) {
            return Err(ServerError::InvalidRequest);
        }
        self.expire_local_approvals(now_unix)?;
        let status = if approve { "approved" } else { "rejected" };
        let changed = self.connection.execute(
            "UPDATE a2a_server_approvals SET status=?1, decided_unix=?2 WHERE approval_id=?3 AND status='pending' AND expires_unix>?2",
            params![status, now_unix, approval_id],
        ).map_err(|_| ServerError::InvalidRequest)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(ServerError::LocalApprovalUnavailable)
        }
    }

    pub fn invalidate_task_approvals(&self, task_id: &A2aTaskId) -> Result<usize, ServerError> {
        self.connection.execute(
            "UPDATE a2a_server_approvals SET status='invalidated', decided_unix=NULL WHERE task_id=?1 AND status IN ('pending','approved')",
            params![task_id.0],
        ).map_err(|_| ServerError::InvalidRequest)
    }

    pub(super) fn expire_local_approvals(&self, now_unix: u64) -> Result<(), ServerError> {
        self.connection.execute("UPDATE a2a_server_approvals SET status='timed_out', decided_unix=?1 WHERE status='pending' AND expires_unix<=?1", params![now_unix]).map_err(|_| ServerError::InvalidRequest)?;
        Ok(())
    }

    fn local_approvals(&self, predicate: &str) -> Result<Vec<StoredLocalApproval>, ServerError> {
        let query = format!("SELECT approval_id,task_id,peer_id,skill_id,turn_id,request_id,call_id,tool_name,canonical_args_sha256,arguments_summary,targets_json,program,side_effects_json,expires_unix,status FROM a2a_server_approvals {predicate} ORDER BY expires_unix,approval_id");
        let mut statement = self
            .connection
            .prepare(&query)
            .map_err(|_| ServerError::InvalidRequest)?;
        let rows = statement
            .query_map([], |row| {
                let targets_json: String = row.get(10)?;
                let side_effects_json: String = row.get(12)?;
                Ok(StoredLocalApproval {
                    approval_id: row.get(0)?,
                    task_id: A2aTaskId(row.get(1)?),
                    peer_id: A2aPeerId(row.get(2)?),
                    skill_id: row.get(3)?,
                    turn_id: row.get(4)?,
                    request_id: row.get(5)?,
                    call_id: row.get(6)?,
                    tool_name: row.get(7)?,
                    canonical_args_sha256: row.get(8)?,
                    arguments_summary: row.get(9)?,
                    targets: serde_json::from_str(&targets_json)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    program: row.get(11)?,
                    side_effects: serde_json::from_str(&side_effects_json)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    expires_unix: row.get(13)?,
                    status: local_approval_status_from_name(&row.get::<_, String>(14)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })
            .map_err(|_| ServerError::InvalidRequest)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| ServerError::InvalidRequest)
    }
}

fn task_state_name(value: A2aTaskState) -> &'static str {
    match value {
        A2aTaskState::Submitted => "SUBMITTED",
        A2aTaskState::Working => "WORKING",
        A2aTaskState::InputRequired => "INPUT_REQUIRED",
        A2aTaskState::Completed => "COMPLETED",
        A2aTaskState::Failed => "FAILED",
        A2aTaskState::Canceled => "CANCELED",
        A2aTaskState::Rejected => "REJECTED",
    }
}

fn task_state_from_name(value: &str) -> Result<A2aTaskState, ServerError> {
    match value {
        "SUBMITTED" => Ok(A2aTaskState::Submitted),
        "WORKING" => Ok(A2aTaskState::Working),
        "INPUT_REQUIRED" => Ok(A2aTaskState::InputRequired),
        "COMPLETED" => Ok(A2aTaskState::Completed),
        "FAILED" => Ok(A2aTaskState::Failed),
        "CANCELED" => Ok(A2aTaskState::Canceled),
        "REJECTED" => Ok(A2aTaskState::Rejected),
        _ => Err(ServerError::InvalidRequest),
    }
}

fn pairing_state_name(value: PairingState) -> &'static str {
    match value {
        PairingState::Pending => "pending",
        PairingState::Approved => "approved",
        PairingState::Rejected => "rejected",
        PairingState::Claimed => "claimed",
        PairingState::Expired => "expired",
    }
}

fn pairing_state_from_name(value: &str) -> Result<PairingState, ServerError> {
    match value {
        "pending" => Ok(PairingState::Pending),
        "approved" => Ok(PairingState::Approved),
        "rejected" => Ok(PairingState::Rejected),
        "claimed" => Ok(PairingState::Claimed),
        "expired" => Ok(PairingState::Expired),
        _ => Err(ServerError::InvalidRequest),
    }
}

pub(super) fn local_approval_status_from_name(
    value: &str,
) -> Result<LocalApprovalStatus, ServerError> {
    match value {
        "pending" => Ok(LocalApprovalStatus::Pending),
        "approved" => Ok(LocalApprovalStatus::Approved),
        "rejected" => Ok(LocalApprovalStatus::Rejected),
        "timed_out" => Ok(LocalApprovalStatus::TimedOut),
        "consumed" => Ok(LocalApprovalStatus::Consumed),
        "invalidated" => Ok(LocalApprovalStatus::Invalidated),
        _ => Err(ServerError::InvalidRequest),
    }
}

pub(super) fn valid_local_approval_id(value: &str) -> bool {
    value.len() == 73
        && value.starts_with("approval-")
        && value[9..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_current_schema(connection: &Connection) -> Result<(), ServerError> {
    for table in [
        "a2a_server_tasks",
        "a2a_server_peer_grants",
        "a2a_server_pairings",
        "a2a_server_approvals",
    ] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                params![table],
                |row| row.get(0),
            )
            .map_err(|_| ServerError::UnsupportedStateSchema)?;
        if !exists {
            return Err(ServerError::UnsupportedStateSchema);
        }
    }
    let mut statement = connection
        .prepare("PRAGMA table_info(a2a_server_approvals)")
        .map_err(|_| ServerError::UnsupportedStateSchema)?;
    let has_turn_id = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| ServerError::UnsupportedStateSchema)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ServerError::UnsupportedStateSchema)?
        .iter()
        .any(|column| column == "turn_id");
    if has_turn_id {
        Ok(())
    } else {
        Err(ServerError::UnsupportedStateSchema)
    }
}
