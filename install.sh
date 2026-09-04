#!/bin/sh
set -eu

REPO="tontinton/maki"
BINARY="maki"

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

is_windows() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) return 0 ;;
        *) return 1 ;;
    esac
}

# Works for both pretty-printed and single-line GitHub API JSON.
latest_tag() {
    github_curl "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n 1
}

default_install_dir() {
    if is_windows; then
        if [ -n "${LOCALAPPDATA:-}" ]; then
            printf '%s\n' "${LOCALAPPDATA}/maki"
        else
            printf '%s\n' "${HOME}/.local/bin"
        fi
    else
        printf '%s\n' "${HOME}/.local/bin"
    fi
}

path_has_dir() {
    dir="$1"
    case ":${PATH}:" in
        *":${dir}:"*) return 0 ;;
        *) return 1 ;;
    esac
}

warn_path() {
    dir="$1"
    if path_has_dir "${dir}"; then
        return 0
    fi
    echo "note: ${dir} is not in PATH; add it to your shell config, e.g.:"
    echo "  export PATH=\"${dir}:\$PATH\""
}

warn_shadowed() {
    dest="$1"
    resolved="$(command -v "${BINARY}" 2>/dev/null || true)"
    if [ -n "${resolved}" ] && [ "${resolved}" != "${dest}" ]; then
        echo "note: '${BINARY}' resolves to ${resolved}, which shadows ${dest};"
        echo "  remove it or reorder PATH so the new install comes first"
    fi
}

