local indexer = require("indexer")

maki.api.register_tool({
  name = "index",
  description = "Return a compact overview of a source file: imports, type definitions, function signatures, and structure with their line numbers surrounded by []. ~70-90% more efficient than reading the full file.\n\n- Use this FIRST to understand file structure before using read with offset/limit.\n- Supports source files in different programming languages and markdown.\n- Falls back with an error on unsupported languages. Use read instead.",
  schema = {
    type = "object",
    properties = {
      path = { type = "string", description = "Absolute path to the file" },
    },
    required = { "path" },
  },
  handler = function(input, ctx)
    local path = input.path
    if not path then return "error: path is required" end

    local ext = path:match("%.([^%.]+)$")
    if not ext then return "DELEGATE_NATIVE" end

    local lang = indexer.EXT_TO_LANG[ext]
    if not lang then return "DELEGATE_NATIVE" end

    local ok, source = pcall(maki.fs.read, path)
    if not ok then return "error: " .. tostring(source) end

    local result, err = indexer.index_source(source, lang)
    if not result then return "DELEGATE_NATIVE" end

    return result
  end,
})
