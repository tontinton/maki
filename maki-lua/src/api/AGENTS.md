Mirror Neovim's Lua API namespaces (maki.uv = vim.uv, maki.fs = vim.fs, maki.treesitter = vim.treesitter).
Keep function signatures identical so plugins can be copy-pasted between Neovim and maki.
`maki.uv` mirrors `vim.uv` for sync utilities only: no libuv handles (tcp, timers) and no callbacks.
Async io goes in `maki.net` / `maki.async`, coroutine style: the call yields and answers a `(value, err)` pair.
Other exception is the UI API, neovim's has baggage.

## Design

Our goal is to let plugin authors have as much freedom as possible, that's why desiging the APIs should be looked at as simple primitives you combine together.

Long-lived work is a `maki.async.spawn` task, and a repeating timer is a `maki.async.sleep` loop inside one.

## Lua thread

All plugins share one Lua thread. Code that holds it for 5s without yielding is killed, warned at 1s.
Long loops must yield with `maki.async.sleep(0)`. Heavy work belongs in a `maki.*` Rust API that runs off-thread or yields.

## Error convention

Fallible runtime operations return the pair (value, err) and never throw.
Throwing is reserved for programmer errors, like passing a number where a string belongs.
`util/pair.rs` is the single home for that shape: use `Pair<T>`, `err_pair`, `pair`, and `try_pair!` instead of writing a new helper.
Exception: `maki.uv` handles follow luv instead, returning `0` on success and `(nil, err, name)` on failure with err-first callbacks, so `vim.uv` plugin code works unmodified.
A call with nothing to return still answers `(true, nil)` on success, so `if not ok` always means failure.

Tool handlers fail with `{ llm_output = msg, is_error = true }`; a plain string is always success (only `is_error` flags the result as an error to the provider).
