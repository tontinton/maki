Jump to the definition of a symbol at a given file position using the Language Server Protocol.

- Requires an LSP server configured for the file's language.
- Line and column are 1-indexed (matching what the read tool shows).
- Returns the file path, line, and column of the definition.
