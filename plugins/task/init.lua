local description = [[Launch an autonomous subagent to perform tasks independently. Best combined with batch.

Subagent types (set via `subagent_type`):
- `research` (default): Read-only tools. For codebase exploration or gathering context.
- `general`: Full tool access. For delegating implementation work.

Notes:
1. Launch multiple tasks concurrently when possible.
2. The agent's result is not visible to the user. Summarize it in your response.
3. Each invocation starts fresh - inline any needed context into the prompt.
4. Tell it to return concise summaries with file:line refs, not full file contents.
]]

local schema = {
  type = "object",
  required = { "description", "prompt" },
  additionalProperties = false,
  properties = {
    description = {
      type = "string",
      description = "Short (3-5 words) description of the task",
    },
    prompt = {
      type = "string",
      description = "Detailed task prompt for the agent",
    },
    subagent_type = {
      type = "string",
      description = 'Subagent type: "research" (read-only, default) or "general" (can modify files)',
    },
    model_tier = {
      type = "string",
      description = 'Model tier (optional, omit to use current model, capped at current tier):\n- "strong" (e.g. Opus): Deep reasoning, complex architecture, subtle bugs, most critical sections. ~5x cost of medium.\n- "medium" (e.g. Sonnet): Balanced. Refactors, features, multi-file changes.\n- "weak" (e.g. Haiku): Fast/cheap. Search, summarize, boilerplate, simple edits.',
    },
  },
}

local examples = {
  {
    description = "Find auth middleware",
    prompt = "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.",
    model_tier = "weak",
  },
}

local function handler(input, ctx)
  local agent_ctx = ctx:agent_context()
  local subagent_type = input.subagent_type or "research"
  if subagent_type ~= "research" and subagent_type ~= "general" then
    return { llm_output = "unknown subagent type: " .. subagent_type, is_error = true }
  end

  local model = maki.agent.resolve_model(agent_ctx, {
    tier = input.model_tier,
  })

  local audience = subagent_type == "research" and "research_sub" or "general_sub"
  local prompt_id = subagent_type == "research" and "research" or "general"
  local system = maki.agent.system_prompt(agent_ctx, {
    prompt_id = prompt_id,
    instructions = true,
  })

  local tool_defs = maki.agent.tools(agent_ctx, {
    audience = audience,
    spec = model.spec,
    include_mcp = true,
  })

  local result = maki.agent.run(agent_ctx, {
    prompt = input.prompt,
    model_spec = model.spec,
    system = system,
    tools = tool_defs,
    name = input.description,
  })

  return { llm_output = result.text }
end

local function header(input)
  return input.description
end

maki.api.register_tool({
  name = "task",
  description = description,
  kind = "execute",
  audiences = { "main" },
  examples = examples,
  schema = schema,
  handler = handler,
  header = header,
})
