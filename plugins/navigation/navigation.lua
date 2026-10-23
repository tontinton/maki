local M = {}

function M.target(view, forward, users_only, wrap)
  local bottom = math.max(1, view.line_count - view.height + 1)
  local function topline(target)
    return math.min(target.line, bottom)
  end
  local targets = {}
  for _, position in ipairs(view.positions) do
    if not users_only or position.role == "user" then
      targets[#targets + 1] = position
    end
  end
  if #targets == 0 then
    return nil
  end

  if forward then
    for _, target in ipairs(targets) do
      if topline(target) > view.topline then
        return target, false
      end
    end
  else
    for index = #targets, 1, -1 do
      local target = targets[index]
      if topline(target) < view.topline then
        return target, false
      end
    end
  end

  local wrapped = forward and targets[1] or targets[#targets]
  if wrap and topline(wrapped) ~= view.topline then
    return wrapped, false
  end
  return forward and targets[#targets] or targets[1], true
end

return M
