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
# The first install writes an anchor.toml (system: /etc/maki/anchor.toml,
# user: ~/.config/maki-anchor/anchor.toml) that holds the bind address, the
# local-login switch, the mint policy and the OIDC block. It is never
# overwritten by an update; edit it and restart the service.
#
# Env:
#   MAKI_INSTALL_REPO     release source, default wmantly/maki
#   MAKI_ANCHOR_VERSION   release tag, default: the latest release
#   MAKI_ANCHOR_BIND      bind seeded into a fresh anchor.toml, default 127.0.0.1:8688
#   MAKI_ANCHOR_ALLOW_LOCAL  local password login in a fresh anchor.toml, default true
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
ExecStart=${BIN} serve --config ${CONFIG}
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

write_config() {
    cat > "$CONFIG" <<EOF
# maki anchor configuration. Edit and restart the service to apply.
# Docs: https://maki.sh/docs/anchor/

# Listen address. Keep 127.0.0.1 behind a local reverse proxy that
# terminates TLS; use 0.0.0.0 only if this box serves traffic directly.
bind = "${BIND}"

[auth]
# Username/password accounts (created via the first-run setup page or
# \`maki-anchor users add\`). Safe to leave on; OIDC can sit beside it.
allow_local_users = ${allow_local}
# Who may create instance tokens from the dashboard: any | user | admin.
# mint_tokens = "admin"

# Single sign-on with any standard OIDC provider (Authelia, Authentik,
# Keycloak, Pocket ID, Google). The callback URL is {origin}/callback.
# [oidc]
# issuer = "https://auth.example.com/realms/main"
# client_id = "maki-anchor"
# client_secret = "..."
# origin = "https://maki.example.com"
EOF
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

    allow_local="${MAKI_ANCHOR_ALLOW_LOCAL:-true}"
    if [ "$(id -u)" = "0" ]; then
        mode=system
        BIN=/usr/local/bin/maki-anchor
        STATE=/var/lib/maki-anchor
        UNIT=/etc/systemd/system/${SERVICE}.service
        CONFIG=/etc/maki/anchor.toml
        WANTED_BY=multi-user.target
        mkdir -p "$STATE" /etc/maki
    else
        mode=user
        BIN="$HOME/.local/bin/maki-anchor"
        STATE="$HOME/.local/state/maki-anchor"
        UNIT="$HOME/.config/systemd/user/${SERVICE}.service"
        CONFIG="$HOME/.config/maki-anchor/anchor.toml"
        WANTED_BY=default.target
        mkdir -p "$HOME/.local/bin" "$STATE" "$HOME/.config/systemd/user" "$HOME/.config/maki-anchor"
    fi

    if [ -f "$CONFIG" ]; then
        say "keeping existing config: $CONFIG"
    else
        write_config
        say "wrote config: $CONFIG"
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
    say "anchor is up (config in ${CONFIG}, data in ${STATE})"
    say "register an instance:   ${BIN} tokens add <name>"
    say "update later: re-run the same curl line"
}

main "$@"
