-- Completion sources other plugins add to the popup, next to the files `@`
-- offers.
--
-- A source is `{ trigger, name, complete }`. A plugin hands its sources over
-- by layering the `completion.sources` slot and appending them to the list
-- passing through. There is no registry on purpose. The set is always what
-- the loaded plugins say right now, so an unloaded plugin's source leaves with
-- its layer and nothing can go stale.
--
-- The chain only gathers the list. The menu then asks every source side by
-- side, since sources add rows and never rewrite each other's.

local Sources = {}

Sources.SLOT = "completion.sources"

local INVALID = "completion: ignoring a source: "
local FAILED = "completion: source failed: "
-- A source claims a place above the files with a positive score, and ties go
-- to the files.
local FILE_SCORE = 0
-- A row shows its `text`, and Enter inserts it and a space, so a source
-- cannot put words in the input that the user did not read. Text that would
-- not show whole on one row is dropped: a line break or another control
-- character, one of {HIDDEN}, or more than this many bytes.
local MAX_TEXT = 120
-- A `label` is drawn dim after the text, cut to this many characters.
local MAX_LABEL = 60
local ELLIPSIS = "…"
local LABEL_GAP = "  "
-- Code points that draw as nothing or turn the text around them: the C1
-- controls, the Unicode format characters that hide or reorder text, the
-- Hangul fillers and the combining grapheme joiner. Other combining marks
-- stay, and so does the zero width joiner, which emoji are built with.
local HIDDEN = {
  { 0x80, 0x9F },
  { 0xAD, 0xAD },
  { 0x34F, 0x34F },
  { 0x61C, 0x61C },
  { 0x115F, 0x1160 },
  { 0x180E, 0x180E },
  { 0x200B, 0x200B },
  { 0x200E, 0x200F },
  { 0x2028, 0x202E },
  { 0x2060, 0x2064 },
  { 0x2066, 0x206F },
  { 0x3164, 0x3164 },
  { 0xFEFF, 0xFEFF },
  { 0xFFA0, 0xFFA0 },
  { 0xFFF9, 0xFFFB },
  { 0xE0000, 0xE007F },
}

local collect = nil
-- The list is gathered on every keystroke in a mention, so without this one
-- bad source would log the same complaint on every key.
local warned = {}

local function warn_once(msg)
  if not warned[msg] then
    warned[msg] = true
    maki.log.warn(INVALID .. msg)
  end
end

-- Layering costs `session_read`, because each source is handed the draft the
-- user is typing as its `query`. Each `complete` runs as the plugin that wrote
-- it, with that plugin's own permissions.
function Sources.declare()
  collect = maki.api.declare_slot(Sources.SLOT, function(list)
    return list
  end, { capability = { "session_read" } })
end

-- What is wrong with {source}, or nil when it can be asked.
local function problem(source)
  if type(source) ~= "table" then
    return "a source must be a table"
  end
  if type(source.name) ~= "string" or source.name == "" then
    return "a source needs a name"
  end
  if type(source.trigger) ~= "string" or not source.trigger:match("^%p$") then
    return source.name .. ": trigger must be one punctuation character"
  end
  if type(source.complete) ~= "function" then
    return source.name .. ": complete must be a function"
  end
  return nil
end

