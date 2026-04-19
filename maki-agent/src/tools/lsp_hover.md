Get type information and documentation for a symbol at a given file position using the Language Server Protocol.

- Requires an LSP server configured for the file's language.
- Line and column are 1-indexed (matching what the read tool shows).
- Returns type signatures, documentation, and other hover info from the language server.