add_windows_user_path() {
    dir="$1"
    # Convert to Windows path when possible so PATH works outside Git Bash.
    if command -v cygpath > /dev/null 2>&1; then
        win_dir="$(cygpath -w "${dir}")"
    else
        win_dir="${dir}"
    fi
    powershell.exe -NoProfile -Command "
\$dir = '${win_dir}' -replace '/', '\\'
\$sep = [IO.Path]::PathSeparator
\$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (\$null -eq \$userPath) { \$userPath = '' }
\$entries = \$userPath -split [regex]::Escape(\$sep) | Where-Object { \$_ -ne '' }
\$already = \$entries | Where-Object { \$_.TrimEnd('\\') -ieq \$dir.TrimEnd('\\') }
if (\$already) { exit 0 }
\$newPath = if (\$userPath.Trim()) { \"\$userPath\$sep\$dir\" } else { \$dir }
[Environment]::SetEnvironmentVariable('Path', \$newPath, 'User')
Write-Host \"added \$dir to user PATH (restart terminal if maki is not found)\"
" || true
}

configure_anchor() {
    [ -n "${ANCHOR_URL:-}" ] || return 0
    anchor_name="${ANCHOR_NAME:-$(hostname 2>/dev/null || echo maki)}"
    config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/maki"
    init_lua="$config_dir/init.lua"
    mkdir -p "$config_dir"
    token_val="${ANCHOR_TOKEN:-YOUR_TOKEN_HERE}"
    if [ ! -f "$init_lua" ]; then
        cat > "$init_lua" <<EOF
maki.setup {
  anchor = {
    url = "$ANCHOR_URL",
    name = "$anchor_name",
    token = "$token_val",
  },
}
EOF
        echo "created $init_lua with anchor $ANCHOR_URL (name $anchor_name)"
    else
        if grep -q "anchor" "$init_lua" 2>/dev/null; then
            echo "note: $init_lua already contains anchor config; not modifying"
            echo "  set url = \"$ANCHOR_URL\", name = \"$anchor_name\", token = \"$token_val\" manually if needed"
        else
            cat >> "$init_lua" <<EOF

-- added by maki install --anchor
maki.setup {
  anchor = {
    url = "$ANCHOR_URL",
    name = "$anchor_name",
    token = "$token_val",
  },
}
EOF
            echo "appended anchor config to $init_lua (name $anchor_name)"
        fi
    fi
    if [ "$token_val" = "YOUR_TOKEN_HERE" ]; then
        echo "next: create a token on the anchor dashboard and set token in $init_lua"
    fi
}

main() {
    ANCHOR_URL=""
    ANCHOR_NAME=""
    ANCHOR_TOKEN=""
    TAG=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --anchor) ANCHOR_URL="$2"; shift 2 ;;
            --name) ANCHOR_NAME="$2"; shift 2 ;;
            --token) ANCHOR_TOKEN="$2"; shift 2 ;;
            --help|-h) echo "usage: $0 [--anchor URL] [--name NAME] [--token TOKEN] [tag]"; exit 0 ;;
            --) shift; break ;;
            -*) err "unknown option $1" ;;
            *) if [ -z "$TAG" ]; then TAG="$1"; else err "too many positional args: $1"; fi; shift ;;
        esac
    done

    need_cmd curl

    if is_windows; then
        # Only x86_64 Windows builds are published; ARM64 runs them under emulation.
        target="x86_64-pc-windows-msvc"
        archive_ext="zip"
        bin_name="${BINARY}.exe"
        need_cmd unzip
    else
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
        archive_ext="tar.gz"
        bin_name="${BINARY}"
    fi

    INSTALL_DIR="${MAKI_INSTALL_DIR:-$(default_install_dir)}"

    tag="${TAG:-$(latest_tag)}"
    [ -n "${tag}" ] || err "failed to determine latest release tag"

    if is_windows; then
        raw_url="https://github.com/${REPO}/releases/download/${tag}/${BINARY}-${tag}-${target}-signed.exe"
        archive_url="https://github.com/${REPO}/releases/download/${tag}/${BINARY}-${tag}-${target}.zip"
    else
        raw_url="https://github.com/${REPO}/releases/download/${tag}/${BINARY}-${tag}-${target}"
        archive_url="https://github.com/${REPO}/releases/download/${tag}/${BINARY}-${tag}-${target}.tar.gz"
    fi
    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' EXIT

    echo "downloading ${BINARY} ${tag} for ${target}..."
    # Try raw binary (new releases); fallback to archive for old releases
    if ! github_curl "${raw_url}" -o "${tmp}/${bin_name}" 2>/dev/null || [ ! -s "${tmp}/${bin_name}" ]; then
        rm -f "${tmp}/${bin_name}"
        echo "raw binary not found at $raw_url, trying archive $archive_url..."
        url="$archive_url"
        if [ "${archive_ext}" = "zip" ]; then
            github_curl "${url}" -o "${tmp}/maki.zip"
            unzip -qo "${tmp}/maki.zip" -d "${tmp}"
        else
            github_curl "${url}" | tar xz -C "${tmp}"
        fi
    else
        echo "downloaded raw binary $raw_url"
    fi

    [ -f "${tmp}/${bin_name}" ] || err "download failed: ${bin_name} not found in ${tmp}"

    dest="${INSTALL_DIR}/${bin_name}"

    if mkdir -p "${INSTALL_DIR}" 2>/dev/null && [ -w "${INSTALL_DIR}" ]; then
        mv "${tmp}/${bin_name}" "${dest}"
        chmod +x "${dest}"
    elif command -v sudo > /dev/null 2>&1; then
        echo "installing to ${INSTALL_DIR} (requires sudo)..."
        sudo sh -c '
            set -e
            mkdir -p "$1"
            mv "$2" "$3"
            chmod +x "$3"
        ' maki-install "${INSTALL_DIR}" "${tmp}/${bin_name}" "${dest}"
    else
        err "cannot write to ${INSTALL_DIR} (set MAKI_INSTALL_DIR to a writable directory)"
    fi

    echo "${BINARY} ${tag} installed to ${dest}"

    if is_windows; then
        add_windows_user_path "${INSTALL_DIR}"
    else
        warn_path "${INSTALL_DIR}"
        warn_shadowed "${dest}"
    fi
    echo ""
    configure_anchor
}

need_cmd() {
    command -v "$1" > /dev/null 2>&1 || err "need '$1' (not found)"
}

err() {
    echo "error: $1" >&2
    exit 1
}

main "$@"
