Read a file or directory. Returns contents with line numbers (1-indexed).

- Supports absolute, relative, and ~/ paths.
- Use index tool first to see file structure and find the right line numbers.
- Up to 2000 lines by default.
- Use offset/limit for large files.
- Use grep to find specific content instead of reading entire large files.
- Call in parallel when reading multiple files.
- Avoid tiny repeated slices - read a larger window if you need more context.