-- A second source under a taken name is dropped. Answers are kept by name,
-- and two sources sharing one would overwrite each other's rows.
function Sources.collect()
  if not collect then
    return {}
  end
  local list = collect({})
  if type(list) ~= "table" then
    warn_once("a layer on " .. Sources.SLOT .. " returned no list")
    return {}
  end
  local out, seen = {}, {}
  for _, source in ipairs(list) do
    local why = problem(source)
    if not why and seen[source.name] then
      why = source.name .. ": name already taken"
    end
    if why then
      warn_once(why)
    else
      seen[source.name] = true
      out[#out + 1] = source
    end
  end
  return out
end

function Sources.triggers(sources)
  local out = {}
  for i, source in ipairs(sources) do
    out[i] = source.trigger
  end
  return table.concat(out)
end

function Sources.for_trigger(sources, trigger)
  local out = {}
  for _, source in ipairs(sources) do
    if source.trigger == trigger then
      out[#out + 1] = source
    end
  end
  return out
end

-- A NaN score would make the sort's order function contradict itself.
local function score_of(item)
  local score = tonumber(item.score)
  if not score or score ~= score then
    return FILE_SCORE
  end
  return score
end

-- Most text is ASCII, which none of the ranges touch.
local function hidden(code)
  if code < HIDDEN[1][1] then
    return false
  end
  for _, range in ipairs(HIDDEN) do
    if code >= range[1] and code <= range[2] then
      return true
    end
  end
  return false
end

local function has_hidden(text)
  for _, code in utf8.codes(text) do
    if hidden(code) then
      return true
    end
  end
  return false
end

-- See {MAX_TEXT}.
local function shows_whole(text: string)
  return text ~= "" and #text <= MAX_TEXT and not text:find("%c") and utf8.len(text) ~= nil and not has_hidden(text)
end

-- {label} as one dim line after the row's text: controls become spaces, the
-- characters in {HIDDEN} go, and the rest is cut to {MAX_LABEL}. Nil when
-- nothing is left to show.
local function detail_of(label)
  if type(label) ~= "string" or not utf8.len(label) then
    return nil
  end
  local out, count = {}, 0
  for _, code in utf8.codes(label) do
    if count == MAX_LABEL then
      out[#out + 1] = ELLIPSIS
      break
    end
    if code < 0x20 or code == 0x7F then
      out[#out + 1] = " "
      count = count + 1
    elseif not hidden(code) then
      out[#out + 1] = utf8.char(code)
      count = count + 1
    end
  end
  local detail = table.concat(out):match("^%s*(.-)%s*$")
  return detail ~= "" and LABEL_GAP .. detail or nil
end

-- An item without `text` has nothing to insert, so it is skipped rather than
-- drawn as a row Enter cannot use.
local function rows_of(items)
  local rows = {}
  if type(items) ~= "table" then
    return rows
  end
  for _, item in ipairs(items) do
    if type(item) == "table" and type(item.text) == "string" and shows_whole(item.text) then
      rows[#rows + 1] = {
        label = item.text,
        detail = detail_of(item.label),
        insert = item.text,
        score = score_of(item),
        highlights = {},
      }
    end
  end
  return rows
end

function Sources.file_rows(items)
  local rows = {}
  for i, item in ipairs(items) do
    rows[i] = { label = item.path, insert = item.path, score = FILE_SCORE, highlights = item.highlights }
  end
  return rows
end

-- Each source gets a task of its own, so a slow one holds up nobody. One that
-- raises or answers `nil, err` is logged and gives no rows. It is one opinion
-- among several and never a reason for the popup to stop.
function Sources.ask(source, query, ctx, on_rows)
  maki.async.run(function()
    local ok, items, err = pcall(source.complete, query, ctx)
    if not ok or err ~= nil then
      maki.log.warn(FAILED .. source.name .. ": " .. tostring(ok and err or items))
      items = nil
    end
    on_rows(rows_of(items))
  end)
end

-- Highest score first. Ties keep the order rows came in: files first, then
-- the sources in the order the slot listed them. `table.sort` is not stable,
-- hence the explicit rank.
function Sources.merge(files, answers, order, limit)
  local all = {}
  local function add(row)
    all[#all + 1] = { row = row, rank = #all + 1 }
  end
  for _, row in ipairs(files) do
    add(row)
  end
  for _, name in ipairs(order) do
    for _, row in ipairs(answers[name] or {}) do
      add(row)
    end
  end
  table.sort(all, function(a, b)
    if a.row.score ~= b.row.score then
      return a.row.score > b.row.score
    end
    return a.rank < b.rank
  end)
  local out = {}
  for i = 1, math.min(limit, #all) do
    out[i] = all[i].row
  end
  return out
end

return Sources
