// Inlines the community plugin listing into plugins/index.html, so the page paints complete
// without waiting on GitHub. Each plugin carries its README path, its cover (the first
// image or video in the README) and the pixel size of every README image, which lets the
// page reserve space for media before it loads.
// Usage: node plugins-snapshot.mjs <plugins/index.html>
import { readFile, writeFile } from 'node:fs/promises';
import { bases, findMedia, isDecoration } from './plugins/readme.js';

const SEARCH_URL = 'https://api.github.com/search/repositories?per_page=100&q=topic:maki+topic:plugin+archived:false';
const API_BASE = 'https://api.github.com/repos/';
const RAW_BASE = 'https://raw.githubusercontent.com/';
const DEFAULT_README = 'README.md';
const KEYS = ['full_name', 'description', 'stargazers_count', 'topics', 'created_at', 'pushed_at'];
const PLACEHOLDER = '<script id="snapshot" type="application/json"></script>';
const TIMEOUT_MS = 15_000;
const HEAD_BYTES = 64 * 1024;
const MAX_SKIP_BYTES = 32 * 1024 * 1024;
const MAX_MEDIA_PER_README = 24;
const CONCURRENCY = 8;
const MIN_COVER_PX = 40;
const MAX_MEDIA_PX = 16_384;
const MP4_CONTAINER = new Set(['ftyp', 'moov', 'mdat', 'free', 'skip', 'wide', 'uuid', 'pdin', 'meta', 'styp', 'sidx', 'moof']);

const [htmlPath] = process.argv.slice(2);
if (!htmlPath) throw new Error('usage: node plugins-snapshot.mjs <plugins/index.html>');

const apiHeaders = { Accept: 'application/vnd.github+json', 'User-Agent': 'maki-sh-build' };
if (process.env.GITHUB_TOKEN) apiHeaders.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;

async function request(url, headers = {}) {
  const res = await fetch(url, { headers: { 'User-Agent': 'maki-sh-build', ...headers }, signal: AbortSignal.timeout(TIMEOUT_MS) });
  if (!res.ok && res.status !== 404) throw new Error(`${url}: ${res.status}`);
  return res.ok ? res : null;
}

async function readme(full) {
  const raw = await request(`${RAW_BASE}${full}/HEAD/${DEFAULT_README}`);
  if (raw) return { path: DEFAULT_README, text: await raw.text() };
  const meta = await request(`${API_BASE}${full}/readme`, apiHeaders);
  if (!meta) return null;
  const { path, download_url } = await meta.json();
  return { path, text: await (await request(download_url)).text() };
}

// HEAD_BYTES of a file from `start`. A server that ignores Range sends the whole file, so skip
// to `start` in the stream, but only that far: a trailing moov past MAX_SKIP_BYTES stays unknown.
async function head(url, start = 0) {
  const res = await request(url, { Range: `bytes=${start}-${start + HEAD_BYTES - 1}` });
  if (!res || res.status !== 206 && start > MAX_SKIP_BYTES) return null;
  let skip = res.status === 206 ? 0 : start;
  const chunks = [];
  let size = 0;
  for await (const chunk of res.body) {
    const kept = chunk.subarray(Math.min(skip, chunk.length));
    skip -= chunk.length - kept.length;
    chunks.push(kept);
    size += kept.length;
    if (size >= HEAD_BYTES) break;
  }
  return { type: res.headers.get('content-type') ?? '', bytes: Buffer.concat(chunks) };
}

function jpegSize(b) {
  for (let i = 2; i + 9 < b.length;) {
    if (b[i] !== 0xff) { i++; continue; }
    const marker = b[i + 1];
    if (marker === 0xff) { i++; continue; }
    if (marker >= 0xd0 && marker <= 0xd9 || marker === 0x01) { i += 2; continue; }
    if (marker >= 0xc0 && marker <= 0xcf && ![0xc4, 0xc8, 0xcc].includes(marker)) {
      return [b.readUInt16BE(i + 7), b.readUInt16BE(i + 5)];
    }
    i += 2 + b.readUInt16BE(i + 2);
  }
  return null;
}

