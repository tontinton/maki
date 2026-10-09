local M = {}

function M.target(view, forward, users_only, wrap)
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
      if target.topline > view.topline then
        return target, false
      end
    end
  else
    for index = #targets, 1, -1 do
      local target = targets[index]
      if target.topline < view.topline then
        return target, false
      end
    end
  end

  local wrapped = forward and targets[1] or targets[#targets]
  if wrap and wrapped.topline ~= view.topline then
    return wrapped, false
  end
  return forward and targets[#targets] or targets[1], true
end

return M
