-- Container files are a flat list of instructions with no nesting, so the
-- skeleton is those instructions in source order; comments carry no structure.
return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local format_skeleton = U.format_skeleton
  local SECTION = U.SECTION

  local INSTRUCTION_TRUNCATE = 100

  return {
    extract = function(source, root)
      local entries = {}
      for _, child in ipairs(root:children()) do
        -- The grammar names every instruction `<name>_instruction`, and they
        -- are the only non-extra children a source file has.
        if child:type():match("_instruction$") then
          local text = truncate(compact_ws(get_text(child, source)), INSTRUCTION_TRUNCATE)
          entries[#entries + 1] = new_entry(SECTION.Instruction, child, text)
        end
      end
      return format_skeleton(entries, {}, nil, "")
    end,
  }
end
