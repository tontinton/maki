Return a compact overview of a source file: imports, type definitions, function signatures, and structure with their line numbers surrounded by []. ~70-90% fewer tokens than reading the full file.

- Use this FIRST to understand file structure before using read with offset/limit.
- Falls back with an error on unsupported languages. Use read instead.
