local NO_RESULTS_MSG = require("constants").NO_RESULTS_MSG

-- Decoded results -> model-facing text with no markup: a numbered title (and
-- date, when the backend sends one), then the url and the snippet, which
-- SearXNG calls content. Entries without a url are skipped.
local function numbered_list(items, max_results)
  local lines, n = {}, 0
  for _, item in ipairs(items) do
    if n < max_results and type(item) == "table" and type(item.url) == "string" and item.url ~= "" then
      n = n + 1
      local title = type(item.title) == "string" and item.title or item.url
      if type(item.published) == "string" then
        title = title .. " (" .. item.published .. ")"
      end
      local snippet = type(item.snippet) == "string" and item.snippet
        or type(item.content) == "string" and item.content
        or ""
      lines[n] = string.format("%d. %s\n   %s\n   %s", n, title, item.url, snippet)
    end
  end
  if n == 0 then
    return NO_RESULTS_MSG
  end
  return table.concat(lines, "\n")
end

return numbered_list
