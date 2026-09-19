-- Synthetic, as a declaration. The slug is one maki ships, so claiming it
-- inherits the display name, the key env var and the curated model table, and
-- restating any of them here would be a registration error, not an override.

maki.provider.register({
  slug = "synthetic",
  codec = "openai",
  openai = {
    max_tokens_field = "max_completion_tokens",
    include_stream_usage = false,
    thinking = { dialect = "standard" },
  },
})
