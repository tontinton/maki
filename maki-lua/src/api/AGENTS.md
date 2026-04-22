Mirror Neovim's Lua API namespaces (maki.uv = vim.uv, maki.fs = vim.fs, maki.treesitter = vim.treesitter).
Keep function signatures identical so plugins can be copy-pasted between Neovim and maki.
Only exception is the UI API, neovim's has baggage.
