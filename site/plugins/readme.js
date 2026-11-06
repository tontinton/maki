// README handling shared by the plugins page and the build's snapshot step (plugins-snapshot.mjs),
// so the build and a live-only plugin pick the same preview. Only renderReadme touches the DOM.
import { Marked, lexer, walkTokens } from './marked.esm.js';

const RAW_BASE = 'https://raw.githubusercontent.com/';
const REPO_BASE = 'https://github.com/';
const ANCHOR_PREFIX = 'user-content-';
const DARK_ONLY = '#gh-dark-mode-only';
const LIGHT_ONLY = '#gh-light-mode-only';
const MIN_DECLARED_PX = 48;
const WEB_SCHEME = /^https?:$/;
const LINK_SCHEME = /^(?:https?|mailto):$/;
const GITHUB_FILE = /^\/([^/]+\/[^/]+)\/(?:blob|raw)\/(.+)$/;
// GitHub turns a bare upload link on its own line into a player, so such a link is a video
const GITHUB_UPLOAD = /^https:\/\/(?:github\.com\/(?:user-attachments\/assets|[^/]+\/[^/]+\/assets\/\d+)\/[\w-]+$|user-images\.githubusercontent\.com\/)/;
const VIDEO_FILE = /\.(?:mp4|m4v|mov|webm|ogv)(?:[?#]|$)/i;
const BADGE = /shields\.io|badgen\.net|badge|travis-ci\.|codecov\.io|coveralls\.io|circleci\.com|star-history\.com|buymeacoffee|ko-fi\.com|liberapay|deepwiki|hits\.|visitor/i;
const MEDIA_TAG = /<(\/?)(img|source|video|picture)\b([^>]*)>/gi;
const ATTR = /([\w-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/g;
const HTML_COMMENT = /<!--[\s\S]*?-->/g;
const SCHEME_MEDIA = /prefers-color-scheme:\s*(dark|light)/;
const PIXELS = /^\d+(?:\.\d+)?(?:px)?$/;
const LENGTH = /^\d+(?:\.\d+)?(?:px|%)?$/;
const CODE_LANGUAGE = /^language-[\w+#.-]+$/;
const MARKDOWN_FILE = /\.(?:md|markdown|mdown|mkdn)$|^[^.]*$/i;
const ALERT = /^\s*\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\][ \t]*\n?/i;

const DROP = new Set(['script', 'style', 'iframe', 'frame', 'frameset', 'object', 'embed', 'form', 'textarea', 'select',
  'button', 'link', 'meta', 'base', 'svg', 'math', 'template', 'noscript', 'title', 'audio', 'canvas', 'dialog']);
const KEEP = new Set(['a', 'abbr', 'b', 'bdo', 'blockquote', 'br', 'caption', 'cite', 'code', 'dd', 'del', 'details', 'dfn',
  'div', 'dl', 'dt', 'em', 'figcaption', 'figure', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'hr', 'i', 'img', 'input', 'ins',
  'kbd', 'li', 'mark', 'ol', 'p', 'picture', 'pre', 'q', 'rp', 'rt', 'ruby', 's', 'samp', 'small', 'source', 'span',
  'strike', 'strong', 'sub', 'summary', 'sup', 'table', 'tbody', 'td', 'tfoot', 'th', 'thead', 'time', 'tr', 'tt', 'u',
  'ul', 'var', 'video', 'wbr']);
const PLAIN_ATTRS = new Set(['alt', 'title', 'align', 'valign', 'colspan', 'rowspan', 'span', 'scope', 'start', 'reversed',
  'type', 'open', 'lang', 'dir', 'datetime', 'checked', 'disabled', 'media', 'controls', 'loop', 'muted', 'cite', 'abbr']);

const LUA_KEYWORDS = 'and|break|do|else|elseif|end|for|function|goto|if|in|local|not|or|repeat|return|then|until|while';
const SH_KEYWORDS = 'if|then|else|elif|fi|for|while|until|do|done|case|esac|function|in|export|local|return|sudo';
const STRING = `"(?:\\\\.|[^"\\\\\\n])*"|'(?:\\\\.|[^'\\\\\\n])*'`;
const NUMBER = '\\b\\d+(?:\\.\\d+)?\\b';
const GRAMMARS = {
  lua: [['cm', '--\\[\\[[\\s\\S]*?\\]\\]|--.*'], ['st', `${STRING}|\\[\\[[\\s\\S]*?\\]\\]`],
    ['con', `\\b(?:true|false|nil)\\b|${NUMBER}`], ['kw', `\\b(?:${LUA_KEYWORDS})\\b`], ['fn', '\\b[A-Za-z_]\\w*(?=\\s*[({"\'])']],
  sh: [['cm', '(?<=^|\\s)#.*'], ['st', STRING], ['con', '\\$\\{?\\w+\\}?'], ['kw', `\\b(?:${SH_KEYWORDS})\\b`],
    ['fn', '(?<=(?:^|[|;&]|^\\$ )\\s*)[A-Za-z_.][\\w./-]*'], ['cm', '^\\$(?= )']],
  json: [['ty', '"(?:\\\\.|[^"\\\\])*"(?=\\s*:)'], ['st', STRING], ['con', `\\b(?:true|false|null)\\b|-?${NUMBER}`]],
  toml: [['cm', '#.*'], ['kw', '^\\s*\\[[^\\]\\n]*\\]'], ['ty', '^\\s*[\\w.-]+(?=\\s*=)'], ['st', STRING],
    ['con', `\\b(?:true|false)\\b|${NUMBER}`]],
  yaml: [['cm', '(?<=^|\\s)#.*'], ['ty', '^\\s*(?:- )?[\\w.-]+(?=:(?:\\s|$))'], ['st', STRING],
    ['con', `\\b(?:true|false|null)\\b|${NUMBER}`]],
  diff: [['st', '^\\+.*'], ['con', '^-.*'], ['cm', '^@@.*']],
};
const LANGUAGE_ALIASES = { luau: 'lua', bash: 'sh', shell: 'sh', zsh: 'sh', console: 'sh', 'shell-session': 'sh',
  jsonc: 'json', json5: 'json', ini: 'toml', yml: 'yaml', patch: 'diff' };
const HIGHLIGHTERS = Object.fromEntries(Object.entries(GRAMMARS).map(([lang, rules]) =>
  [lang, { re: new RegExp(rules.map(([, re]) => `(${re})`).join('|'), 'gm'), classes: rules.map(([cls]) => cls) }]));
const HTML_ESCAPES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' };

const markdown = new Marked({ gfm: true });

export const bases = (full, path) => ({
  raw: `${RAW_BASE}${full}/HEAD/`,
  blob: `${REPO_BASE}${full}/blob/HEAD/`,
  dir: path.slice(0, path.lastIndexOf('/') + 1),
});

function resolve(url, root, dir) {
  const fromRoot = url.startsWith('/') && !url.startsWith('//');
  try { return new URL(fromRoot ? url.slice(1) : url, fromRoot ? root : root + dir); } catch { return null; }
}

function mediaUrl(src, b) {
  const url = resolve(src.trim(), b.raw, b.dir);
  if (!url || !WEB_SCHEME.test(url.protocol)) return null;
  const file = url.hostname === 'github.com' && url.pathname.match(GITHUB_FILE);
  return file ? `${RAW_BASE}${file[1]}/${file[2]}${url.hash}` : url.href;
}

function linkUrl(href, b) {
  if (href.startsWith('#')) return `#${ANCHOR_PREFIX}${href.slice(1)}`;
  const url = resolve(href.trim(), b.blob, b.dir);
  return url && LINK_SCHEME.test(url.protocol) ? url.href : null;
}

function attrsOf(source) {
  const attrs = {};
  for (const [, name, ...values] of source.matchAll(ATTR)) attrs[name.toLowerCase()] = values.find(v => v !== undefined);
  return attrs;
}

const firstSrc = srcset => srcset?.trim().split(/\s+/)[0];
const declaredPx = value => PIXELS.test(value ?? '') ? parseFloat(value) : undefined;

// Every image and video in document order, as GitHub would show them. Theme pairs (<picture> sources or
// #gh-*-mode-only twins) collapse into one entry with a dark variant.
export function findMedia(text, b) {
  const found = [];
  const add = (src, kind, attrs = {}, dark) => {
    const url = src && mediaUrl(src, b);
    if (!url) return;
    found.push({ src: url, dark: dark && mediaUrl(dark, b) || undefined, kind: VIDEO_FILE.test(url) ? 'video' : kind,
      w: declaredPx(attrs.width), h: declaredPx(attrs.height) });
  };
  let themed = {};
  let videoSource = null;
  const scanHtml = html => {
    for (const [, closing, tag, rawAttrs] of html.replace(HTML_COMMENT, '').matchAll(MEDIA_TAG)) {
      const name = tag.toLowerCase();
      const attrs = attrsOf(rawAttrs);
      if (name === 'picture') themed = {};
      else if (name === 'video' && closing) { add(videoSource, 'video'); videoSource = null; }
      else if (name === 'video') attrs.src ? add(attrs.src, 'video', attrs) : videoSource = '';
      else if (name === 'source' && videoSource === '') videoSource = attrs.src;
      else if (name === 'source') {
        const scheme = attrs.media?.match(SCHEME_MEDIA)?.[1];
        if (scheme) themed[scheme] = firstSrc(attrs.srcset);
      } else if (name === 'img') {
        add(themed.light ?? attrs.src, 'image', attrs, themed.dark);
        themed = {};
      }
    }
  };
  walkTokens(lexer(text, { gfm: true }), token => {
    if (token.type === 'image') add(token.href, 'image');
    else if (token.type === 'html') scanHtml(token.text);
    else if (token.type === 'paragraph') {
      const [only, ...rest] = token.tokens.filter(t => t.type !== 'text' || t.text.trim());
      if (!rest.length && only?.type === 'link' && only.text === only.href && GITHUB_UPLOAD.test(only.href)) add(only.href, 'video');
    }
  });
  return pairThemes(found);
}

function pairThemes(found) {
  const paired = [];
  for (const media of found) {
    const previous = paired.at(-1);
    const mode = media.src.endsWith(DARK_ONLY) ? 'dark' : media.src.endsWith(LIGHT_ONLY) ? 'light' : null;
    const previousMode = previous?.mode;
    if (mode && previousMode && mode !== previousMode && !previous.dark) {
      const [light, dark] = mode === 'dark' ? [previous.src, media.src] : [media.src, previous.src];
      Object.assign(previous, { src: light, dark, mode: null });
    } else paired.push({ ...media, mode });
  }
  return paired.map(({ mode, ...media }) => media);
}

export const isDecoration = media => BADGE.test(media.src)
  || media.w < MIN_DECLARED_PX || media.h < MIN_DECLARED_PX;

export const previewCandidates = (text, b) => findMedia(text, b).filter(media => !isDecoration(media));

const escapeHtml = text => text.replace(/[&<>"]/g, c => HTML_ESCAPES[c]);

export function highlight(code, language) {
  const grammar = HIGHLIGHTERS[LANGUAGE_ALIASES[language] ?? language];
  if (!grammar) return null;
  let html = '';
  let last = 0;
  for (const match of code.matchAll(grammar.re)) {
    if (!match[0]) continue;
    const group = match.findIndex((g, i) => i > 0 && g !== undefined);
    html += `${escapeHtml(code.slice(last, match.index))}<span class="${grammar.classes[group - 1]}">${escapeHtml(match[0])}</span>`;
    last = match.index + match[0].length;
  }
  return html + escapeHtml(code.slice(last));
}

function cleanAttr(tag, name, value, b) {
  switch (name) {
    case 'href': return tag === 'a' ? linkUrl(value, b) : null;
    case 'src': case 'poster': return mediaUrl(value, b);
    case 'srcset': {
      const sources = value.split(',').map(entry => {
        const [url, ...descriptor] = entry.trim().split(/\s+/);
        const resolved = url && mediaUrl(url, b);
        return resolved && [resolved, ...descriptor].join(' ');
      });
      return sources.every(Boolean) ? sources.join(', ') : null;
    }
    case 'width': case 'height': return LENGTH.test(value) ? value : null;
    case 'id': case 'name': return ANCHOR_PREFIX + value;
    case 'class': return tag === 'code' && CODE_LANGUAGE.test(value) ? value : null;
    default: return PLAIN_ATTRS.has(name) ? value : null;
  }
}

// Mirrors GitHub's allowlist: unknown tags unwrap to their content, no style, no class, no scripting.
function sanitize(node, b) {
  for (const child of [...node.childNodes]) {
    if (child.nodeType === Node.COMMENT_NODE) { child.remove(); continue; }
    if (child.nodeType !== Node.ELEMENT_NODE) continue;
    const tag = child.localName;
    if (DROP.has(tag) || (tag === 'input' && child.getAttribute('type') !== 'checkbox')) { child.remove(); continue; }
    sanitize(child, b);
    if (!KEEP.has(tag)) { child.replaceWith(...child.childNodes); continue; }
    for (const { name, value } of [...child.attributes]) {
      const kept = cleanAttr(tag, name, value, b);
      if (kept == null) child.removeAttribute(name);
      else if (kept !== value) child.setAttribute(name, kept);
    }
    if (tag === 'input') child.setAttribute('disabled', '');
  }
}

function sizeMedia(node, size) {
  if (!size) return;
  const [w, h] = size;
  const width = declaredPx(node.getAttribute('width'));
  const height = declaredPx(node.getAttribute('height'));
  if (node.hasAttribute('width') && !width || node.hasAttribute('height') && !height) return;
  if (!width && !height) { node.setAttribute('width', w); node.setAttribute('height', h); }
  else if (!height) node.setAttribute('height', Math.round(width * h / w));
  else if (!width) node.setAttribute('width', Math.round(height * w / h));
}

function untheme(picture) {
  const img = picture.querySelector('img');
  const variants = {};
  for (const source of picture.querySelectorAll('source')) {
    const scheme = source.getAttribute('media')?.match(SCHEME_MEDIA)?.[1];
    if (scheme) variants[scheme] = firstSrc(source.getAttribute('srcset'));
  }
  if (!img || !variants.dark && !variants.light) return;
  variants.light ??= img.getAttribute('src');
  picture.replaceWith(...Object.entries(variants).filter(([, src]) => src).map(([scheme, src]) => {
    const variant = img.cloneNode();
    variant.setAttribute('src', src);
    variant.classList.add(`only-${scheme}`);
    return variant;
  }));
}

function slugger() {
  const seen = new Map();
  return text => {
    const slug = text.trim().toLowerCase().replace(/[^\p{L}\p{N}\p{M}\s_-]/gu, '').replace(/\s/g, '-');
    const count = seen.get(slug) ?? 0;
    seen.set(slug, count + 1);
    return count ? `${slug}-${count}` : slug;
  };
}

// An upload link GitHub would play can also be an image; swap it in when the player gives up.
function fallBackToImage(video) {
  video.addEventListener('error', () => {
    const img = video.ownerDocument.createElement('img');
    Object.assign(img, { src: video.src, alt: '', decoding: 'async', referrerPolicy: 'no-referrer' });
    video.replaceWith(img);
  }, { once: true });
}

export function renderReadme(text, { full, path, sizes = {} }) {
  const b = bases(full, path);
  const template = document.createElement('template');
  template.innerHTML = MARKDOWN_FILE.test(path) ? markdown.parse(text) : `<pre>${escapeHtml(text)}</pre>`;
  const root = template.content;
  const doc = template.content.ownerDocument;
  sanitize(root, b);

  for (const picture of root.querySelectorAll('picture')) untheme(picture);
  for (const img of root.querySelectorAll('img')) {
    const src = img.getAttribute('src');
    if (!src) { img.remove(); continue; }
    if (src.endsWith(DARK_ONLY)) img.classList.add('only-dark');
    if (src.endsWith(LIGHT_ONLY)) img.classList.add('only-light');
    Object.assign(img, { loading: 'lazy', decoding: 'async', referrerPolicy: 'no-referrer' });
    sizeMedia(img, sizes[src]);
  }
  for (const paragraph of root.querySelectorAll('p')) {
    const [only, ...rest] = [...paragraph.childNodes].filter(n => n.nodeType !== Node.TEXT_NODE || n.data.trim());
    const href = only?.localName === 'a' && only.getAttribute('href');
    if (rest.length || !href || only.textContent.trim() !== href || !(GITHUB_UPLOAD.test(href) || VIDEO_FILE.test(href))) continue;
    const video = doc.createElement('video');
    video.src = href;
    if (GITHUB_UPLOAD.test(href)) fallBackToImage(video);
    paragraph.replaceWith(video);
  }
  for (const video of root.querySelectorAll('video')) {
    Object.assign(video, { controls: true, playsInline: true, preload: 'metadata' });
    sizeMedia(video, sizes[video.getAttribute('src')]);
  }
  for (const link of root.querySelectorAll('a[href]')) {
    if (link.getAttribute('href').startsWith('#')) continue;
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
  }
  const slug = slugger();
  for (const heading of root.querySelectorAll('h1, h2, h3, h4, h5, h6')) {
    if (!heading.id) heading.id = ANCHOR_PREFIX + slug(heading.textContent);
  }
  for (const quote of root.querySelectorAll('blockquote')) {
    const first = quote.firstElementChild;
    const lead = first?.localName === 'p' && first.firstChild;
    const marker = lead?.nodeType === Node.TEXT_NODE && lead.data.match(ALERT);
    if (!marker) continue;
    lead.data = lead.data.slice(marker[0].length);
    if (first.firstChild?.localName === 'br') first.firstChild.remove();
    if (!first.textContent.trim() && !first.children.length) first.remove();
    const kind = marker[1].toLowerCase();
    const title = doc.createElement('p');
    title.className = 'alert-title';
    title.textContent = kind[0].toUpperCase() + kind.slice(1);
    quote.className = `alert alert-${kind}`;
    quote.prepend(title);
  }
  for (const table of root.querySelectorAll('table')) {
    const scroller = doc.createElement('div');
    scroller.className = 'table-wrap';
    table.replaceWith(scroller);
    scroller.append(table);
  }
  for (const pre of root.querySelectorAll('pre')) {
    const code = pre.querySelector(':scope > code');
    const language = code?.className.match(CODE_LANGUAGE) && code.className.slice('language-'.length).toLowerCase();
    const highlighted = language && highlight(code.textContent, language);
    if (highlighted) code.innerHTML = highlighted;
    const frame = doc.createElement('div');
    frame.className = 'code';
    const copy = doc.createElement('button');
    copy.className = 'code-copy';
    copy.textContent = 'copy';
    pre.replaceWith(frame);
    frame.append(pre, copy);
  }
  return root;
}
