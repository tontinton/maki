-- Tenstorrent Cloud. Chat is OpenAI-compatible at console.tenstorrent.com/v1.
--
-- Two gateways share the served catalogue. Qwen and MiniMax take the thinking
-- toggle as `chat_template_kwargs.enable_thinking`. GLM-5.3 ignores that and
-- keeps thinking on until `reasoning_effort` is `"low"`; `"medium"` turns it
-- back on. The VL instruct model does not think, and `reasoning_effort =
-- "none"` is a 400 there, so it gets no fragment. The other ids are on the
-- rate card and were not probed, so thinking stays off.
--
-- Prices are USD per 1M tokens from the console pricing table. Cached input
-- is `cache_read`. The card has no cache-write column, and created cache
-- tokens stay in the input bucket, so `cache_write` is 0. Llama 3.3 70B is
-- omitted: the card quotes an input rate and leaves cached input and output
-- blank. A context window is stated only where a live probe measured one.

local TOGGLE_ON = { chat_template_kwargs = { enable_thinking = true } }
local TOGGLE_OFF = { chat_template_kwargs = { enable_thinking = false } }
local GLM_ON = { reasoning_effort = "medium" }
local GLM_OFF = { reasoning_effort = "low" }

local function rate(input, cached, output)
  return { input = input, output = output, cache_write = 0.0, cache_read = cached }
end

-- Priced from the card, absent from the live catalogue. No window, no fragment.
local function listed(id, tier, input, cached, output, family)
  return {
    prefixes = { id },
    tier = tier,
    family = family,
    supports_thinking = false,
    supports_vision = false,
    pricing = rate(input, cached, output),
  }
end

maki.provider.register({
  slug = "tenstorrent",
  display_name = "Tenstorrent",
  codec = "openai",
  base_url = "https://console.tenstorrent.com/v1",
  api_key_env = "TENSTORRENT_API_KEY",
  login_url = "https://console.tenstorrent.com/dashboard/inference/keys",
  default_model = "Qwen/Qwen3.8-27B",
  family = "generic",
  accepts_arbitrary_models = false,
  -- The host compares `max_tokens` to the whole window. 16384 sits under every
  -- measured window, the smallest of which is 131072.
  max_output_tokens = 16384,
  aperture = { path_prefix = "/v1" },
  docs = { features = "Open-weight models, tool calls, prompt caching" },
  models = {
    listed("MiniMaxAI/MiniMax-M3", "medium", 0.6, 0.12, 2.4),
    {
      prefixes = { "MiniMaxAI/MiniMax-M3.1-Flash" },
      tier = "medium",
      supports_thinking = true,
      supports_vision = false,
      context_window = 1048576,
      pricing = rate(0.15, 0.015, 0.6),
      thinking_fields = { off = TOGGLE_OFF, adaptive = TOGGLE_ON },
    },
    {
      prefixes = { "Qwen/Qwen3-32B" },
      tier = "medium",
      supports_thinking = true,
      supports_vision = false,
      context_window = 131072,
      pricing = rate(0.14, 0.09, 0.4),
      thinking_fields = { off = TOGGLE_OFF, adaptive = TOGGLE_ON },
    },
    {
      prefixes = { "Qwen/Qwen3-VL-32B-Instruct" },
      tier = "medium",
      supports_thinking = false,
      supports_vision = true,
      context_window = 131072,
      pricing = rate(0.9, 0.45, 0.9),
    },
    {
      prefixes = { "Qwen/Qwen3.8-27B" },
      tier = "medium",
      default = true,
      supports_thinking = true,
      supports_vision = false,
      context_window = 262144,
      pricing = rate(0.02, 0.009, 0.5),
      thinking_fields = { off = TOGGLE_OFF, adaptive = TOGGLE_ON },
    },
    listed("deepseek-ai/DeepSeek-R1-0528", "strong", 3, 1.5, 8),
    listed("moonshotai/Kimi-K2.6", "medium", 0.95, 0.16, 4),
    listed("moonshotai/Kimi-K2.7-Code", "medium", 0.85, 0.14, 3),
    listed("moonshotai/Kimi-K3", "strong", 3, 0.3, 15),
    listed("openai/gpt-oss-120b", "medium", 0.15, 0.075, 0.6),
    listed("zai-org/GLM-5.2", "strong", 2.1, 0.21, 6.6, "glm"),
    {
      prefixes = { "zai-org/GLM-5.3" },
      tier = "strong",
      default = true,
      family = "glm",
      supports_thinking = true,
      supports_vision = false,
      context_window = 1048576,
      pricing = rate(0.6, 0.1, 2.5),
      thinking_fields = { off = GLM_OFF, adaptive = GLM_ON },
    },
  },
})