function webpSize(b) {
  const chunk = b.toString('latin1', 12, 16);
  if (chunk === 'VP8 ') return [b.readUInt16LE(26) & 0x3fff, b.readUInt16LE(28) & 0x3fff];
  if (chunk === 'VP8L') {
    return [1 + (((b[22] & 0x3f) << 8) | b[21]), 1 + (((b[24] & 0xf) << 10) | (b[23] << 2) | ((b[22] & 0xc0) >> 6))];
  }
  if (chunk === 'VP8X') return [1 + b.readUIntLE(24, 3), 1 + b.readUIntLE(27, 3)];
  return null;
}

function svgSize(b) {
  const tag = b.toString('utf8').match(/<svg\b[^>]*>/i)?.[0];
  if (!tag) return null;
  const attr = name => tag.match(new RegExp(`\\s${name}\\s*=\\s*["']([^"']*)["']`, 'i'))?.[1];
  const px = value => /^\s*[\d.]+\s*(?:px)?\s*$/.test(value ?? '') ? parseFloat(value) : null;
  const box = attr('viewBox')?.trim().split(/[\s,]+/).map(Number);
  const [w, h] = [px(attr('width')), px(attr('height'))];
  if (w && h) return [w, h];
  if (box?.length !== 4 || !(box[2] > 0 && box[3] > 0)) return null;
  return w ? [w, w * box[3] / box[2]] : h ? [h * box[2] / box[3], h] : [box[2], box[3]];
}

// tkhd stores 16.16 fixed-point width and height; the audio track's are zero
function tkhdSize(b) {
  for (let i = b.indexOf('tkhd'); i >= 0; i = b.indexOf('tkhd', i + 4)) {
    const at = i + (b[i + 4] === 1 ? 92 : 80);
    if (at + 8 > b.length) break;
    const [w, h] = [b.readUInt32BE(at) / 65536, b.readUInt32BE(at + 4) / 65536];
    if (w && h) return [Math.round(w), Math.round(h)];
  }
  return null;
}

// moov often trails a multi-megabyte mdat, so walk the top-level boxes and fetch it where it starts
async function mp4Size(url, b) {
  let offset = 0;
  for (let hops = 0; hops < 4 && b; hops++) {
    const found = tkhdSize(b);
    if (found) return found;
    let next = offset;
    for (let i = 0; i + 8 <= b.length;) {
      const type = b.toString('latin1', i + 4, i + 8);
      let size = b.readUInt32BE(i);
      if (size === 1 && i + 16 <= b.length) size = Number(b.readBigUInt64BE(i + 8));
      if (!MP4_CONTAINER.has(type) || size < 8 || type === 'moov') return null;
      next = offset + i + size;
      i += size;
    }
    if (next === offset) return null;
    offset = next;
    b = (await head(url, offset))?.bytes;
  }
  return null;
}

function readVint(b, i) {
  const length = Math.clz32(b[i]) - 23;
  if (length < 1 || length > 8 || i + length > b.length) return null;
  let value = b[i] & (0xff >> length);
  for (let k = 1; k < length; k++) value = value * 256 + b[i + k];
  return { length, value };
}

// EBML: the Video element (0xE0) holds PixelWidth (0xB0) and PixelHeight (0xBA)
function webmSize(b) {
  for (let i = b.indexOf(0xe0); i >= 0; i = b.indexOf(0xe0, i + 1)) {
    const size = readVint(b, i + 1);
    if (!size) continue;
    const end = Math.min(b.length, i + 1 + size.length + size.value);
    const dims = {};
    for (let j = i + 1 + size.length; j < end;) {
      const id = b[j];
      const field = readVint(b, j + 1);
      if (!field || field.value > 8) break;
      const at = j + 1 + field.length;
      if (id === 0xb0 || id === 0xba) dims[id] = b.readUIntBE(at, field.value);
      j = at + field.value;
    }
    if (dims[0xb0] && dims[0xba]) return [dims[0xb0], dims[0xba]];
  }
  return null;
}

