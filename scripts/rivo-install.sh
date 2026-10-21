#!/bin/sh
set -eu

GITHUB_ORIGIN=https://github.com
ASSETS_ORIGIN=https://release-assets.githubusercontent.com
GITHUB_MIRROR="${RIVO_GITHUB_MIRROR:-}"
ASSETS_MIRROR="${RIVO_RELEASE_ASSETS_MIRROR:-}"
GITHUB_MIRROR="${GITHUB_MIRROR%/}"
ASSETS_MIRROR="${ASSETS_MIRROR%/}"
for MIRROR_VALUE in "$GITHUB_MIRROR" "$ASSETS_MIRROR"; do
    [ -z "$MIRROR_VALUE" ] && continue
    printf '%s
' "$MIRROR_VALUE" | grep -Eq '^https://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?$' || {
        echo "RIVO_GITHUB_MIRROR and RIVO_RELEASE_ASSETS_MIRROR must be https:// URLs" >&2
        exit 1
    }
done

REPOSITORY="${RIVO_REPOSITORY:-abcred-labs/rivo}"
VERSION="${RIVO_INSTALL_VERSION:-latest}"
INSTALL_ROOT="${RIVO_INSTALL_ROOT:-/usr/local/lib/rivo}"
BIN_DIR="${RIVO_INSTALL_BIN_DIR:-}"
BASE_URL="${RIVO_INSTALL_BASE_URL:-}"

usage() {
    cat <<'EOF'
Install the latest stable Rivo release.

Options:
  --version VERSION       Install VERSION instead of latest
  --install-root PATH     Override the system install root
  --base-url URL          Use a release mirror
  -h, --help              Show this help

Mainland China networks can set these before running the installer:
  RIVO_GITHUB_MIRROR          e.g. https://github-mirror.abc.red
  RIVO_RELEASE_ASSETS_MIRROR  e.g. https://gh-release-assets.abc.red
Mirrors only speed up downloads; SHA-256 verification is unchanged.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { echo "--version requires a value" >&2; exit 2; }
            VERSION="$2"
            shift 2
            ;;
        --install-root)
            [ "$#" -ge 2 ] || { echo "--install-root requires a value" >&2; exit 2; }
            INSTALL_ROOT="$2"
            shift 2
            ;;
        --base-url)
            [ "$#" -ge 2 ] || { echo "--base-url requires a value" >&2; exit 2; }
            BASE_URL="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "$(uname -s)" in
    Linux) PLATFORM=linux ;;
    Darwin) PLATFORM=macos ;;
    *) echo "unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
    x86_64|amd64) ARCH=amd64 ;;
    arm64|aarch64) ARCH=arm64 ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [ -z "$BASE_URL" ]; then
    DOWNLOAD_ORIGIN="${GITHUB_MIRROR:-$GITHUB_ORIGIN}"
    if [ "$VERSION" = latest ]; then
        BASE_URL="$DOWNLOAD_ORIGIN/$REPOSITORY/releases/latest/download"
    else
        BASE_URL="$DOWNLOAD_ORIGIN/$REPOSITORY/releases/download/v$VERSION"
    fi
    MANIFEST_URL="$BASE_URL/release-manifest-$PLATFORM-$ARCH.json"
    MIRROR=0
else
    BASE_URL="${BASE_URL%/}"
    case "$BASE_URL" in
        https://*|http://127.0.0.1|http://127.0.0.1[:/]*|http://localhost|http://localhost[:/]*|"http://[::1]"|"http://[::1]"[:/]*) ;;
        *) echo "--base-url must be an https:// URL (http is only allowed for loopback)" >&2; exit 1 ;;
    esac
    MANIFEST_URL="$BASE_URL/$PLATFORM-$ARCH/release-manifest.json"
    MIRROR=1
fi

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "tar is required" >&2; exit 1; }

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/rivo-install.XXXXXX")
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

