#!/usr/bin/env bash
set -euo pipefail

remote_host="${1:?remote host required}"
windows_rivo="${2:?Windows rivo.exe path required}"
remote_binary="/tmp/rivo-a2a-src/runtime/target/release/rivo"
remote_config="/tmp/rivo-a2a-smoke/server.toml"
remote_data="/tmp/rivo-a2a-smoke/data"
workspace_root="$(cd "$(dirname "$0")/.." && pwd)"

psk="$(openssl rand -hex 32)"
coproc SERVER {
  printf '%s\n' "$psk" | ssh -o BatchMode=yes "root@${remote_host}" \
    "RIVO_DATA_DIR=${remote_data} ${remote_binary} a2a serve research-lan --config ${remote_config} --transport psk --secret-stdin" 2>&1
}

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  ssh -o BatchMode=yes "root@${remote_host}" \
    "pkill -f '${remote_binary} a2a serve research-lan --config ${remote_config}' || true" \
    >/dev/null 2>&1 || true
  [[ -z "${client_dir:-}" ]] || rm -rf "$client_dir"
}
trap cleanup EXIT

IFS= read -r line <&"${SERVER[0]}"
if [[ ! "$line" =~ spki_sha256=([0-9a-f]{64}) ]]; then
  printf 'server_start_failed\n' >&2
  exit 1
fi
spki="${BASH_REMATCH[1]}"
server_peer_id="$(curl -ksS -D - -o /dev/null "https://${remote_host}:8443/.well-known/agent-card.json" | tr -d '\r' | sed -n 's/^x-rivo-psk-peer-id: //Ip')"
printf 'server_psk_peer_id=%s\n' "$server_peer_id"
client_dir="$(mktemp -u "${workspace_root}/.a2a-psk-client.XXXXXX")"
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
set +e
printf '%s\n' "$psk" | run_windows a2a pair smoke \
  --endpoint "https://${remote_host}:8443/a2a" \
  --spki-sha256 "$spki" \
  --transport psk --secret-stdin
pair_status=$?
set -e
if (( pair_status != 0 )); then
  if IFS= read -r -t 1 diagnostic <&"${SERVER[0]}"; then
    printf '%s\n' "$diagnostic" >&2
  fi
  exit "$pair_status"
fi
printf 'pair_probe=passed\n'
