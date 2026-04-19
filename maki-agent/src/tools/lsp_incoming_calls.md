Find all functions or methods that call the function at a given position using the Language Server Protocol.

- Requires an LSP server configured for the file's language.
- Line and column are 1-indexed (matching what the read tool shows).
- Returns a list of callers with their file paths and line numbers.
- Useful for understanding who depends on a function before refactoring.
