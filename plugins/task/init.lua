-- Structured-output story: the subagent gets a session-local structured_output
-- tool whose handler validates and captures the result as closure upvalues.
-- Invalid input is an inline tool error the model can fix in the same run.
-- This plugin owns structured output and subagent concurrency; Rust exposes
-- primitives only (`maki.agent.session`, `maki.json.schema_validator`,
-- `maki.async.semaphore`).
--
-- It also owns the /tasks picker over the subagents spawned here: picker.lua
-- registers the command and the keymap when this file is loaded, so the two
-- cannot be enabled apart and left pointing at each other's absence.

local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
require("picker")

local STRUCTURED_OUTPUT_NAME = "structured_output"
local STRUCTURED_OUTPUT_DESCRIPTION = "Report your final result. Call it exactly once when your task is complete."
local STRUCTURED_OUTPUT_ACK = "Output recorded."
local STRUCTURED_OUTPUT_PROMPT_SUFFIX = "\n\nWhen finished, call the structured_output tool with your final result."
local MAX_NUDGES = 2
local MAX_SCHEMA_ERRORS = 3
local SCHEMA_COMPILE_ERROR = "invalid output_schema"
local SCHEMA_ROOT_ERROR = "output_schema must have type object"
local STRUCTURED_MISSING_ERROR = "subagent finished without calling structured_output"
local STRUCTURED_INVALID_ERROR = "subagent result does not match output_schema"
local SUMMARY_MISSING_ERROR = "subagent finished without providing a summary"
local NUDGE_MISSING =
  "You did not call the structured_output tool. Call it now with your final result matching its input schema."
local NUDGE_SUMMARY =
  "You finished your work but did not provide a summary. Reply with a concise summary of what you did and found."
local INVALID_INPUT_PREFIX =
  "Input does not match the required schema. Fix the errors and call structured_output again:\n"
local BODY_INDENT_COLS = 4
local MIN_MD_WIDTH = 20
local DEFAULT_OUTPUT_LINES = 5

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

-- Extension points, wrapped via maki.api.set_slot; each returns the shape
-- it received (resolve_model the (model, err) pair).
local resolve_model_slot, system_prompt_slot, tools_slot

local opts = maki.api.register_options({
  max_concurrent = { default = 8, min = 1, desc = "Max concurrently running subagents." },
  allow_model = {
    default = false,
    desc = "Expose a `model` input that overrides the subagent model. Only enable if you trust callers to pick an exact model themselves.",
  },
})

resolve_model_slot = maki.api.declare_slot("task.resolve_model", function(ctx, input)
  return maki.agent.resolve_model(ctx, {
    tier = input.model_tier,
    spec = opts.allow_model and input.model or nil,
  })
end)

system_prompt_slot = maki.api.declare_slot("task.system_prompt", function(system, _ctx, _input, _subagent_type)
  return system
end)

tools_slot = maki.api.declare_slot("task.tools", function(tool_defs, _ctx, _input, _subagent_type)
  return tool_defs
end)

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

-- Only advertise `model` when the plugin opts in: it costs tokens in every
-- task schema, and an off-by-default flag keeps the common path lean.
if opts.allow_model then
  schema.properties.model = {
    type = "string",
    description = 'Exact model spec, e.g. "ollama/glm-5.2". You tell maki the model; maki will not guess. Overrides model_tier.',
  }
end

local examples = {
  {
    description = "Find auth middleware",
    prompt = "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.",
    model_tier = "weak",
  },
}

-- Process-wide cap on concurrent subagents.
local semaphore = maki.async.semaphore(opts.max_concurrent)

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
    if type(input.output_schema) ~= "table" or input.output_schema.type ~= "object" then
      return { llm_output = SCHEMA_ROOT_ERROR, is_error = true }
    end
    local compile_err
    validator, compile_err = maki.json.schema_validator(input.output_schema)
    if compile_err then
      return { llm_output = SCHEMA_COMPILE_ERROR .. ": " .. compile_err, is_error = true }
    end
  end

  local model, model_err = resolve_model_slot(ctx, input)
  if model_err then
    return { llm_output = model_err, is_error = true }
  end

  local audience = subagent_type == "research" and "research_sub" or "general_sub"
  local prompt_id = subagent_type == "research" and "research" or "general"
  local system, system_err = maki.agent.system_prompt(ctx, {
    prompt_id = prompt_id,
    instructions = true,
  })
  if system_err then
    return { llm_output = system_err, is_error = true }
  end
  system = system_prompt_slot(system, ctx, input, subagent_type)

  local tool_defs, tools_err = maki.agent.tools(ctx, {
    audience = audience,
    spec = model.spec,
  })
  if not tools_err then
    tool_defs = tools_slot(tool_defs, ctx, input, subagent_type)
  end
  if tools_err then
    return { llm_output = tools_err, is_error = true }
  end

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

  local permit = semaphore:acquire()

  -- pcall so a raised error cannot leak the permit.
  local ok, out = pcall(function()
    local sess, sess_err = maki.agent.session(ctx, {
      model_spec = model.spec,
      system = system,
      tools = tool_defs,
      local_tools = local_tools,
      audience = audience,
      name = input.description,
    })
    if sess_err then
      return { llm_output = sess_err, is_error = true }
    end

    local message = input.prompt
    if validator then
      message = message .. STRUCTURED_OUTPUT_PROMPT_SUFFIX
    end

    local result, err = sess:prompt(message)
    local retries = 0
    while not err and retries < MAX_NUDGES do
      if validator and not captured then
        retries = retries + 1
        result, err = sess:prompt(NUDGE_MISSING)
      elseif not validator and result.text == "" then
        retries = retries + 1
        result, err = sess:prompt(NUDGE_SUMMARY)
      else
        break
      end
    end

    sess:close()

    if err then
      -- A result alongside the error means the run was cut short after
      -- streaming some text, and half a transcript beats a bare error.
      if result then
        return {
          llm_output = "sub-agent interrupted (" .. err .. "). Partial output:\n" .. result.text,
          is_error = true,
        }
      end
      return { llm_output = "sub-agent error: " .. err, is_error = true }
    end
    if validator and not captured then
      local msg = last_errors and (STRUCTURED_INVALID_ERROR .. ":\n" .. last_errors) or STRUCTURED_MISSING_ERROR
      return { llm_output = msg, is_error = true }
    end
    if not validator and result.text == "" then
      return { llm_output = SUMMARY_MISSING_ERROR, is_error = true }
    end
    return { llm_output = captured and maki.json.encode(captured) or result.text, format = "markdown" }
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

-- Standalone runs render markdown on the Rust side (format = "markdown");
-- this mirrors that for restore and batch children, which build the body here.
local function restore(_input, output, is_error, ctx)
  local tol = ctx:tool_output_lines()
  return ToolView.restore_markdown(output, is_error, {
    max_lines = (tol and tol.task) or DEFAULT_OUTPUT_LINES,
    keep = "head",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
    width = math.max(maki.ui.terminal_size().cols - BODY_INDENT_COLS, MIN_MD_WIDTH),
  })
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
  restore = restore,
})
