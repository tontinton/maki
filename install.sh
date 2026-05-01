#!/bin/sh
set -eu

REPO="tontinton/maki"
BINARY="maki"
INSTALL_DIR="${MAKI_INSTALL_DIR:-/usr/local/bin}"

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

    tag="${1:-$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | cut -d'"' -f4)}"
    [ -n "${tag}" ] || err "failed to determine latest release tag"

    url="https://github.com/${REPO}/releases/download/${tag}/${BINARY}-${tag}-${target}.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' EXIT

    echo "downloading ${BINARY} ${tag} for ${target}..."
    curl -fsSL "${url}" | tar xz -C "${tmp}"

    if [ -w "${INSTALL_DIR}" ]; then
        mv "${tmp}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    else
        echo "installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "${tmp}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    fi

    chmod +x "${INSTALL_DIR}/${BINARY}"
    echo "${BINARY} ${tag} installed to ${INSTALL_DIR}/${BINARY}"
    echo ""
    print_migration_guide
}

print_migration_guide() {
    cat <<'GUIDE'
=== Migration Guide ===

A few things changed recently. Nothing will break right away, but
you will want to move your config files over when you get a chance.

--- Config files ---

config.toml is gone. Settings now live in init.lua.

Before (config.toml):
  [agent]
  bash_timeout_secs = 180

After (init.lua):
  maki.setup({
      agent = { bash_timeout_secs = 180 },
  })

Rename your files:
  ~/.config/maki/config.toml  ->  ~/.config/maki/init.lua
  .maki/config.toml           ->  .maki/init.lua

Wrap the content in maki.setup({ ... }) and switch from TOML to Lua
table syntax.

--- MCP servers ---

MCP config used to live inside config.toml under [mcp.*] sections.
It now has its own file:

  ~/.config/maki/mcp.toml   (global)
  .maki/mcp.toml            (per-project)

Move your [mcp.*] sections there. The format stays the same, just a
different file. permissions.toml is unchanged.

--- Directory layout ---

If you had a ~/.maki/ directory, it still works. Maki checks it first
as a fallback. But new installs and new files go to XDG locations:

  Config:  ~/.config/maki/    (init.lua, permissions.toml, mcp.toml)
  Data:    ~/.local/share/maki/
  Logs:    ~/.local/logs/maki/
  State:   ~/.local/state/maki/

You can keep using ~/.maki/ forever, or move your files to the XDG
paths whenever you feel like it. Maki checks both.

Full docs: https://maki.sh/docs/configuration/
GUIDE
}

need_cmd() {
    command -v "$1" > /dev/null 2>&1 || err "need '$1' (not found)"
}

err() {
    echo "error: $1" >&2
    exit 1
}

main "$@"