Search file contents using regex.

- Respects .gitignore.
- Results grouped by file, sorted by modification time.
- Prefer speculative parallel searches over sequential rounds of glob+grep.
- Do NOT wrap the pattern in quotes. Do NOT double-escape (e.g. `\[` not `\\[`).
- Multi-line matching is auto-enabled when the pattern contains `\n`, `(?s)`, or `(?m)`.
