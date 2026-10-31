-- What the task tool runs, from input checks to the prompt loop. A background
-- task is the same call moved off its turn, so it shares all of it.
--
-- Structured-output story: the subagent gets a session-local structured_output
-- tool whose handler validates the result and keeps it in `capture`. Invalid
-- input is an inline tool error the model can fix in the same run. This plugin
-- owns structured output and subagent concurrency. Rust exposes primitives only
-- (`maki.agent.session`, `maki.json.schema_validator`, `maki.async.semaphore`).

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

local Runner = {}

local opts = maki.api.register_options({
  max_concurrent = { default = 8, min = 1, desc = "Max concurrently running subagents." },
  allow_background = {
    default = false,
    desc = "Expose a `background` input that pushes subagent results to the chat when they finish.",
  },
  allow_model = {
    default = false,
    desc = "Expose a `model` input that overrides the subagent model. Only enable if you trust callers to pick an exact model themselves.",
  },
})

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
    thinking = {
      description = "Thinking: off|adaptive|minimal|low|medium|high|xhigh|max|int budget. Omit to inherit parent; capped at parent.",
    },
    output_schema = {
      type = "object",
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

if opts.allow_background then
  schema.properties.background = {
    type = "boolean",
    description = "Run in the background and continue other work: the call returns at once and the result arrives later as a message. Launch every background task in one parallel batch, never as sequential calls, and don't sleep or poll for it; end your turn once nothing else is left.",
    needs_background = true,
  }
end

Runner.allow_background = opts.allow_background
Runner.schema = schema

local slots = maki.async.semaphore(opts.max_concurrent)

-- {on_wait} runs only when every slot is taken, right before the call parks
-- for one.
function Runner.acquire_slot(on_wait)
  local permit = slots:try_acquire()
  if permit then
    return permit
  end
  on_wait()
  return slots:acquire()
end

local function bounded_errors(errors)
  local out = {}
  for i = 1, math.min(#errors, MAX_SCHEMA_ERRORS) do
    out[i] = errors[i]
  end
  return table.concat(out, "\n")
end

function Runner.prepare(input, ctx)
  local subagent_type = input.subagent_type or "research"
  if subagent_type ~= "research" and subagent_type ~= "general" then
    return nil, "unknown subagent type: " .. subagent_type
  end

  -- Compile early: a bad schema costs zero tokens. Models often leave out
  -- the root type, and tool inputs are always objects, so fill it in
  -- instead of bouncing the call.
  local validator
  local output_schema = input.output_schema
  if output_schema then
    if (output_schema.type or "object") ~= "object" then
      return nil, SCHEMA_ROOT_ERROR
    end
    output_schema.type = "object"
    local compile_err
    validator, compile_err = maki.json.schema_validator(output_schema)
    if compile_err then
      return nil, SCHEMA_COMPILE_ERROR .. ": " .. compile_err
    end
  end

  local model, model_err = maki.agent.resolve_model(ctx, {
    tier = input.model_tier,
    spec = opts.allow_model and input.model or nil,
  })
  if model_err then
    return nil, model_err
  end

  local audience = subagent_type == "research" and "research_sub" or "general_sub"
  local prompt_id = subagent_type == "research" and "research" or "general"
  local system, system_err = maki.agent.system_prompt(ctx, {
    prompt_id = prompt_id,
    instructions = true,
  })
  if system_err then
    return nil, system_err
  end

  local tool_defs, tools_err = maki.agent.tools(ctx, {
    audience = audience,
    spec = model.spec,
    mcp = true,
  })
  if tools_err then
    return nil, tools_err
  end

  local capture = validator and {} or nil
  local local_tools
  if validator then
    local_tools = {
      [STRUCTURED_OUTPUT_NAME] = {
        description = STRUCTURED_OUTPUT_DESCRIPTION,
        input_schema = output_schema,
        handler = function(value)
          local errs = validator:validate(value)
          if errs then
            capture.last_errors = bounded_errors(errs)
            return nil, INVALID_INPUT_PREFIX .. capture.last_errors
          end
          capture.value = value
          return STRUCTURED_OUTPUT_ACK
        end,
      },
    }
  end

  return {
    options = {
      model_spec = model.spec,
      system = system,
      tools = tool_defs,
      local_tools = local_tools,
      audience = audience,
      mcp = true,
      name = input.description,
      thinking = input.thinking,
    },
    message = input.prompt .. (validator and STRUCTURED_OUTPUT_PROMPT_SUFFIX or ""),
    capture = capture,
  }
end

function Runner.run(sess, prepared)
  local capture = prepared.capture
  local result, err = sess:prompt(prepared.message)
  local retries = 0
  while not err and retries < MAX_NUDGES do
    if capture and not capture.value then
      retries = retries + 1
      result, err = sess:prompt(NUDGE_MISSING)
    elseif not capture and result.text == "" then
      retries = retries + 1
      result, err = sess:prompt(NUDGE_SUMMARY)
    else
      break
    end
  end

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
  if capture and not capture.value then
    local msg = capture.last_errors and (STRUCTURED_INVALID_ERROR .. ":\n" .. capture.last_errors)
      or STRUCTURED_MISSING_ERROR
    return { llm_output = msg, is_error = true }
  end
  if not capture and result.text == "" then
    return { llm_output = SUMMARY_MISSING_ERROR, is_error = true }
  end
  return { llm_output = capture and maki.json.encode(capture.value) or result.text, format = "markdown" }
end

return Runner
