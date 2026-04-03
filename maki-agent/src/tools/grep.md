Search file contents using regex.

- Respects .gitignore.
- Results grouped by file, sorted by modification time.
- Prefer speculative parallel searches over sequential rounds of glob+grep.
- Do NOT wrap the pattern in quotes - the value is passed directly to the regex engine.
- Multi-line matching is auto-enabled when the pattern contains `\\n`, `(?s)`, or `(?m)`.
- To find all references to a symbol, prefer find_symbol over grep.
