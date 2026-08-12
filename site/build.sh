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
cp doom.mp4 "$OUT/"
cp doom-av1.mp4 "$OUT/"
cp ../install.sh "$OUT/"
cp ../install.ps1 "$OUT/"
cp favicon.ico "$OUT/"
cp favicon-16x16.png "$OUT/"
cp favicon-32x32.png "$OUT/"
cp apple-touch-icon.png "$OUT/"
cp android-chrome-192x192.png "$OUT/"
cp android-chrome-512x512.png "$OUT/"
cp site.webmanifest "$OUT/"

# 2. Build Zola docs
cd docs
zola build -o "../_build/docs"
cd ..

# 3. Markdown mirrors + llms.txt / llms-full.txt for LLM consumption
BASE_URL="https://maki.sh"

body() {
  awk '/^\+\+\+$/{c++; next} c>=2' "$1"
}

first_paragraph() {
  body "$1" | awk '
    /^```/ { fence = !fence; next }
    fence || /^#/ { next }
    /^$/ { if (p) exit; next }
    { printf "%s%s", (p ? " " : ""), $0; p = 1 }
    END { print "" }
  '
}

pages=$(for f in docs/content/*/_index.md; do
  w=$(sed -n 's/^weight = \([0-9]*\)$/\1/p' "$f")
  echo "${w:-999} $f"
done | sort -n | cut -d' ' -f2-)

body docs/content/_index.md > "$OUT/docs/index.md"

summary=$(first_paragraph docs/content/_index.md)

{
  echo "# Maki"
  echo
  echo "> $summary"
  echo
  echo "Full documentation in one file: $BASE_URL/llms-full.txt"
  echo
  echo "## Docs"
  echo
  echo "- [Maki Docs]($BASE_URL/docs/index.md): overview and map of the documentation"
  for f in $pages; do
    slug=$(basename "$(dirname "$f")")
    title=$(sed -n 's/^title = "\(.*\)"$/\1/p' "$f")
    desc=$(first_paragraph "$f")
    echo "- [$title]($BASE_URL/docs/$slug/index.md): $desc"
  done
} > "$OUT/llms.txt"

{
  body docs/content/_index.md
  for f in $pages; do
    slug=$(basename "$(dirname "$f")")
    body "$f" > "$OUT/docs/$slug/index.md"
    echo
    echo "---"
    echo
    body "$f"
  done
} > "$OUT/llms-full.txt"

# 4. Search index (lazy-loaded by the docs search modal)
python3 - "$OUT/docs/search.json" docs/content <<'EOF'
import json, os, re, sys

out, root = sys.argv[1], sys.argv[2]
docs = []
for slug in os.listdir(root):
    path = os.path.join(root, slug, "_index.md")
    if not os.path.isfile(path):
        continue
    raw = open(path, encoding="utf-8").read()
    m = re.match(r"\+\+\+\n(.*?)\n\+\+\+\n", raw, re.S)
    fm, body = m.group(1), raw[m.end():]
    title = re.search(r'^title = "(.*)"$', fm, re.M).group(1)
    weight = re.search(r"^weight = (\d+)$", fm, re.M)
    body = re.sub(r"^```.*$", "", body, flags=re.M)
    body = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", body)
    body = re.sub(r"<[^>]+>", " ", body)
    body = re.sub(r"[#*`|]", " ", body)
    body = re.sub(r"-{3,}", " ", body)
    body = re.sub(r"\s+", " ", body).strip()
    docs.append((int(weight.group(1)) if weight else 999,
                 {"title": title, "href": f"/docs/{slug}/", "body": body}))
docs.sort(key=lambda d: d[0])
with open(out, "w", encoding="utf-8") as f:
    json.dump([d for _, d in docs], f, ensure_ascii=False)
EOF

cp "$OUT/llms.txt" "$OUT/llms-full.txt" "$OUT/docs/"
