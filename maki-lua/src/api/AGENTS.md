Mirror Neovim's Lua API namespaces (maki.uv = vim.uv, maki.fs = vim.fs, maki.treesitter = vim.treesitter).
Keep function signatures identical so plugins can be copy-pasted between Neovim and maki.
Only exception is the UI API, neovim's has baggage.

## Design

Our goal is to let plugin authors have as much freedom as possible, that's why desiging the APIs should be looked at as simple primitives you combine together.

## Error convention

Fallible runtime operations return the pair (value, err) and never throw.
Throwing is reserved for programmer errors, like passing a number where a string belongs.
`util/pair.rs` is the single home for that shape: use `Pair<T>`, `err_pair`, `pair`, and `try_pair!` instead of writing a new helper.
A call with nothing to return still answers `(true, nil)` on success, so `if not ok` always means failure.

Tool handlers fail with `{ llm_output = msg, is_error = true }`; a plain string is always success (only `is_error` flags the result as an error to the provider).
