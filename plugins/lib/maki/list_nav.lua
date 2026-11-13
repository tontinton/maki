-- Moves a cursor through a list, so every picker answers a key the same way.
-- It keeps no state. You give it the key, the row the cursor is on and a
-- function that moves the cursor. The list and the drawing stay yours, so a
-- picker that tracks its selection by id works as well as one that tracks a
-- row number.
--
-- Ask it last, after your own keys, so a key you bind always wins:
--
--   if key == "<CR>" then
--     open(cursor)
--   elseif not ListNav.handle_key(key, cursor, { count = #rows, page = height }, select_row) then
--     input:handle_key(key)
--   end
local ListNav = {}

local UNKNOWN_ACTION_FMT = "list_nav: %s is bound to %s, but only up, down, page_up and page_down exist"

local function is_selectable(row, opts)
  return not opts.selectable or opts.selectable(row)
end

local function step(cursor, delta, opts)
  local n = opts.count
  local i = cursor
  for _ = 1, n do
    i = i + delta
    if opts.wrap ~= false then
      i = (i - 1) % n + 1
    elseif i < 1 or i > n then
      return nil
    end
    if is_selectable(i, opts) then
      return i
    end
  end
  return nil
end

-- A page stops at the ends instead of wrapping. When it lands on a spacer it
-- keeps going the same way, and only turns back when nothing is left there.
local function page(cursor, delta, opts)
  local n = opts.count
  local target = math.min(math.max(cursor + delta, 1), n)
  local dir = delta < 0 and -1 or 1
  for _, d in ipairs({ dir, -dir }) do
    local i = target
    while i >= 1 and i <= n do
      if is_selectable(i, opts) then
        return i
      end
      i = i + d
    end
  end
  return nil
end

local function page_rows(opts)
  return math.max(opts.page or opts.count, 1)
end

local ACTIONS = {
  up = function(cursor, opts)
    return step(cursor, -1, opts)
  end,
  down = function(cursor, opts)
    return step(cursor, 1, opts)
  end,
  page_up = function(cursor, opts)
    return page(cursor, -page_rows(opts), opts)
  end,
  page_down = function(cursor, opts)
    return page(cursor, page_rows(opts), opts)
  end,
}

-- For a list under a text input, like a filtered picker. Letters are missing
-- on purpose, because the input needs them.
ListNav.KEYS = table.freeze({
  ["<Up>"] = "up",
  ["<Down>"] = "down",
  ["<PageUp>"] = "page_up",
  ["<PageDown>"] = "page_down",
})

-- For a list where nothing takes typed text, so j and k are free to move.
ListNav.VIM_KEYS = table.freeze({
  ["<Up>"] = "up",
  ["<Down>"] = "down",
  ["<PageUp>"] = "page_up",
  ["<PageDown>"] = "page_down",
  j = "down",
  k = "up",
})

-- Moves the cursor for {key} by calling {move_to}(row), and answers whether
-- {key} is a navigation key, so you know when to pass it on. It answers true
-- even when the cursor cannot move, on an empty list or at an end that does
-- not wrap. That way an arrow key never falls through and types into a
-- filter.
--
-- {move_to} only ever gets a selectable row that exists. A {cursor} past the
-- end, left behind when the list shrank, is pulled back onto the list first.
--
-- {opts}:
--   count: rows in the list
--   page: rows a page moves, usually the visible height. Defaults to {count},
--     so a list without a viewport jumps to its ends
--   wrap: false stops Up and Down at the ends. Pages never wrap
--   selectable: function(row) -> boolean, for rows the cursor skips, like
--     spacers or headings
--   keys: key -> "up"|"down"|"page_up"|"page_down", in the spelling
--     `maki.keymap.normalize` gives. Defaults to `ListNav.KEYS`
function ListNav.handle_key(key, cursor, opts, move_to)
  local action = (opts.keys or ListNav.KEYS)[key]
  if not action then
    return false
  end
  local move = ACTIONS[action]
  if not move then
    error(UNKNOWN_ACTION_FMT:format(tostring(key), tostring(action)), 2)
  end
  if opts.count < 1 then
    return true
  end
  local to = move(math.min(math.max(cursor, 1), opts.count), opts)
  if to then
    move_to(to)
  end
  return true
end

return ListNav
