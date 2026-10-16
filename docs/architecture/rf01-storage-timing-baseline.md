# RF-01 Storage 时序与故障基线

状态：`completed`

本文件记录 RF-01 的可重复基线，不代表已迁移 Storage worker，也不改变当前 V1
同步 SQLite 执行模型。基线的目的，是在任何执行边界重构前固定顺序、取消、事务和
故障语义，避免用重构后的实现反向定义成功标准。

## 已完成基线

- `poisoned_storage_lock_is_reported_as_a_structured_storage_error`：SQLite mutex
  poisoned 时返回 `StorageError::Unavailable`，不继续使用 poisoned guard。
- `storage_lock_contention_is_serialized_until_the_guard_is_released`：通过 barrier
  验证同步锁竞争的先后关系，不依赖墙钟阈值。
- `append_event_rolls_back_session_and_run_on_sequence_conflict`：重复 event sequence
  触发 SQLite 错误时，session/turn/event 写入保持事务原子性，不留下孤立 turn。
- `opening_a_directory_returns_a_structured_sqlite_error`：数据库路径不可打开时返回
  `StorageError::Sqlite`，不发生 panic。
- `cancel_stops_a_live_provider_stream_before_final_answer`：provider stream 在受控
  `Notify` 闸门后才允许完成；取消会先产生 `Stopped`，且在释放 provider 闸门前已
  完成终态事件持久化，不再追加最终答案，不依赖固定 sleep 或机器速度。
- `owner_queue_returns_rejected_when_task_is_cancelled_while_waiting`：本地 owner 审批
  轮询期间由同一任务的取消操作使 pending 记录失效，等待方返回 `Rejected`，验证
  SQL 排队与取消不会继续等待或错误地接受批准。
- `interrupted_migration_returns_structured_sqlite_error_at_checkpoint`：模拟部分应用
  migration 后重复添加列，确认迁移失败返回 `StorageError::Sqlite`，并保留已完成的
  schema checkpoint，便于诊断和后续修复，不发生 panic。
- `readonly_connection_returns_structured_write_error_without_partial_event`：将 SQLite
  连接切换为只读后尝试追加事件，确认写入失败映射为 `StorageError::Sqlite`，且不会
  留下部分 event/session/turn 记录。
- `explicit_close_succeeds_for_a_healthy_store` 与
  `explicit_close_reports_an_active_transaction_as_structured_error`：显式关闭健康连接
  成功；关闭仍有活动 statement 的连接返回 `StorageError::Sqlite`，关闭故障不转化为
  panic。
- `tokio_scheduler_observes_sync_storage_lock_blocking_without_wall_clock_thresholds`：在
  双 worker Tokio fixture 中，一条任务同步等待 Storage mutex 时，另一条任务仍通过
  协作信号完成；测试固定阻塞/释放顺序，不使用机器相关的毫秒阈值，记录当前同步
  SQLite 边界的调度影响。
- `page_limit_write_failure_is_structured_and_atomic`：将文件数据库的
  `max_page_count` 固定为当前页数，注入确定性的 `SQLITE_FULL` 写入失败，确认返回
  `StorageError::Sqlite` 且事务不会留下部分 session/turn/event。
- `configured_storage_persists_ordered_turn_events`：Session 在终态后释放 channel 和
  实例，再重新打开数据库回放事件，确认 drop 后历史序列完整且终态唯一。Rust `Drop`
  本身不能返回错误，因此退出期间的持久化失败必须通过显式 `Store::close` 或上层
  shutdown API 传播，不能把失败隐藏在析构函数中。

上述测试只验证当前行为基线，聚焦命令为：

```text
cd runtime
cargo test -p rivo-session -p rivo-storage --locked
```

## 尚未完成的 RF-01 门槛

### 调度延迟

同步 mutex 持锁期间的 Tokio 调度影响已由协作式双 worker fixture 固定下来；这只是
当前同步实现的行为基线，不代表异步执行边界已经改善。未来 worker 迁移必须使用同一
fixture 对照验证排队和取消顺序。

### 取消响应

需要补充取消请求从进入 Session channel 到安全检查点、`Stopped` 事件和必要持久化
结果的顺序矩阵。当前 stream + SQLite 取消测试已覆盖 provider 完成前的终态持久化，
但尚未覆盖 SQL 请求排队、关闭和取消同时发生时的全部组合。

### 故障注入

当前已覆盖事务约束冲突、损坏事件、poisoned lock、连接显式关闭、migration 中断和
确定性的 `SQLITE_FULL` 写入失败、Session drop 后的历史恢复，以及 CLI 显式退出路径的
可观测 shutdown 结果均已覆盖。Rust `Drop` 无法返回错误，CLI 通过显式
`Session::shutdown_async` 传播关闭边界；失败只记录诊断，不改变既有 turn 退出码和 JSONL
事件语义。

## Gate 条件

RF-01 已同时满足以下条件并标记 `completed`：

1. 调度、取消和故障矩阵均有不依赖环境偶然性的确定性测试；
2. `cargo test -p rivo-session -p rivo-storage --locked` 通过；
3. 完整 `cargo xtask check`、coverage、CLI/TUI/SQLite/A2A/MCP/Web 回归通过；
4. release-test 报告的 `Commit` 与工作树均对应同一个干净候选提交。

RF-01 Gate 已关闭；RF-08 仍须先完成执行模型设计 Gate，才能把同步 SQLite 迁移到
worker 或 `spawn_blocking` 生产路径。
