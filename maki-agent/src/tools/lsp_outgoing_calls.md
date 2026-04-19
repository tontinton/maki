Find all functions or methods called by the function at a given position using the Language Server Protocol.

- Requires an LSP server configured for the file's language.
- Line and column are 1-indexed (matching what the read tool shows).
- Returns a list of callees with their file paths and line numbers.
- Useful for understanding the dependencies of a function.
