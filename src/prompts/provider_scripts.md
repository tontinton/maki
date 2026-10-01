Port my maki provider scripts to Lua plugins.

Maki no longer runs the executable scripts in `{providers_dir}`.
A Lua plugin that calls `maki.provider.register` replaces each one.
Give each plugin the script's file name as its slug.
Then my saved models (`<slug>/<model>`) and `maki auth login <slug>` keep working.

Scripts to port:
{scripts}

## Before you write anything

- If you are maki, load the `maki-plugin-dev` skill.
  Otherwise read https://maki.sh/docs/providers/#plugin-providers and https://maki.sh/docs/lua-api/#maki-provider-register.
  They define the API. Do not guess field names: an unknown key fails registration.
- Read each script's source.
  You may run `<script> info` and `<script> models` to see the JSON they print.
  Do not run `resolve`, `refresh`, `reload`, `login` or `logout`: they print live credentials or wait for my input.
- Do not change, move or delete the scripts or any credential file they use.

## How the script protocol maps to the Lua API

- `info`: `display_name` and `system_prefix` keep their names.
  `has_auth: true` becomes a `login` hook.
- `base`: keep `base = "<same>"` when it is one of `anthropic`, `openai`, `google`, `copilot`, `ollama`, `llama-cpp`, `zai`, `opencode`, `xai` or `aperture`.
  Any other old base (`mistral`, `deepseek`, `openrouter`, `requesty`, `synthetic`, `regolo`, `tensorx`) is no longer valid.
  Use `codec = "openai"` instead, with that provider's API origin as `base_url` when `resolve` did not return one.
- `models`: becomes the `models` table.
  Each `"id": "x"` becomes `prefixes = { "x" }`, and every other field keeps its name and default.
  Rows describe models but do not limit the list: without a `list_models` hook, maki also lists what the API's model endpoint serves.
  If the API has no model list, add `list_models = function() return {} end` so only the rows are listed.
  A script without `models` used its base's catalog.
  With a kept `base`, leave `models` out.
  With `codec = "openai"`, add a `list_models` hook that reads the API's model list with `ctx.get_json`, or ask me which models I use.
- `resolve`, `refresh` and `reload`: become one `auth = function(ctx, purpose)` hook.
  It returns `{ base_url = ..., headers = { ... } }`, and `purpose` is `"resolve"`, `"refresh"` or `"reload"`.
  Keep the script's expiry checks: a `"resolve"` that finds an expired token refreshes it, as the script did.
- `login` and `logout`: become `login = function(ctx)` and `logout = function(ctx)`.
  They talk to me through `ctx.print`, `ctx.prompt({ label = ..., secret = true })` and `ctx.open_url`.
- HTTP calls in the script become `maki.net.request`.
- Credentials: store them with `maki.provider.auth.get`, `set` and `clear`.
  If the script kept tokens in its own file, import that file the first time `maki.provider.auth.get` returns nil.
  Then I do not have to log in again.
- A key read from an environment variable becomes `api_key_env = "VAR"`, with no `auth` hook.
- A secret from another program (`op`, `pass`, `gcloud`, `security`, `gh`, ...): run that same command from the hook with `maki.fn.jobstart({ ... })` and `maki.fn.jobwait(id, timeout_ms)`.
  Both return nil and an error message on failure (missing binary, timeout), so raise it: `local id, err = maki.fn.jobstart({ ... }); if not id then error(err) end`, and the same for `jobwait`.
  Do not call the old script. An `auth` hook has 30 seconds, and `login` has no time limit.

## Details that are easy to get wrong

- `maki.provider.register` takes one table. `slug` and `display_name` are required fields in it.
- Every hook gets `ctx` first. Pass `ctx.slug` to `maki.provider.auth.get`, `set` and `clear`.
- Call `maki.provider.auth.set` and `clear` inside hooks only, never at the top level of the file.
- `maki.net.request` returns a response for a 4xx or 5xx too. Check `res.status`, then fail with `return nil, maki.provider.http_error(res)`.
- For any other failure, raise with `error("message")`. The message reaches me. `return nil, "message"` does not work.
- `login` and `logout` return nothing.

## Where the files go

- One file per provider at `{config_dir}/lua/<slug>.lua`.
  Load it with a `require("<slug>")` line in `{config_dir}/init.lua`.
  Create `init.lua` if it is missing, and keep everything already in it.
- Permissions live in `{config_dir}/plugin.toml`, shared by every Lua file in that directory.
  Registering a provider needs `net_hosts` under `[permissions]`.
  List every host the plugin reaches: the API, any auth or token endpoint, and a loopback `base_url` too.
  Entries are host names only, with no scheme or port, such as `"api.acme.com"` or `"127.0.0.1"`.
- Once `net_hosts` is set, `maki.net` in my other plugins in that directory can reach only those hosts.
  So first search `{config_dir}/init.lua` and `{config_dir}/lua/` for `maki.net`, and add the hosts they use.
- Keep every existing key in `plugin.toml`.
  If it sets `net`, `run`, `env` or `fs_read` to `false` and a plugin needs it, ask me before changing it.
  A new `plugin.toml` needs only `[permissions]` and `net_hosts`, because a file I write grants every other permission.
- A provider on localhost or a LAN address: chat requests to an `http://` loopback `base_url` work.
  `ctx.get_json` and `maki.net` refuse private addresses until the host is allowed in `{config_dir}/init.lua`:
  `maki.setup({ net = { allowed_private_hosts = { "localhost:4000" } } })`, merged into any `maki.setup` call already there.

## Check your work

For each slug:

1. `maki models 2>&1 | grep -E '^warning|^<slug>/'` lists the models I had before, with no `warning:` line about the plugin.
   A provider that needs a login may warn that it is not logged in. That warning is expected until I log in.
2. When the credentials come from an imported file, an environment variable or a command, send one real request: `maki -p -m <slug>/<model> "Reply with OK"`.
   When they need a login, stop and ask me to run `maki auth login <slug>`, then send the request.
3. `maki migrate providers` no longer lists the slug.

Hook errors are logged to `maki.log` in `{logs_dir}`.

When every check passes, tell me which scripts and old credential files I can delete. Do not delete them yourself.
