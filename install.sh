#!/bin/sh
set -eu

REPO="tontinton/maki"
BINARY="maki"
INSTALL_DIR="${MAKI_INSTALL_DIR:-/usr/local/bin}"

github_curl() {
    token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
    if [ -n "${token}" ]; then
        curl -fsSL \
            -H "Authorization: Bearer ${token}" \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: maki-install" \
            "$@"
    else
        curl -fsSL \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: maki-install" \
            "$@"
    fi
}

main() {
    need_cmd curl

    case "$(uname -s)" in
        Linux)  os="unknown-linux-musl" ;;
        Darwin) os="apple-darwin" ;;
        *) err "unsupported OS: $(uname -s)" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64)   arch="x86_64" ;;
        aarch64|arm64)  arch="aarch64" ;;
        *) err "unsupported architecture: $(uname -m)" ;;
    esac

    target="${arch}-${os}"

    tag="${1:-$(github_curl "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | cut -d'"' -f4)}"
    [ -n "${tag}" ] || err "failed to determine latest release tag"

    url="https://github.com/${REPO}/releases/download/${tag}/${BINARY}-${tag}-${target}.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' EXIT

    echo "downloading ${BINARY} ${tag} for ${target}..."
    github_curl "${url}" | tar xz -C "${tmp}"

    if [ -w "${INSTALL_DIR}" ]; then
        mv "${tmp}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    else
        echo "installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "${tmp}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    fi

    chmod +x "${INSTALL_DIR}/${BINARY}"
    echo "${BINARY} ${tag} installed to ${INSTALL_DIR}/${BINARY}"
    echo ""
}

need_cmd() {
    command -v "$1" > /dev/null 2>&1 || err "need '$1' (not found)"
}

err() {
    echo "error: $1" >&2
    exit 1
}

main "$@"
