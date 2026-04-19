Find implementations of an interface, trait, or abstract method at a given file position using the Language Server Protocol.

- Requires an LSP server configured for the file's language.
- Line and column are 1-indexed (matching what the read tool shows).
- Returns a list of file:line:col locations where the symbol is implemented.
- Useful for navigating from a trait definition to its concrete implementations.
