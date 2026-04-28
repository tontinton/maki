Read a file or directory. Returns contents with line numbers (1-indexed).

- Supports absolute, relative, and ~/ paths.
- Use the index tool first to locate relevant line ranges.
- **Always include offset and limit**. Defaults: no offset = start at 1; no limit = up to 2000 lines.
- Use truncation hints (e.g. "truncated lines X-Y") to continue with the correct offset.
- For files >500 lines, always **read** with offset/limit (only what you need).
- Do not reread the same range (same file and same offset).
- Prefer grep to locate content instead of scanning full files.
- Call in parallel when reading multiple files.
- Avoid tiny repeated slices - read a larger window if you need more context.
