-- Structured-output story: the subagent gets a session-local structured_output
-- tool whose handler validates and captures the result as closure upvalues.
-- Invalid input is an inline tool error the model can fix in the same run.
-- Rust provides session, schema_validator, and semaphore primitives only.

local STRUCTURED_OUTPUT_NAME = "structured_output"
local STRUCTURED_OUTPUT_DESCRIPTION = "Report your final result. Call it exactly once when your task is complete."
local STRUCTURED_OUTPUT_ACK = "Output recorded."
local STRUCTURED_OUTPUT_PROMPT_SUFFIX = "\n\nWhen finished, call the structured_output tool with your final result."
local MAX_STRUCTURED_RETRIES = 2
local MAX_SCHEMA_ERRORS = 3
local SCHEMA_COMPILE_ERROR = "invalid output_schema"
local STRUCTURED_MISSING_ERROR = "subagent finished without calling structured_output"
local STRUCTURED_INVALID_ERROR = "subagent result does not match output_schema"
local NUDGE_MISSING =
  "You did not call the structured_output tool. Call it now with your final result matching its input schema."
local INVALID_INPUT_PREFIX =
  "Input does not match the required schema. Fix the errors and call structured_output again:\n"

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
    output_schema = {
      description = "JSON Schema (object) the subagent's final result must match. When set, the result is returned as a validated JSON string.",
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

local semaphore

local function bounded_errors(errors)
  local out = {}
  for i = 1, math.min(#errors, MAX_SCHEMA_ERRORS) do
    out[i] = errors[i]
  end
  return table.concat(out, "\n")
end

local function handler(input, ctx)
  local subagent_type = input.subagent_type or "research"
  if subagent_type ~= "research" and subagent_type ~= "general" then
    return { llm_output = "unknown subagent type: " .. subagent_type, is_error = true }
  end

  -- Compile early: a bad schema costs zero tokens.
  local validator
  if input.output_schema then
    local compile_err
    validator, compile_err = maki.json.schema_validator(input.output_schema)
    if compile_err then
      return { llm_output = SCHEMA_COMPILE_ERROR .. ": " .. compile_err, is_error = true }
    end
  end

  local model = maki.agent.resolve_model(ctx, {
    tier = input.model_tier,
  })

  local audience = subagent_type == "research" and "research_sub" or "general_sub"
  local prompt_id = subagent_type == "research" and "research" or "general"
  local system = maki.agent.system_prompt(ctx, {
    prompt_id = prompt_id,
    instructions = true,
  })

  local tool_defs = maki.agent.tools(ctx, {
    audience = audience,
    spec = model.spec,
    include_mcp = true,
  })

  local captured, last_errors
  local local_tools
  if validator then
    local_tools = {
      [STRUCTURED_OUTPUT_NAME] = {
        description = STRUCTURED_OUTPUT_DESCRIPTION,
        input_schema = input.output_schema,
        handler = function(value)
          local errs = validator:validate(value)
          if errs then
            last_errors = bounded_errors(errs)
            return nil, INVALID_INPUT_PREFIX .. last_errors
          end
          captured = value
          return STRUCTURED_OUTPUT_ACK
        end,
      },
    }
  end

  -- Process-wide cap on concurrent subagents, sized from config on first use.
  if not semaphore then
    semaphore = maki.async.semaphore(math.max(ctx:config().task_max_concurrent or 1, 1))
  end
  local permit = semaphore:acquire()

  -- pcall so a raised error cannot leak the permit.
  local ok, out = pcall(function()
    local sess = maki.agent.session(ctx, {
      model_spec = model.spec,
      system = system,
      tools = tool_defs,
      local_tools = local_tools,
      audience = audience,
      name = input.description,
    })

    local message = input.prompt
    if validator then
      message = message .. STRUCTURED_OUTPUT_PROMPT_SUFFIX
    end

    local result, err = sess:prompt(message)
    local retries = 0
    while not err and validator and not captured and retries < MAX_STRUCTURED_RETRIES do
      retries = retries + 1
      result, err = sess:prompt(NUDGE_MISSING)
    end

    sess:close()

    if err then
      return { llm_output = "sub-agent error: " .. err, is_error = true }
    end
    if validator and not captured then
      local msg = last_errors and (STRUCTURED_INVALID_ERROR .. ":\n" .. last_errors) or STRUCTURED_MISSING_ERROR
      return { llm_output = msg, is_error = true }
    end
    return { llm_output = captured and maki.json.encode(captured) or result.text }
  end)

  permit:release()
  if not ok then
    error(out, 0)
  end
  return out
end

local function header(input)
  return input.description
end

maki.api.register_tool({
  name = "task",
  description = description,
  kind = "execute",
  audiences = { "main", "workflow" },
  examples = examples,
  schema = schema,
  handler = handler,
  header = header,
})
