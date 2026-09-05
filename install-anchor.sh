#!/bin/sh
# Install or update maki-anchor as a systemd service.
#
#   curl -fsSL https://raw.githubusercontent.com/wmantly/maki/main/install-anchor.sh | sh
#
# Run as root for a system unit (/usr/local/bin, /var/lib/maki-anchor), or as
# a regular user for a user unit with linger (~/.local/bin,
# ~/.local/state/maki-anchor). Re-run the same line to update: the binary is
# swapped and the service restarted.
#
# Env:
#   MAKI_INSTALL_REPO   release source, default wmantly/maki
#   MAKI_ANCHOR_VERSION release tag, default: the latest release
#   MAKI_ANCHOR_BIND    listen address, default 127.0.0.1:8688
set -eu

REPO="${MAKI_INSTALL_REPO:-wmantly/maki}"
VERSION="${MAKI_ANCHOR_VERSION:-latest}"
BIND="${MAKI_ANCHOR_BIND:-127.0.0.1:8688}"
SERVICE=maki-anchor

say() { printf '%s\n' "$*"; }
err() { printf '%s\n' "$*" >&2; exit 1; }

tag_of_latest() {
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1
}

unit_body() {
    cat <<EOF
Description=maki anchor - the remote-control hub for maki instances
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=${BIN} serve --bind ${BIND}
WorkingDirectory=${STATE}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=${WANTED_BY}
EOF
}

install_binary() {
    tmp=$(mktemp)
    trap 'rm -f "$tmp"' EXIT
    say "downloading ${URL}"
    curl -fL "$URL" -o "$tmp"
    chmod 0755 "$tmp"
    install -m 0755 "$tmp" "$BIN"
}

main() {
    [ "$(uname -s)" = "Linux" ] || err "the anchor installer needs Linux"
    command -v systemctl >/dev/null || err "systemd (systemctl) not found"

    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) err "unsupported architecture: $(uname -m)" ;;
    esac

    tag="$VERSION"
    [ "$tag" = "latest" ] && tag=$(tag_of_latest)
    [ -n "$tag" ] || err "could not resolve the latest release of ${REPO}"
    URL="https://github.com/${REPO}/releases/download/${tag}/maki-anchor-${tag}-${arch}-unknown-linux-musl"

    if [ "$(id -u)" = "0" ]; then
        mode=system
        BIN=/usr/local/bin/maki-anchor
        STATE=/var/lib/maki-anchor
        UNIT=/etc/systemd/system/${SERVICE}.service
        WANTED_BY=multi-user.target
        mkdir -p "$STATE"
    else
        mode=user
        BIN="$HOME/.local/bin/maki-anchor"
        STATE="$HOME/.local/state/maki-anchor"
        UNIT="$HOME/.config/systemd/user/${SERVICE}.service"
        WANTED_BY=default.target
        mkdir -p "$HOME/.local/bin" "$STATE" "$HOME/.config/systemd/user"
    fi

    install_binary
    say "installed ${tag} -> ${BIN}"

    { printf '[Unit]\n'; unit_body; } > "$UNIT"
    if [ "$mode" = system ]; then
        systemctl daemon-reload
        systemctl enable --now "$SERVICE"
        systemctl restart "$SERVICE"
    else
        systemctl --user daemon-reload
        systemctl --user enable --now "$SERVICE"
        systemctl --user restart "$SERVICE"
        loginctl enable-linger "$(id -un)" 2>/dev/null ||
            say "note: enable linger so the anchor survives logout: loginctl enable-linger $(id -un)"
        case ":$PATH:" in
            *":$HOME/.local/bin:"*) ;;
            *) say "note: add ~/.local/bin to your PATH for the maki-anchor CLI" ;;
        esac
    fi

    say ""
    say "anchor is up on ${BIND} (data in ${STATE})"
    say "register an instance:   ${BIN} tokens add <name>"
    say "update later: re-run the same curl line"
}

main "$@"
