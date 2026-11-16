-- Path completion for the bare word at the caret, the way a shell completes
-- one: a directory at a time, relative to the session's working tree unless
-- the word starts at `/` or `~/`, with the quotes and backslashes a shell
-- reads.
--
-- Unlike a mention it has no trigger character, since any word may be a path,
-- so it opens on a key instead. See `completion.path` in `init.lua`.
--
-- Offsets count bytes, like everything `maki.ui.input_edit` is handed.

local Trigger = require("trigger")

local Path = {}

-- Not a character: what tells a path view from a mention's in the menu.
Path.TRIGGER = "path"

local SEPARATOR = "/"
local HOME = "~/"
local HIDDEN = "."
local SPACE = " "
-- Words other completers own: mentions, and whatever opens with `$` or `!`.
local OWNED = "^[@$!]"
local SHELL_SPECIALS = "([%s\\'\"`$;&|<>()%[%]{}*?!#])"
-- What a backslash escapes inside double quotes. Before anything else it is a
-- literal backslash, as in a shell.
local DQUOTE_ESCAPES = '([\\"$`])'

-- The word ending at {text}'s end, unquoted and unescaped, the byte offset it
-- starts at, and the quote still open in it.
function Path.token_before(text)
  local start, quote, escaped = 1, nil, false
  local value = {}
  for i = 1, #text do
    local char = text:sub(i, i)
    if escaped then
      value[#value + 1] = char
      escaped = false
    elseif char == "\\" and not quote then
      escaped = true
    elseif char == "\\" and quote == '"' and text:sub(i + 1, i + 1):match(DQUOTE_ESCAPES) then
      escaped = true
    elseif quote then
      if char == quote then
        quote = nil
      else
        value[#value + 1] = char
      end
    elseif char == "'" or char == '"' then
      quote = char
    elseif char:match("%s") then
      start = i + 1
      value = {}
    else
      value[#value + 1] = char
    end
  end
  return start - 1, table.concat(value), quote
end

-- {path} written so `token_before` reads it back, inside {quote} when one is
-- open.
function Path.encode(path, quote)
  if quote == "'" then
    return "'" .. path:gsub("'", "'\\''")
  elseif quote == '"' then
    return '"' .. path:gsub(DQUOTE_ESCAPES, "\\%1")
  end
  return (path:gsub(SHELL_SPECIALS, "\\%1"))
end

-- The word at {st}'s caret: where it starts, what it says and the quote open
-- in it. Nil when there is none to complete: a selection, a caret inside a
-- word, an empty word, one another completer owns, or a command line.
function Path.word(st)
  if not st or st.anchor or Trigger.in_command(st.text) then
    return nil
  end
  if st.text:sub(st.cursor + 1, st.cursor + 1):match("%S") then
    return nil
  end
  local start, token, quote = Path.token_before(st.text:sub(1, st.cursor))
  if token == "" or token:match(OWNED) then
    return nil
  end
  return start, token, quote
end

-- The directory {directory} names, `""` being {root} itself.
local function resolve(directory: string, root: string): string?
  if directory == "" then
    return root
  elseif directory:sub(1, #HOME) == HOME then
    local home = maki.uv.os_homedir()
    return home and maki.fs.joinpath(home, directory:sub(#HOME + 1))
  elseif directory:sub(1, 1) == SEPARATOR then
    return directory
  end
  return maki.fs.joinpath(root, directory)
end

-- The names in {lookup}, sorted so equal scores keep a stable order, with a
-- directory ending in a separator. Unreadable reads as empty: the popup then
-- says `no matches`, which is what there is to pick from.
local function list(lookup)
  local names = {}
  for _, entry in ipairs(lookup and maki.fs.dir(lookup) or {}) do
    names[#names + 1] = entry[1] .. (entry[2] == "directory" and SEPARATOR or "")
  end
  table.sort(names)
  return { lookup = lookup, names = names }
end

-- Rows for the entries of the directory {token} names, fuzzy-ranked on its
-- last segment. Dotfiles show only once that segment starts with a dot.
--
-- {listing} is what the last call read. It is reused while {token} stays in
-- the same directory, so narrowing does not read the directory on every key.
-- Returns the rows and the listing they came from.
function Path.rows(token, quote, root, listing)
  local last_separator = token:find(SEPARATOR .. "[^" .. SEPARATOR .. "]*$")
  local directory = last_separator and token:sub(1, last_separator) or ""
  local needle = token:sub((last_separator or 0) + 1)
  local lookup = resolve(directory, root)
  if not listing or listing.lookup ~= lookup then
    listing = list(lookup)
  end
  local lowered = needle:lower()
  local shows_hidden = needle:sub(1, #HIDDEN) == HIDDEN
  local rows = {}
  for _, name in ipairs(listing.names) do
    if shows_hidden or name:sub(1, #HIDDEN) ~= HIDDEN then
      local score, highlights = maki.text.fuzzy(needle, name, { paths = true })
      if score then
        -- A file ends the word, closing any quote left open. A directory
        -- leaves the caret, and the quote, inside it to keep going.
        local is_directory = name:sub(-#SEPARATOR) == SEPARATOR
        rows[#rows + 1] = {
          label = name,
          insert = Path.encode(directory .. name, quote),
          tail = is_directory and "" or (quote or "") .. SPACE,
          directory = is_directory,
          prefix = name:sub(1, #needle):lower() == lowered,
          score = score,
          highlights = highlights or {},
        }
      end
    end
  end
  return rows, listing
end

return Path
