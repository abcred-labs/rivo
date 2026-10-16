#!/usr/bin/env bash
set -euo pipefail

# Real Windows-client -> Linux-server PSK lifecycle probe. The shared secret
# only exists in this process and is piped to each CLI invocation via stdin.
remote_host="${1:?remote host required}"
windows_rivo="${2:?Windows rivo.exe path required}"
remote_binary="/tmp/rivo-a2a-src/runtime/target/release/rivo"
remote_config="/tmp/rivo-a2a-smoke/server.toml"
remote_data="/tmp/rivo-a2a-smoke/data"

psk="$(openssl rand -hex 32)"
workspace_root="$(cd "$(dirname "$0")/.." && pwd)"
client_dir="$(mktemp -u "${workspace_root}/.a2a-psk-client.XXXXXX")"
windows_rivo="$(wslpath -w "$windows_rivo")"
windows_data_dir="$(wslpath -w "$client_dir")"
powershell.exe -NoProfile -NonInteractive -Command \
  "New-Item -ItemType Directory -Force -Path '${windows_data_dir}\\rivo' | Out-Null"

run_windows_at() {
  local data_dir="$1"
  shift
  # WSL does not reliably forward ad-hoc environment variables to .exe
  # children. Set the isolated data directory in the Windows process itself.
  local escaped_binary="${windows_rivo//\'/\'\'}"
  local command="& '${escaped_binary}'"
  local argument escaped
  for argument in "$@"; do
    escaped="${argument//\'/\'\'}"
    command+=" '${escaped}'"
  done
  powershell.exe -NoProfile -NonInteractive -Command "\$env:RIVO_DATA_DIR='${data_dir}'; ${command}"
}

run_windows() {
  run_windows_at "$windows_data_dir" "$@"
}

coproc SERVER {
  printf '%s\n' "$psk" | ssh -o BatchMode=yes "root@${remote_host}" \
    "RIVO_A2A_SMOKE_HOLD_MS=30000 RIVO_DATA_DIR=${remote_data} ${remote_binary} a2a serve research-lan --config ${remote_config} --transport psk --secret-stdin" 2>&1
}

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  ssh -o BatchMode=yes "root@${remote_host}" \
    "pkill -f '${remote_binary} a2a serve research-lan --config ${remote_config}' || true" \
    >/dev/null 2>&1 || true
  rm -rf "$client_dir"
  [[ -z "${wrong_dir:-}" ]] || rm -rf "$wrong_dir"
}
trap cleanup EXIT

IFS= read -r line <&"${SERVER[0]}"
if [[ ! "$line" =~ spki_sha256=([0-9a-f]{64}) ]]; then
  printf 'server_start_failed\n' >&2
  exit 1
fi
spki="${BASH_REMATCH[1]}"
peer_id="$(curl -ksS -D - -o /dev/null "https://${remote_host}:8443/.well-known/agent-card.json" | tr -d '\r' | sed -n 's/^x-rivo-psk-peer-id: //Ip')"
if [[ ! "$peer_id" =~ ^peer-[0-9a-f]{32}$ ]]; then
  printf 'psk_peer_id_missing\n' >&2
  exit 1
fi

printf '%s\n' "$psk" | run_windows a2a pair smoke \
  --endpoint "https://${remote_host}:8443/a2a" --spki-sha256 "$spki" \
  --transport psk --secret-stdin >/dev/null

request_nonce="$(openssl rand -hex 16)"
delegate_output="$(printf '%s\nsmoke lifecycle %s\n' "$psk" "$request_nonce" | run_windows a2a delegate "$peer_id" research \
  --message-stdin --yes --transport psk --secret-stdin)"
task_id="$(sed -n 's/^task_id=\([^ ]*\).*/\1/p' <<<"$delegate_output")"
if [[ ! "$task_id" =~ ^task-[0-9a-f]+$ ]]; then
  printf 'delegate_did_not_create_task\n' >&2
  exit 1
fi
if ! grep -q 'state=Working' <<<"$delegate_output"; then
  printf 'delegate_state=%s\n' "$delegate_output" >&2
  printf 'delegate_not_working\n' >&2
  exit 1
fi

# `recover` must use GetTask/subscription for the known task rather than send
# a second delegation. A fresh PSK bootstrap is expected for this CLI process.
recover_output="$(printf '%s\n' "$psk" | run_windows a2a recover "$peer_id" "$task_id" --transport psk --secret-stdin)"
if ! grep -q "task_id=${task_id} state=Working" <<<"$recover_output"; then
  printf 'recover_not_working\n' >&2
  exit 1
fi

# Recovery is read-only for an existing task. Exercise a second independent
# recovery before cancellation to prove it does not create a second task or
# get rejected as a replay.
recover_again_output="$(printf '%s\n' "$psk" | run_windows a2a recover "$peer_id" "$task_id" --transport psk --secret-stdin)"
if ! grep -q "task_id=${task_id} state=Working" <<<"$recover_again_output"; then
  printf 'second_recover_not_working\n' >&2
  exit 1
fi

cancel_output="$(printf '%s\n' "$psk" | run_windows a2a cancel "$peer_id" "$task_id" --yes --transport psk --secret-stdin)"
if ! grep -q "task_id=${task_id} state=Canceled" <<<"$cancel_output"; then
  printf 'cancel_not_terminal\n' >&2
  exit 1
fi

run_windows a2a revoke "$peer_id" --yes >/dev/null
set +e
revoke_output="$(printf '%s\npost-revoke request\n' "$psk" | run_windows a2a delegate "$peer_id" research \
  --message-stdin --yes --transport psk --secret-stdin 2>&1)"
revoke_status=$?
set -e
if (( revoke_status == 0 )) || ! grep -Eq 'a2a_|peer|信任|撤销|不存在' <<<"$revoke_output"; then
  printf 'client_revoke_not_enforced\n' >&2
  exit 1
fi

# A distinct client with a wrong secret must fail during bootstrap and must
# not create a trusted peer or a server task.
wrong_dir="$(mktemp -u "${workspace_root}/.a2a-psk-wrong.XXXXXX")"
wrong_data_dir="$(wslpath -w "$wrong_dir")"
powershell.exe -NoProfile -NonInteractive -Command \
  "New-Item -ItemType Directory -Force -Path '${wrong_data_dir}\\rivo' | Out-Null"
wrong_psk="$(openssl rand -hex 32)"
set +e
wrong_output="$(printf '%s\n' "$wrong_psk" | run_windows_at "$wrong_data_dir" a2a pair wrong-secret \
  --endpoint "https://${remote_host}:8443/a2a" --spki-sha256 "$spki" --transport psk --secret-stdin 2>&1)"
wrong_status=$?
set -e
rm -rf "$wrong_dir"
if (( wrong_status == 0 )) || ! grep -q 'a2a_psk_transport' <<<"$wrong_output"; then
  printf 'wrong_secret_not_rejected\n' >&2
  exit 1
fi

printf 'lifecycle_probe=passed peer_id=%s task_id=%s states=WORKING,RECOVERED_TWICE,CANCELED,REVOKED negatives=wrong-secret,revoke\n' "$peer_id" "$task_id"
