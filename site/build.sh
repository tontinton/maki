#!/bin/sh
set -e

# Cloudflare Pages build script
# Assembles the static landing page + Zola docs into a single output dir.

ZOLA_VERSION="${ZOLA_VERSION:-0.19.2}"

if ! command -v zola >/dev/null 2>&1; then
  echo "Installing zola ${ZOLA_VERSION}..."
  mkdir -p .bin
  curl -sL "https://github.com/getzola/zola/releases/download/v${ZOLA_VERSION}/zola-v${ZOLA_VERSION}-x86_64-unknown-linux-gnu.tar.gz" | tar xz -C .bin
  export PATH="$PWD/.bin:$PATH"
fi

echo "Using $(zola --version)"

OUT="_build"
rm -rf "$OUT"
mkdir -p "$OUT"

# 1. Copy static landing page files
cp index.html "$OUT/"
cp asciinema-player.css "$OUT/"
cp asciinema-player.min.js "$OUT/"
cp demo.cast "$OUT/"

# 2. Build Zola docs
cd docs
zola build -o "../_build/docs"
