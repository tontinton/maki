Search for symbols across the entire workspace by name using the Language Server Protocol.

- Requires an LSP server configured for the file's language.
- Returns matching symbols with their file paths, line numbers, and kinds.
- The path parameter is used to determine which LSP server to query.
- Useful for finding a type or function when you don't know which file it's in.
