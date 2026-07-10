#!/bin/sh
set -eu

REPOSITORY="randomvibecoder/subagent"
ASSET="subagent-linux-x86_64"
VERSION=${SUBAGENT_VERSION:-latest}
INSTALL_DIR=${SUBAGENT_INSTALL_DIR:-"${HOME:?HOME is not set}/.local/bin"}

fail() {
    printf 'subagent installer: %s\n' "$*" >&2
    exit 1
}

download() {
    url=$1
    output=$2
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$output"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$output" "$url"
    else
        fail "curl or wget is required"
    fi
}

sha256() {
    file=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$file" | awk '{print $NF}'
    else
        fail "sha256sum, shasum, or openssl is required to verify the download"
    fi
}

[ "$(uname -s)" = "Linux" ] || fail "only Linux is supported"
case "$(uname -m)" in
    x86_64 | amd64) ;;
    *) fail "unsupported architecture: $(uname -m); this release supports x86_64 Linux" ;;
esac

case "$VERSION" in
    latest)
        BASE_URL="https://github.com/$REPOSITORY/releases/latest/download"
        ;;
    v[0-9]*.[0-9]*.[0-9]*)
        BASE_URL="https://github.com/$REPOSITORY/releases/download/$VERSION"
        ;;
    [0-9]*.[0-9]*.[0-9]*)
        VERSION="v$VERSION"
        BASE_URL="https://github.com/$REPOSITORY/releases/download/$VERSION"
        ;;
    *)
        fail "invalid SUBAGENT_VERSION: $VERSION (expected latest, vX.Y.Z, or X.Y.Z)"
        ;;
esac

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/subagent-install.XXXXXX") || fail "cannot create temporary directory"
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

printf 'Downloading %s for Linux x86_64...\n' "$ASSET"
download "$BASE_URL/$ASSET" "$TMP_DIR/$ASSET"
download "$BASE_URL/$ASSET.sha256" "$TMP_DIR/$ASSET.sha256"

expected=$(awk 'NR == 1 {print $1}' "$TMP_DIR/$ASSET.sha256")
actual=$(sha256 "$TMP_DIR/$ASSET")
[ -n "$expected" ] || fail "release checksum is empty"
[ "$actual" = "$expected" ] || fail "checksum mismatch: expected $expected, got $actual"

mkdir -p "$INSTALL_DIR"
staged="$INSTALL_DIR/.subagent-install.$$"
cp "$TMP_DIR/$ASSET" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$INSTALL_DIR/subagent"

printf 'Installed %s\n' "$INSTALL_DIR/subagent"
"$INSTALL_DIR/subagent" --version

case ":${PATH:-}:" in
    *:"$INSTALL_DIR":*) ;;
    *)
        printf '\nAdd this directory to PATH:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
        ;;
esac
