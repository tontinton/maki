-- Default PreStream hook: forwards the Rust-assembled system prompt and
-- tool definitions unchanged. Override this callback in your own plugin to
-- customize the system prompt or tool set per turn.
maki.api.create_autocmd("PreStream", {
  callback = function(payload)
    return {
      system = payload.system,
      tools = payload.tools,
    }
  end,
})
