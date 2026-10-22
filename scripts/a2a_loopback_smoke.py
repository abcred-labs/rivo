#!/usr/bin/env python3
"""Local A2A PSK smoke between two independent rivo processes on loopback.

    python scripts/a2a_loopback_smoke.py --rivo runtime/target/debug/rivo [--client-rivo OLD_RIVO]

Starts `rivo a2a serve` (PSK transport) on 127.0.0.1 with a throwaway data directory and
test identity, then runs `rivo a2a pair` from separate client processes and checks:
  * the correct PSK pairs and the peer becomes Trusted,
  * a wrong PSK is rejected and creates no peer,
  * a wrong SPKI pin is rejected.
Pass --client-rivo to pair with a different build (for example the previous release) to
check protocol compatibility across versions. Uses RIVO_A2A_LOCAL_TEST, which only allows
loopback binds; nothing leaves the host. Test identities are cleaned up afterwards.
"""
from __future__ import annotations

import argparse
import os
import re
import secrets
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def toml_path(path: Path) -> str:
    return path.as_posix()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rivo", type=Path, required=True, help="server (and default client) binary")
    parser.add_argument("--client-rivo", type=Path, help="client binary, defaults to --rivo")
    args = parser.parse_args()
    server_bin = args.rivo.resolve()
    client_bin = (args.client_rivo or args.rivo).resolve()
    for binary in (server_bin, client_bin):
        if not binary.is_file():
            raise SystemExit(f"binary not found: {binary}")

    port = free_port()
    suffix = secrets.token_hex(3)
    server_ref, issuer_ref = f"a2a-test-srv-{suffix}", f"a2a-test-iss-{suffix}"
    psk = secrets.token_hex(32)
    results: list[tuple[str, bool]] = []
    server: subprocess.Popen[str] | None = None

    with tempfile.TemporaryDirectory(prefix="rivo-a2a-loopback-") as directory:
        root = Path(directory)
        for name in ("workspace", "server", "client-ok/rivo", "client-bad/rivo"):
            (root / name).mkdir(parents=True)
        base_env = dict(os.environ, RIVO_A2A_LOCAL_TEST="1", RIVO_A2A_LOCAL_TEST_CONFIRM="1")

        def run(binary: Path, data: str, arguments: list[str], stdin: str | None = None, timeout: int = 60):
            env = dict(base_env, RIVO_DATA_DIR=str(root / data))
            return subprocess.run(
                [str(binary), *arguments], input=stdin, env=env, text=True, encoding="utf-8",
                capture_output=True, timeout=timeout,
            )

        try:
            provisioned = run(server_bin, "server", ["a2a", "provision", "--yes", "--bind", f"127.0.0.1:{port}",
                                                     "--server-identity-ref", server_ref, "--issuer-ref", issuer_ref])
            if provisioned.returncode != 0:
                print(provisioned.stdout + provisioned.stderr, file=sys.stderr)
                return 1
            config = root / "server.toml"
            config.write_text(
                f'schema_version = 1\nstate_db = "{toml_path(root / "server" / "state.db")}"\n\n'
                f'[workspace_aliases]\nresearch = "{toml_path(root / "workspace")}"\n\n'
                f'[[profiles]]\nidentity_ref = "{server_ref}"\n\n[profiles.profile]\nname = "research-lan"\n'
                f'issuer_ref = "{issuer_ref}"\nbind = "127.0.0.1:{port}"\nskill_id = "research"\n'
                'workspace_alias = "research"\ncapability_profile = "research"\npolicy_ceiling = "strict"\n'
                'network_enabled = false\nbrowser_enabled = false\nmcp_servers = []\nmcp_toolsets = {}\n'
                'per_peer_max_active_tasks = 1\nmax_active_tasks = 3\nmax_message_bytes = 65536\n'
                'max_output_bytes = 262144\nmax_duration_seconds = 1800\napproval_timeout_seconds = 300\n',
                encoding="utf-8",
            )

            def start_server(allowed: str | None) -> tuple[subprocess.Popen[str], str]:
                text = config.read_text(encoding="utf-8")
                text = re.sub(r"^allowed_peers = .*\n", "", text, flags=re.M)
                if allowed:
                    text = text.replace('name = "research-lan"\n', f'name = "research-lan"\nallowed_peers = ["{allowed}"]\n', 1)
                config.write_text(text, encoding="utf-8")
                env = dict(base_env, RIVO_DATA_DIR=str(root / "server"))
                process = subprocess.Popen(
                    [str(server_bin), "a2a", "serve", "research-lan", "--config", str(config),
                     "--transport", "psk", "--secret-stdin"],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, text=True,
                    encoding="utf-8",
                )
                assert process.stdin and process.stdout
                process.stdin.write(psk + "\n")
                process.stdin.close()
                line = process.stdout.readline()
                match = re.search(r"spki_sha256=([0-9a-f]{64})", line)
                if not match:
                    raise SystemExit(f"server did not start: {line!r}")
                return process, match.group(1)

            # First start without an allowlist: the client prints the deterministic PSK peer id.
            server, spki = start_server(None)
            endpoint = f"https://127.0.0.1:{port}/a2a"
            probe = run(client_bin, "client-bad", ["a2a", "pair", "probe", "--endpoint", endpoint,
                                                   "--spki-sha256", spki, "--transport", "psk", "--secret-stdin"], psk + "\n")
            match = re.search(r"peer_id=(peer-[0-9a-f]+)", probe.stdout + probe.stderr)
            if not match:
                print(probe.stdout + probe.stderr, file=sys.stderr)
                return 1
            peer_id = match.group(1)
            server.kill()
            server.wait()
            server, spki = start_server(peer_id)

            ok = run(client_bin, "client-ok", ["a2a", "pair", "smoke", "--endpoint", endpoint, "--spki-sha256", spki,
                                               "--transport", "psk", "--secret-stdin"], psk + "\n")
            results.append(("correct PSK pairs", ok.returncode == 0 and "paired peer_id=" in ok.stdout + ok.stderr))
            peers = run(client_bin, "client-ok", ["a2a", "peers"])
            results.append(("peer is Trusted", "status=Trusted" in peers.stdout))

            bad = run(client_bin, "client-bad", ["a2a", "pair", "bad", "--endpoint", endpoint, "--spki-sha256", spki,
                                                 "--transport", "psk", "--secret-stdin"], secrets.token_hex(32) + "\n")
            bad_peers = run(client_bin, "client-bad", ["a2a", "peers"])
            results.append(("wrong PSK is rejected", bad.returncode != 0 and "a2a_psk" in bad.stderr))
            results.append(("wrong PSK creates no peer", "peer-" not in bad_peers.stdout))

            pinned = run(client_bin, "client-bad", ["a2a", "pair", "bad2", "--endpoint", endpoint, "--spki-sha256", "0" * 64,
                                                    "--transport", "psk", "--secret-stdin"], psk + "\n")
            results.append(("wrong SPKI pin is rejected", pinned.returncode != 0))
        finally:
            if server is not None:
                server.kill()
                server.wait()
                time.sleep(0.5)
            run(server_bin, "server", ["a2a", "provision", "cleanup", "--yes", "--server-identity-ref", server_ref,
                                       "--issuer-ref", issuer_ref])

    for name, passed in results:
        print(f"[{'PASS' if passed else 'FAIL'}] {name}")
    return 0 if results and all(passed for _, passed in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