# Follows redirects one HTTPS hop at a time so GitHub and release-asset
# redirects can be rewritten to the configured mirrors.
fetch() {
    fetch_url=$1
    fetch_hop=0
    while [ "$fetch_hop" -lt 6 ]; do
        fetch_location=$(curl --fail --silent --show-error --retry 3 --tlsv1.2             -w '%{redirect_url}' -o "$2" "$fetch_url") || return 1
        [ -n "$fetch_location" ] || return 0
        case "$fetch_location" in
            https://*) ;;
            *) echo "refusing non-HTTPS redirect" >&2; return 1 ;;
        esac
        if [ -n "$GITHUB_MIRROR" ]; then
            case "$fetch_location" in
                "$GITHUB_ORIGIN"/*) fetch_location="$GITHUB_MIRROR${fetch_location#"$GITHUB_ORIGIN"}" ;;
            esac
        fi
        if [ -n "$ASSETS_MIRROR" ]; then
            case "$fetch_location" in
                "$ASSETS_ORIGIN"/*) fetch_location="$ASSETS_MIRROR${fetch_location#"$ASSETS_ORIGIN"}" ;;
            esac
        fi
        fetch_url=$fetch_location
        fetch_hop=$((fetch_hop + 1))
    done
    echo "too many redirects" >&2
    return 1
}

fetch "$MANIFEST_URL" "$TMP_DIR/manifest.json"

field() {
    sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$TMP_DIR/manifest.json" | head -n 1
}

MANIFEST_SCHEMA=$(sed -n 's/.*"schema"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$TMP_DIR/manifest.json" | head -n 1)
MANIFEST_PRODUCT=$(field product)
MANIFEST_VERSION=$(field version)
MANIFEST_PLATFORM=$(field platform)
MANIFEST_ARCH=$(field arch)
MANIFEST_CHANNEL=$(field channel)
ARTIFACT=$(field file)
EXPECTED_SHA=$(field sha256)

[ "$MANIFEST_SCHEMA" = 2 ] || { echo "unsupported release manifest schema: $MANIFEST_SCHEMA" >&2; exit 1; }
[ "$MANIFEST_PRODUCT" = rivo ] || { echo "release manifest product is not rivo" >&2; exit 1; }
[ "$MANIFEST_CHANNEL" = stable ] || { echo "release manifest is not a stable release" >&2; exit 1; }
[ "$MANIFEST_PLATFORM" = "$PLATFORM" ] || { echo "release manifest platform mismatch" >&2; exit 1; }
[ "$MANIFEST_ARCH" = "$ARCH" ] || { echo "release manifest architecture mismatch" >&2; exit 1; }
printf '%s\n' "$MANIFEST_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || {
    echo "invalid release version: $MANIFEST_VERSION" >&2
    exit 1
}
[ "$VERSION" = latest ] || [ "$VERSION" = "$MANIFEST_VERSION" ] || {
    echo "requested version $VERSION does not match manifest $MANIFEST_VERSION" >&2
    exit 1
}

case "$ARTIFACT" in
    "rivo-$MANIFEST_VERSION-$PLATFORM-$ARCH.tar.gz") ;;
    *) echo "release artifact does not match the target: $ARTIFACT" >&2; exit 1 ;;
esac

if [ "$MIRROR" = 1 ]; then
    ARTIFACT_BASE="$BASE_URL/$PLATFORM-$ARCH"
else
    ARTIFACT_BASE="$BASE_URL"
fi
ARCHIVE="$TMP_DIR/$ARTIFACT"
CHECKSUM="$TMP_DIR/$ARTIFACT.sha256"
fetch "$ARTIFACT_BASE/$ARTIFACT" "$ARCHIVE"
fetch "$ARTIFACT_BASE/$ARTIFACT.sha256" "$CHECKSUM"

if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_SHA=$(sha256sum "$ARCHIVE" | awk '{print $1}')
else
    command -v shasum >/dev/null 2>&1 || { echo "sha256sum or shasum is required" >&2; exit 1; }
    ACTUAL_SHA=$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')
fi
SIDECAR_SHA=$(awk 'NF {print $1; exit}' "$CHECKSUM")
[ "${#EXPECTED_SHA}" -eq 64 ] || { echo "invalid manifest checksum" >&2; exit 1; }
printf '%s\n' "$EXPECTED_SHA" | grep -Eq '^[0-9a-fA-F]{64}$' || { echo "invalid manifest checksum" >&2; exit 1; }
[ "$ACTUAL_SHA" = "$EXPECTED_SHA" ] || { echo "manifest checksum mismatch" >&2; exit 1; }
[ "$ACTUAL_SHA" = "$SIDECAR_SHA" ] || { echo "checksum sidecar mismatch" >&2; exit 1; }

if tar -tzf "$ARCHIVE" | grep -Eq '(^/|(^|/)\.\.(/|$))'; then
    echo "archive contains an unsafe path" >&2
    exit 1
fi
tar -xzf "$ARCHIVE" -C "$TMP_DIR" --no-same-owner
PAYLOAD="$TMP_DIR/rivo-$MANIFEST_VERSION-$PLATFORM-$ARCH"
[ -x "$PAYLOAD/rivo" ] || { echo "archive does not contain an executable rivo" >&2; exit 1; }
[ -f "$PAYLOAD/share/rivo/languages/en-US.ftl" ] || { echo "archive is missing language resources" >&2; exit 1; }

case "$INSTALL_ROOT" in
    ""|/) echo "refusing unsafe install root: $INSTALL_ROOT" >&2; exit 1 ;;
esac
if [ -z "$BIN_DIR" ]; then
    if [ "$INSTALL_ROOT" = "/usr/local/lib/rivo" ]; then
        BIN_DIR=/usr/local/bin
    else
        BIN_DIR="$INSTALL_ROOT/bin"
    fi
fi
mkdir -p "$INSTALL_ROOT/versions" "$(dirname "$INSTALL_ROOT")" "$BIN_DIR"

VERSION_DIR="$INSTALL_ROOT/versions/$MANIFEST_VERSION"
OLD_DIR=""
if [ -e "$VERSION_DIR" ]; then
    OLD_DIR="$VERSION_DIR.old.$$"
    mv "$VERSION_DIR" "$OLD_DIR"
fi
if ! mv "$PAYLOAD" "$VERSION_DIR"; then
    [ -z "$OLD_DIR" ] || mv "$OLD_DIR" "$VERSION_DIR"
    echo "failed to place the new version" >&2
    exit 1
fi

if [ -L "$INSTALL_ROOT/current" ]; then
    OLD_TARGET=$(readlink "$INSTALL_ROOT/current")
    ln -sfn "$OLD_TARGET" "$INSTALL_ROOT/previous.new"
    mv -f "$INSTALL_ROOT/previous.new" "$INSTALL_ROOT/previous"
fi
ln -sfn "versions/$MANIFEST_VERSION" "$INSTALL_ROOT/current.new"
mv -f "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"
ln -sfn "$INSTALL_ROOT/current/rivo" "$BIN_DIR/rivo.new"
mv -f "$BIN_DIR/rivo.new" "$BIN_DIR/rivo"

[ -z "$OLD_DIR" ] || rm -rf "$OLD_DIR"

echo "installed Rivo $MANIFEST_VERSION ($PLATFORM/$ARCH)"
echo "run: rivo --version"
