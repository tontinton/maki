// Highlights the built docs' code blocks with the plugin reader's own highlighter, so a
// snippet is coloured the same on /docs/ as in a plugin README, and the docs ship no JS for it.
// Usage: node docs-highlight.mjs <built docs dir>
import { readdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { highlight } from './plugins/readme.js';

const CODE_BLOCK = /<pre><code data-lang="([\w+-]+)">([\s\S]*?)<\/code><\/pre>/g;
const ENTITY = /&(?:amp|lt|gt|quot|#(\d+));/g;
const NAMED = { '&amp;': '&', '&lt;': '<', '&gt;': '>', '&quot;': '"' };

const unescapeHtml = html => html.replace(ENTITY, (entity, code) => code ? String.fromCodePoint(code) : NAMED[entity]);

const root = process.argv[2];
const pages = (await readdir(root, { recursive: true })).filter(path => path.endsWith('.html'));
let blocks = 0;
await Promise.all(pages.map(async page => {
  const path = join(root, page);
  const html = await readFile(path, 'utf8');
  const out = html.replace(CODE_BLOCK, (block, language, body) => {
    const highlighted = highlight(unescapeHtml(body), language);
    if (highlighted === null) return block;
    blocks++;
    return `<pre><code data-lang="${language}">${highlighted}</code></pre>`;
  });
  if (out !== html) await writeFile(path, out);
}));
console.log(`highlighted ${blocks} code blocks`);
