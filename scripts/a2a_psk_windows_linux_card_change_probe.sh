#!/usr/bin/env bash
set -euo pipefail

# Verifies that a PSK peer remains SPKI-pinned across a server restart but is
# suspended when the public Agent Card changes. No PSK is written to disk.
remote_host="${1:?remote host required}"
windows_rivo="${2:?Windows rivo.exe path required}"
remote_binary="/tmp/rivo-a2a-src/runtime/target/release/rivo"
remote_config="/tmp/rivo-a2a-smoke/server.toml"
altered_config="/tmp/rivo-a2a-smoke/server-card-change.toml"
remote_data="/tmp/rivo-a2a-smoke/data"
workspace_root="$(cd "$(dirname "$0")/.." && pwd)"
psk="$(openssl rand -hex 32)"
client_dir="$(mktemp -u "${workspace_root}/.a2a-psk-card.XXXXXX")"
windows_rivo="$(wslpath -w "$windows_rivo")"
windows_data_dir="$(wslpath -w "$client_dir")"
powershell.exe -NoProfile -NonInteractive -Command \
  "New-Item -ItemType Directory -Force -Path '${windows_data_dir}\\rivo' | Out-Null"

run_windows() {
  local escaped_binary="${windows_rivo//\'/\'\'}"
  local command="& '${escaped_binary}'"
  local argument escaped
  for argument in "$@"; do
    escaped="${argument//\'/\'\'}"
    command+=" '${escaped}'"
  done
  powershell.exe -NoProfile -NonInteractive -Command "\$env:RIVO_DATA_DIR='${windows_data_dir}'; ${command}"
}

server_pid=""
server_log="$(mktemp)"
start_server() {
  local config="$1"
  : > "$server_log"
  printf '%s\n' "$psk" | ssh -o BatchMode=yes "root@${remote_host}" \
    "RIVO_DATA_DIR=${remote_data} ${remote_binary} a2a serve research-lan --config ${config} --transport psk --secret-stdin" \
    >"$server_log" 2>&1 &
  server_pid="$!"
  for _ in $(seq 1 50); do
    if grep -q 'spki_sha256=' "$server_log"; then
      sed -n 's/.*spki_sha256=\([0-9a-f]\{64\}\).*/\1/p' "$server_log"
      return 0
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then
      cat "$server_log" >&2
      return 1
    fi
    sleep 0.1
  done
  return 1
}

stop_server() {
  [[ -z "$server_pid" ]] || kill "$server_pid" 2>/dev/null || true
  [[ -z "$server_pid" ]] || wait "$server_pid" 2>/dev/null || true
  server_pid=""
  ssh -o BatchMode=yes "root@${remote_host}" \
    "pkill -f '${remote_binary} a2a serve research-lan --config /tmp/rivo-a2a-smoke/server' || true" \
    >/dev/null 2>&1 || true
}

cleanup() {
  stop_server
  ssh -o BatchMode=yes "root@${remote_host}" "rm -f '${altered_config}'" >/dev/null 2>&1 || true
  rm -rf "$client_dir" "$server_log"
}
trap cleanup EXIT

original_spki="$(start_server "$remote_config")"
peer_id="$(curl -ksS -D - -o /dev/null "https://${remote_host}:8443/.well-known/agent-card.json" | tr -d '\r' | sed -n 's/^x-rivo-psk-peer-id: //Ip')"
if [[ ! "$peer_id" =~ ^peer-[0-9a-f]{32}$ ]]; then
  printf 'psk_peer_id_missing\n' >&2
  exit 1
fi
printf '%s\n' "$psk" | run_windows a2a pair card-change \
  --endpoint "https://${remote_host}:8443/a2a" --spki-sha256 "$original_spki" \
  --transport psk --secret-stdin >/dev/null

stop_server
ssh -o BatchMode=yes "root@${remote_host}" \
  "sed 's/skill_id = \"research\"/skill_id = \"review\"/' '${remote_config}' > '${altered_config}'"
changed_spki="$(start_server "$altered_config")"
if [[ "$changed_spki" != "$original_spki" ]]; then
  printf 'psk_spki_changed_after_restart\n' >&2
  exit 1
fi

set +e
changed_output="$(printf '%s\ncard-change request\n' "$psk" | run_windows a2a delegate "$peer_id" research \
  --message-stdin --yes --transport psk --secret-stdin 2>&1)"
changed_status=$?
set -e
if (( changed_status == 0 )) || ! grep -q 'a2a_agent_card_changed' <<<"$changed_output"; then
  printf 'agent_card_change_not_suspended\n' >&2
  exit 1
fi
inspect_output="$(run_windows a2a inspect "$peer_id")"
if ! grep -q 'status=Suspended' <<<"$inspect_output"; then
  printf 'agent_card_change_not_persisted\n' >&2
  exit 1
fi

printf 'card_change_probe=passed peer_id=%s spki_stable=true status=suspended\n' "$peer_id"