async function probe(url) {
  const file = await head(url).catch(() => null);
  if (!file || file.bytes.length < 16) return null;
  const { type, bytes: b } = file;
  const magic = b.toString('latin1', 0, 12);
  let size = null;
  let video = type.startsWith('video/');
  if (magic.startsWith('\x89PNG')) size = [b.readUInt32BE(16), b.readUInt32BE(20)];
  else if (magic.startsWith('GIF8')) size = [b.readUInt16LE(6), b.readUInt16LE(8)];
  else if (b[0] === 0xff && b[1] === 0xd8) size = jpegSize(b);
  else if (magic.startsWith('RIFF') && magic.endsWith('WEBP')) size = webpSize(b);
  else if (b.readUInt32BE(0) === 0x1a45dfa3) [size, video] = [webmSize(b), true];
  else if (magic.slice(4, 8) === 'ftyp' && /avi[fs]|heic|mif1/.test(magic.slice(8))) {
    const at = b.indexOf('ispe');
    if (at >= 0) size = [b.readUInt32BE(at + 8), b.readUInt32BE(at + 12)];
  } else if (MP4_CONTAINER.has(magic.slice(4, 8))) [size, video] = [await mp4Size(url, b).catch(() => null), true];
  else if (/svg/.test(type) || /<svg/i.test(b.toString('utf8', 0, 1024))) size = svgSize(b);
  const [w, h] = size?.map(Math.round) ?? [];
  if (!(w > 0 && h > 0 && w <= MAX_MEDIA_PX && h <= MAX_MEDIA_PX)) return null;
  return { w, h, video };
}

async function pool(items, worker) {
  const results = new Array(items.length);
  let next = 0;
  const run = async () => { while (next < items.length) { const i = next++; results[i] = await worker(items[i]); } };
  await Promise.all(Array.from({ length: Math.min(CONCURRENCY, items.length) }, run));
  return results;
}

async function describe(repo) {
  const listing = {
    ...Object.fromEntries(KEYS.map(k => [k, repo[k]])),
    license: repo.license && { spdx_id: repo.license.spdx_id },
  };
  let doc;
  try {
    doc = await readme(repo.full_name);
  } catch (error) {
    // left without a `readme` key, so the page looks for the cover itself
    console.warn(`warning: ${error.message}`);
    return listing;
  }
  if (!doc) return { ...listing, readme: null };

  const media = findMedia(doc.text, bases(repo.full_name, doc.path)).slice(0, MAX_MEDIA_PER_README);
  const urls = [...new Set(media.flatMap(m => [m.src, m.dark].filter(Boolean)))];
  const probed = new Map((await pool(urls, probe)).map((size, i) => [urls[i], size]));
  const sizes = {};
  for (const [url, size] of probed) if (size) sizes[url] = [size.w, size.h];
  const cover = media.find(m => {
    const size = probed.get(m.src);
    return !isDecoration(m) && size?.w >= MIN_COVER_PX && size.h >= MIN_COVER_PX;
  });
  // undefined fields (no cover, no dark variant) drop out of the JSON
  const preview = cover && { src: cover.src, dark: cover.dark, kind: probed.get(cover.src).video ? 'video' : cover.kind };
  return { ...listing, readme: doc.path, preview, sizes };
}

const search = await request(SEARCH_URL, apiHeaders);
const { items } = await search.json();
const plugins = await pool(items, describe);
const html = await readFile(htmlPath, 'utf8');
if (!html.includes(PLACEHOLDER)) throw new Error(`${htmlPath} has no ${PLACEHOLDER}`);
const json = JSON.stringify(plugins).replace(/</g, '\\u003c');
await writeFile(htmlPath, html.replace(PLACEHOLDER, PLACEHOLDER.replace('><', `>${json}<`)));
console.log(`inlined ${plugins.length} plugins, ${plugins.filter(p => p.preview).length} with a cover`);
