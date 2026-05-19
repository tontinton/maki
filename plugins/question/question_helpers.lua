local QuestionHelpers = {}

function QuestionHelpers.escape_cell(s)
  return (s:gsub("\\", "\\\\"):gsub("|", "\\|"):gsub("\r?\n", "<br>"))
end

function QuestionHelpers.format_answer_table(questions, answers)
  local lines = { "| Question | Answer |", "|----------|--------|" }
  for i, q in ipairs(questions) do
    local ans = answers[i]
    local text = "(no answer)"
    if ans and #ans > 0 then
      local escaped = {}
      for j, v in ipairs(ans) do
        escaped[j] = QuestionHelpers.escape_cell(v)
      end
      text = table.concat(escaped, ", ")
    end
    lines[#lines + 1] = "| " .. QuestionHelpers.escape_cell(q.question) .. " | " .. text .. " |"
  end
  return table.concat(lines, "\n")
end

return QuestionHelpers
