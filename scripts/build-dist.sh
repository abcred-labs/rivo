#!/usr/bin/env bash
set -eu

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
runtime_dir="$repo_root/runtime"
dist_dir="$repo_root/dist"
configuration="release"
platform=""
arch=""
version=""

usage() {
  printf 'Usage: %s [--debug]\n' "$0"
}

if [ "$#" -gt 1 ]; then
  usage >&2
  exit 2
fi
if [ "$#" -eq 1 ]; then
  case "$1" in
    --debug) configuration="debug" ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
fi

command -v cargo >/dev/null 2>&1 || {
  printf 'cargo is required; install Rust with rustup first\n' >&2
  exit 1
}
command -v python3 >/dev/null 2>&1 || {
  printf 'python3 is required to read the workspace version\n' >&2
  exit 1
}

version="$(python3 - "$runtime_dir/Cargo.toml" <<'PY'
import re
import sys

text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r"(?ms)^\[workspace\.package\].*?^version\s*=\s*\"([^\"]+)\"", text)
if match is None:
    raise SystemExit("workspace version missing from runtime/Cargo.toml")
print(match.group(1))
PY
)"

case "$(uname -s)" in
  Darwin) platform="macos" ;;
  Linux) platform="linux" ;;
  *)
    printf 'unsupported operating system: %s\n' "$(uname -s)" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  arm64|aarch64) arch="arm64" ;;
  amd64|x86_64) arch="amd64" ;;
  *)
    printf 'unsupported architecture: %s\n' "$(uname -m)" >&2
    exit 1
    ;;
esac

if [ "$configuration" = "release" ]; then
  cargo build --release --locked --manifest-path "$runtime_dir/Cargo.toml"
else
  cargo build --locked --manifest-path "$runtime_dir/Cargo.toml"
fi

built_binary="$runtime_dir/target/$configuration/rivo"
if [ ! -f "$built_binary" ]; then
  printf 'built binary not found: %s\n' "$built_binary" >&2
  exit 1
fi

output_dir="$dist_dir/$platform-$arch/$version/$configuration"
mkdir -p "$output_dir"
cp "$built_binary" "$output_dir/rivo"
chmod +x "$output_dir/rivo"
printf 'Built (%s): %s\n' "$configuration" "$output_dir/rivo"
