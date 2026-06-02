local QuestionHelpers = {}

function QuestionHelpers.format_answer_list(questions, answers)
  local blocks = {}
  for i, q in ipairs(questions) do
    local lines = { "**Q" .. i .. ".** " .. q.question }
    lines[#lines + 1] = "**A" .. i .. ".**"
    local ans = answers[i]
    if ans and #ans > 0 then
      for _, v in ipairs(ans) do
        lines[#lines + 1] = "- " .. (v:gsub("\r?\n", "\n  "))
      end
    else
      lines[#lines + 1] = "- (no answer)"
    end
    blocks[#blocks + 1] = table.concat(lines, "\n")
  end
  return table.concat(blocks, "\n\n")
end

return QuestionHelpers
