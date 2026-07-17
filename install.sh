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
        printf '%s\n' "/usr/local/bin"
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

main() {
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

    tag="${1:-$(latest_tag)}"
    [ -n "${tag}" ] || err "failed to determine latest release tag"

    url="https://github.com/${REPO}/releases/download/${tag}/${BINARY}-${tag}-${target}.${archive_ext}"
    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' EXIT

    echo "downloading ${BINARY} ${tag} for ${target}..."
    if [ "${archive_ext}" = "zip" ]; then
        github_curl "${url}" -o "${tmp}/maki.zip"
        unzip -qo "${tmp}/maki.zip" -d "${tmp}"
    else
        github_curl "${url}" | tar xz -C "${tmp}"
    fi

    [ -f "${tmp}/${bin_name}" ] || err "archive did not contain ${bin_name}"

    dest="${INSTALL_DIR}/${bin_name}"

    if mkdir -p "${INSTALL_DIR}" 2>/dev/null && [ -w "${INSTALL_DIR}" ]; then
        mv "${tmp}/${bin_name}" "${dest}"
        chmod +x "${dest}"
    else
        echo "installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mkdir -p "${INSTALL_DIR}"
        sudo mv "${tmp}/${bin_name}" "${dest}"
        sudo chmod +x "${dest}"
    fi

    echo "${BINARY} ${tag} installed to ${dest}"

    if is_windows; then
        add_windows_user_path "${INSTALL_DIR}"
    fi
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
