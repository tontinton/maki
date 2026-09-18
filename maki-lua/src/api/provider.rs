use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::marker::PhantomData;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maki_config::providers::Protocol;
use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::AgentError;
use maki_providers::plugin::{self, Hook, PluginModel, ProviderHooks, Registration};
use maki_providers::provider::BoxFuture;
use maki_storage::StateDir;
use maki_storage::auth::{
    delete_plugin_auth, load_plugin_auth, lock_credentials, save_plugin_auth,
};
use mlua::{Function, Lua, MultiValue, RegistryKey, Result as LuaResult, Table, Value as LuaValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::api::util::convert::{json_to_lua, lua_to_json};
use crate::api::util::pair::{Pair, try_pair};
use crate::plugin_permissions::{NetHosts, PluginPermissions};
use crate::runtime::{DeferredCallback, Request, host_senders, run_detached};

/// How long the host waits for a hook that runs between turns. Generous,
/// because an auth hook may be talking to an identity provider, but bounded:
/// a plugin that never answers must not park the model behind it forever.
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);
/// The budget for hooks that sit between the user and their first token.
/// Anything slower here is felt as latency on every single request.
const REQUEST_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

const RESOLVE_AUTH: &str = "resolve_auth";
const REFRESH_AUTH: &str = "refresh_auth";
const RELOAD_AUTH: &str = "reload_auth";
const LIST_MODELS: &str = "list_models";
const BUILD_BODY: &str = "build_body";
const MAP_ERROR: &str = "map_error";
const FETCH_USAGE: &str = "fetch_usage";
const LOGIN: &str = "login";
const LOGOUT: &str = "logout";

const SLUG: &str = "slug";
const DISPLAY_NAME: &str = "display_name";
const CODEC: &str = "codec";
const BASE: &str = "base";
const BASE_URL: &str = "base_url";
const API_KEY_ENV: &str = "api_key_env";
const SYSTEM_PREFIX: &str = "system_prefix";
const MODELS: &str = "models";

const BODY_FIELD: &str = "body";
const MODEL_FIELD: &str = "model";
const STATUS_FIELD: &str = "status";
const MESSAGE_FIELD: &str = "message";

const NO_NET_HOSTS: &str = "maki.provider.register: declare the hosts this provider talks to as \
     `net_hosts` under `[permissions]` in plugin.toml before registering";
const SECRET_MASK: char = '*';

/// One plugin-supplied provider callback, named the way the registry names it.
///
/// A slot is not the same thing as a Lua entry: `Auth` is one hook the registry
/// calls with a purpose, and three entries a plugin may write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookSlot {
    Auth,
    ListModels,
    BuildBody,
    MapError,
    FetchUsage,
    Login,
    Logout,
}

/// Everything that differs between hooks, one row each, so adding a hook is a
/// row and a call site rather than a branch anywhere in the bridge.
struct SlotSpec {
    /// Lua entries that can serve the slot, each with the serialized payload
    /// that selects it. Only auth has more than one, and `""` marks the entry
    /// every payload falls back to.
    entries: &'static [(&'static str, &'static str)],
    /// Payload fields handed over as positional arguments, in order. Whatever
    /// the payload holds beyond them arrives as a trailing table, so the Lua
    /// signature an author writes does not have to track the wire shape.
    positional: &'static [&'static str],
    /// Whether the call is handed a `ctx` that can talk to the terminal.
    ctx: bool,
}

impl HookSlot {
    /// Every slot, so the Lua entry names live in exactly one table: the one
    /// [`Self::spec`] holds.
    const ALL: [Self; 7] = [
        Self::Auth,
        Self::ListModels,
        Self::BuildBody,
        Self::MapError,
        Self::FetchUsage,
        Self::Login,
        Self::Logout,
    ];

    const fn spec(self) -> SlotSpec {
        match self {
            Self::Auth => SlotSpec {
                entries: &[
                    ("resolve", RESOLVE_AUTH),
                    ("refresh", REFRESH_AUTH),
                    ("reload", RELOAD_AUTH),
                ],
                positional: &[],
                ctx: false,
            },
            Self::ListModels => SlotSpec {
                entries: &[("", LIST_MODELS)],
                positional: &[],
                ctx: false,
            },
            Self::BuildBody => SlotSpec {
                entries: &[("", BUILD_BODY)],
                positional: &[BODY_FIELD, MODEL_FIELD],
                ctx: false,
            },
            Self::MapError => SlotSpec {
                entries: &[("", MAP_ERROR)],
                positional: &[STATUS_FIELD, MESSAGE_FIELD],
                ctx: false,
            },
            Self::FetchUsage => SlotSpec {
                entries: &[("", FETCH_USAGE)],
                positional: &[],
                ctx: false,
            },
            Self::Login => SlotSpec {
                entries: &[("", LOGIN)],
                positional: &[],
                ctx: true,
            },
            Self::Logout => SlotSpec {
                entries: &[("", LOGOUT)],
                positional: &[],
                ctx: true,
            },
        }
    }
}

/// The Lua functions one registration handed over, plus the way back to the
/// thread that may call them.
///
/// Held by `Arc` from every hook the registration produced, so a call that
/// started before an unload finishes against the functions it started with:
/// the plugin's environment goes away, these registry entries do not.
pub struct LuaHookKeys {
    plugin: Arc<str>,
    slug: String,
    keys: HashMap<&'static str, RegistryKey>,
    requests: flume::Sender<Request>,
    release: flume::Sender<DeferredCallback>,
}

impl LuaHookKeys {
    /// The entry that serves this call: the one the payload names, else the
    /// first the plugin supplied, in the order [`HookSlot::spec`] lists them.
    /// A plugin that writes only `resolve_auth` gets it for refreshes and
    /// reloads too, which is right for one that reads its credentials fresh
    /// every time, and one that writes only `refresh_auth` gets that for the
    /// initial resolve rather than no way to mint credentials at all.
    fn entry(&self, slot: HookSlot, payload: &Value) -> Option<&RegistryKey> {
        let entries = slot.spec().entries;
        let selector = payload.as_str().unwrap_or_default();
        entries
            .iter()
            .find(|(select, _)| *select == selector)
            .and_then(|(_, entry)| self.keys.get(entry))
            .or_else(|| entries.iter().find_map(|(_, entry)| self.keys.get(entry)))
    }

    fn serves(&self, slot: HookSlot) -> bool {
        slot.spec()
            .entries
            .iter()
            .any(|(_, entry)| self.keys.contains_key(entry))
    }
}

impl Drop for LuaHookKeys {
    /// A registry key may only be released on the Lua thread, and this drop
    /// runs wherever the last hook handle happened to die. The defer queue is
    /// the existing way back there; a callback handed over already cancelled is
    /// the dispatcher's path for releasing a key without running it.
    fn drop(&mut self) {
        for (_, func) in std::mem::take(&mut self.keys) {
            let _ = self.release.send(DeferredCallback {
                func,
                delay: Duration::ZERO,
                plugin: Arc::clone(&self.plugin),
                cancel: Arc::new(AtomicBool::new(true)),
            });
        }
    }
}

/// One hook, as the registry sees it: input in, output out, both checked by the
/// compiler against the slot's declared pair.
pub struct LuaHook<In, Out> {
    keys: Arc<LuaHookKeys>,
    slot: HookSlot,
    /// How long to wait for an answer, or `None` for the hooks that wait on a
    /// person rather than on a server.
    timeout: Option<Duration>,
    _types: PhantomData<fn(In) -> Out>,
}

impl<In, Out> Hook<In, Out> for LuaHook<In, Out>
where
    In: Serialize + Send + 'static,
    Out: DeserializeOwned + Send + 'static,
{
    fn call(&self, input: In) -> BoxFuture<'_, Result<Out, AgentError>> {
        Box::pin(async move {
            let payload = serde_json::to_value(input)?;
            let (reply, answer) = flume::bounded(1);
            self.keys
                .requests
                .send(Request::CallProviderHook {
                    hook: Arc::clone(&self.keys),
                    slot: self.slot,
                    payload,
                    reply,
                })
                .map_err(|_| AgentError::Channel)?;
            let answered = async { answer.recv_async().await.map_err(|_| AgentError::Channel) };
            let value = match self.timeout {
                Some(limit) => {
                    futures_lite::future::or(answered, async {
                        smol::Timer::after(limit).await;
                        Err(self.timed_out(limit))
                    })
                    .await?
                }
                None => answered.await?,
            };
            let value = value.map_err(|message| AgentError::Config {
                message: format!(
                    "provider '{}': {:?} hook failed: {message}",
                    self.keys.slug, self.slot
                ),
            })?;
            Ok(serde_json::from_value(value)?)
        })
    }
}

impl<In, Out> LuaHook<In, Out> {
    fn timed_out(&self, limit: Duration) -> AgentError {
        AgentError::Config {
            message: format!(
                "provider '{}': {:?} hook did not answer within {}s",
                self.keys.slug,
                self.slot,
                limit.as_secs()
            ),
        }
    }
}

/// The one place a hook handle is made. `In` and `Out` come from the field it
/// is assigned to, so the registry's idea of a hook's shape and the bridge's
/// are the same idea.
fn hook<In, Out>(
    keys: &Arc<LuaHookKeys>,
    slot: HookSlot,
    timeout: Option<Duration>,
) -> Option<Arc<dyn Hook<In, Out>>>
where
    In: Serialize + Send + 'static,
    Out: DeserializeOwned + Send + 'static,
{
    keys.serves(slot).then(|| {
        Arc::new(LuaHook {
            keys: Arc::clone(keys),
            slot,
            timeout,
            _types: PhantomData,
        }) as Arc<dyn Hook<In, Out>>
    })
}

/// Runs one hook call on the Lua thread. The whole bridge in one direction:
/// JSON in, Lua arguments, JSON back out.
pub(crate) async fn run_hook(
    lua: &Lua,
    keys: &LuaHookKeys,
    slot: HookSlot,
    payload: Value,
) -> Result<Value, String> {
    let Some(key) = keys.entry(slot, &payload) else {
        return Err(format!("no lua function is registered for {slot:?}"));
    };
    let func: Function = lua.registry_value(key).map_err(|e| e.to_string())?;
    let args = call_args(lua, slot.spec(), payload).map_err(|e| e.to_string())?;
    let result = run_detached(lua, async {
        lua.create_thread(func)?.into_async::<LuaValue>(args)?.await
    })
    .await
    .map_err(|e| e.to_string())?;
    lua_to_json(lua, &result).map_err(|e| e.to_string())
}

fn call_args(lua: &Lua, spec: SlotSpec, payload: Value) -> LuaResult<MultiValue> {
    let mut args = Vec::new();
    if spec.ctx {
        args.push(LuaValue::Table(stdio_ctx(lua)?));
    }
    match payload {
        Value::Null => {}
        Value::Object(mut fields) if !spec.positional.is_empty() => {
            for name in spec.positional {
                let field = fields.remove(*name).unwrap_or(Value::Null);
                args.push(json_to_lua(lua, &field)?);
            }
            args.push(json_to_lua(lua, &Value::Object(fields))?);
        }
        other => args.push(json_to_lua(lua, &other)?),
    }
    Ok(MultiValue::from_vec(args))
}

/// The `ctx` a login or logout gets. Stdio, because the one caller is the cli,
/// which is exactly what the retired subprocess providers got by inheriting it.
fn stdio_ctx(lua: &Lua) -> LuaResult<Table> {
    let ctx = lua.create_table()?;
    ctx.set(
        "print",
        lua.create_function(|_, text: String| {
            println!("{text}");
            Ok(())
        })?,
    )?;
    ctx.set(
        "prompt",
        lua.create_function(|_, opts: Table| {
            let label: String = opts.get("label").unwrap_or_default();
            let secret = opts.get::<Option<bool>>("secret")?.unwrap_or(false);
            Ok(read_answer(&label, secret))
        })?,
    )?;
    ctx.set(
        "open_url",
        lua.create_function(|_, url: String| -> LuaResult<Pair<bool>> {
            try_pair!(open::that(&url));
            Ok((Some(true), None))
        })?,
    )?;
    Ok(ctx)
}

/// Reads one line of an answer from the terminal.
///
/// A secret is read with the terminal in raw mode so the characters never reach
/// the scrollback, and echoed as mask characters so there is still feedback
/// that a key landed. Raw mode is given back whatever the read did.
fn read_answer(label: &str, secret: bool) -> Pair<String> {
    print!("{label}");
    if std::io::stdout().flush().is_err() {
        return (None, Some("cannot write to the terminal".to_owned()));
    }
    if !secret {
        let mut line = String::new();
        return match std::io::stdin().lock().read_line(&mut line) {
            Ok(_) => (Some(line.trim_end().to_owned()), None),
            Err(e) => (None, Some(e.to_string())),
        };
    }
    if let Err(e) = crossterm::terminal::enable_raw_mode() {
        return (None, Some(e.to_string()));
    }
    let answer = read_masked();
    let _ = crossterm::terminal::disable_raw_mode();
    println!();
    answer
}

/// Raw mode hands over bytes, so the answer is gathered as bytes and decoded
/// once at the end. Taking each one for a char would turn a typed accented
/// letter into two wrong ones.
fn read_masked() -> Pair<String> {
    const BACKSPACE: u8 = 0x7f;
    const CTRL_C: u8 = 0x03;
    let mut answer = Vec::new();
    for byte in std::io::stdin().lock().bytes() {
        match byte {
            Ok(b'\r' | b'\n') => break,
            Ok(CTRL_C) => return (None, Some("cancelled".to_owned())),
            Ok(BACKSPACE) => {
                if answer.pop().is_some() {
                    print!("\u{8} \u{8}");
                }
            }
            Ok(byte) => {
                answer.push(byte);
                print!("{SECRET_MASK}");
            }
            Err(e) => return (None, Some(e.to_string())),
        }
        let _ = std::io::stdout().flush();
    }
    (Some(String::from_utf8_lossy(&answer).into_owned()), None)
}

/// The slugs the calling plugin registered, captured when its `maki` global was
/// built. Nothing on the Lua side names a plugin, so a plugin reaches its own
/// credentials and no one else's by construction rather than by a check it
/// could be handed the wrong argument for.
type OwnedSlugs = Arc<Mutex<Vec<String>>>;

fn owned(slugs: &OwnedSlugs, slug: &str) -> LuaResult<()> {
    if slugs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|owned| owned == slug)
    {
        return Ok(());
    }
    Err(mlua::Error::runtime(format!(
        "maki.provider.auth: '{slug}' is not a provider this plugin registered"
    )))
}

/// Register a provider this plugin implements. maki then treats its models like
/// any other provider's: they appear in the model picker, in `/model`, and in
/// `providers.toml` overrides, addressed as `<slug>/<model>`.
///
/// The plugin must declare the hosts it talks to as `net_hosts` under
/// `[permissions]` in its `plugin.toml`. That list is the only set of origins
/// maki will send this provider's credentials to, whatever a hook returns
/// later. Registering with no declared host fails.
///
/// Give exactly one of `codec` (speak a wire protocol maki already knows) or
/// `base` (borrow a native provider whole, including its quirks and its model
/// table). `codec` is the supported choice for a new provider. `base` exists
/// for a provider that needs a specific vendor adapter, and what it inherits
/// changes whenever that provider does.
///
/// Every callback is optional, and a registration with none is a perfectly
/// good static provider. Callbacks run on maki's plugin host, so they may use
/// `maki.net`, `maki.fs` and the rest of the API.
///
/// An option the target cannot honour fails at registration rather than being
/// ignored at request time: `build_body` needs one of the `openai` codecs, and
/// `system_prefix` is refused by the `google` codec, which drops it.
///
/// {spec} fields:
///   `slug` (string) Required. How the provider is addressed: `<slug>/<model>`.
///           Letters, digits, `_` and `-`, starting with a letter or digit, and
///           not a slug a built-in or `providers.toml` already owns.
///   `display_name` (string) Required. Shown in the UI.
///   `codec` (string) `"openai"`, `"openai-responses"`, `"anthropic"` or
///           `"google"`. Mutually exclusive with `base`.
///   `base` (string) A native provider slug to build on, e.g. `"anthropic"`.
///   `base_url` (string) Default origin for requests. Its host must be one of
///           the declared `net_hosts`, and it must be `https` unless it points
///           at loopback.
///   `api_key_env` (string) Environment variable holding an API key. Read at
///           registration and sent as a bearer token when set.
///   `system_prefix` (string) Text prepended to the system prompt.
///   `models` (table) List of model rows. Each row has `prefixes` (list): the
///            row answers for every model id starting with one of them,
///            longest prefix first, and `prefixes[1]` is the canonical id.
///            Also `tier` (`"weak"`, `"medium"`, `"strong"` or
///            `"compaction"`, default `"medium"`), `context_window`,
///            `max_output_tokens`, `supports_thinking`, `supports_vision`,
///            `supports_tool_examples`, `requires_thinking`, `pricing`
///            (`input`, `output`, `cache_write`, `cache_read`, in dollars per
///            million tokens) and `thinking_fields`. The three `supports_`
///            flags have three states: leaving one out asks the codec or base
///            provider, `false` turns the feature off for that model. Read
///            once, here, so this table must not depend on anything asked at
///            runtime.
///   `resolve_auth` (function) `function(purpose)` returning
///            `{ base_url = ..., headers = { ... } }`. Called once, lazily,
///            before the first request, so a provider whose credentials are
///            broken is still listed and fails when it is used. `purpose` is
///            `"resolve"`. Omitting `base_url` keeps the one in force.
///   `refresh_auth` (function) Same shape, called after a 401 with
///            `purpose = "refresh"`. Falls back to `resolve_auth`.
///   `reload_auth` (function) Same shape, called with `purpose = "reload"` to
///            re-read what a `login` wrote. Falls back to `resolve_auth`.
///   `list_models` (function) `function()` returning a list of model rows,
///            for a provider whose catalogue is only known at runtime. Rows
///            carry `id`, `context_window`, `max_output_tokens`, `pricing`,
///            `supports_thinking`, `supports_vision` and `tier`.
///   `build_body` (function) `function(body, model, opts)` returning the
///            request body to send. `opts.thinking` is the effort level as
///            rendered. Only for the `openai` codecs.
///   `map_error` (function) `function(status, message)` returning
///            `{ status = ..., message = ... }`, or nil to keep the original.
///            Those two fields are all it may change: `retry_after` comes from
///            the response header, and whether an error is retryable is
///            derived from the status.
///   `fetch_usage` (function) `function()` returning a usage summary.
///   `login` (function) `function(ctx)`. Having one is what makes the provider
///            an auth target: it then shows up in `maki auth login`. There is
///            no separate flag for it. `ctx` is a table of functions, called
///            with a dot: `ctx.print(text)`,
///            `ctx.prompt({ label = ..., secret = ... })` and
///            `ctx.open_url(url)`.
///   `logout` (function) `function(ctx)`, the `maki auth logout` side.
///
/// @param spec table Provider specification (see above).
/// @return
/// @example
/// maki.provider.register({
///   slug = "acme",
///   display_name = "Acme",
///   codec = "openai",
///   base_url = "https://api.acme.com/v1",
///   api_key_env = "ACME_API_KEY",
///   models = {
///     { prefixes = { "acme-large" }, tier = "strong", context_window = 200000 },
///   },
///   resolve_auth = function()
///     local token = maki.provider.auth.get("acme")
///     return { headers = { Authorization = "Bearer " .. token.access } }
///   end,
/// })
#[lua_fn(guard = Net)]
fn register(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] hosts: NetHosts,
    #[ctx] slugs: OwnedSlugs,
    spec: Table,
) -> LuaResult<()> {
    let hosts = hosts
        .filter(|hosts| !hosts.is_empty())
        .ok_or_else(|| mlua::Error::runtime(NO_NET_HOSTS))?;
    let slug: String = field(&spec, SLUG)?;
    let (requests, release) = host_senders(lua)?;
    let keys = Arc::new(LuaHookKeys {
        plugin,
        slug: slug.clone(),
        keys: hook_keys(lua, &spec)?,
        requests,
        release,
    });

    plugin::register(Registration {
        slug: slug.clone(),
        display_name: field(&spec, DISPLAY_NAME)?,
        codec: codec(&spec)?,
        base: optional(&spec, BASE)?,
        base_url: optional(&spec, BASE_URL)?,
        api_key_env: optional(&spec, API_KEY_ENV)?,
        system_prefix: optional(&spec, SYSTEM_PREFIX)?,
        models: models(lua, &spec)?,
        net_hosts: hosts.to_vec(),
        hooks: ProviderHooks {
            auth: hook(&keys, HookSlot::Auth, Some(HOOK_TIMEOUT)),
            list_models: hook(&keys, HookSlot::ListModels, Some(HOOK_TIMEOUT)),
            build_body: hook(&keys, HookSlot::BuildBody, Some(REQUEST_HOOK_TIMEOUT)),
            map_error: hook(&keys, HookSlot::MapError, Some(REQUEST_HOOK_TIMEOUT)),
            fetch_usage: hook(&keys, HookSlot::FetchUsage, Some(HOOK_TIMEOUT)),
            login: hook(&keys, HookSlot::Login, None),
            logout: hook(&keys, HookSlot::Logout, None),
        },
    })
    .map_err(|e| mlua::Error::runtime(e.to_string()))?;

    slugs.lock().unwrap_or_else(|e| e.into_inner()).push(slug);
    Ok(())
}

fn field(spec: &Table, key: &str) -> LuaResult<String> {
    optional(spec, key)?.ok_or_else(|| {
        mlua::Error::runtime(format!("maki.provider.register: '{key}' must be a string"))
    })
}

fn optional(spec: &Table, key: &str) -> LuaResult<Option<String>> {
    spec.get::<Option<String>>(key).map_err(|_| {
        mlua::Error::runtime(format!("maki.provider.register: '{key}' must be a string"))
    })
}

/// The registry holds protocols, not names, and has no parser: a codec nobody
/// implements is caught here, where the plugin that wrote it can be named.
fn codec(spec: &Table) -> LuaResult<Option<Protocol>> {
    optional(spec, CODEC)?
        .map(|name| {
            name.parse::<Protocol>().map_err(|_| {
                mlua::Error::runtime(format!(
                    "maki.provider.register: unknown codec '{name}' (expected one of openai, \
                     openai-responses, anthropic, google)"
                ))
            })
        })
        .transpose()
}

/// Model rows are static data, read here and never again, so a provider's
/// catalogue costs nothing per request.
fn models(lua: &Lua, spec: &Table) -> LuaResult<Vec<PluginModel>> {
    let Some(table) = spec.get::<Option<Table>>(MODELS).map_err(|_| {
        mlua::Error::runtime("maki.provider.register: 'models' must be a list of tables")
    })?
    else {
        return Ok(Vec::new());
    };
    let json = lua_to_json(lua, &LuaValue::Table(table))?;
    // An empty Lua table reads as an object, and a plugin that curates no models
    // writes one rather than leaving the key out.
    if json.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(Vec::new());
    }
    serde_json::from_value(json).map_err(|e| {
        mlua::Error::runtime(format!(
            "maki.provider.register: invalid 'models' entry: {e}"
        ))
    })
}

fn hook_keys(lua: &Lua, spec: &Table) -> LuaResult<HashMap<&'static str, RegistryKey>> {
    let mut keys = HashMap::new();
    for entry in HookSlot::ALL
        .into_iter()
        .flat_map(|slot| slot.spec().entries)
        .map(|(_, entry)| *entry)
    {
        let Some(func) = spec.get::<Option<Function>>(entry).map_err(|_| {
            mlua::Error::runtime(format!(
                "maki.provider.register: '{entry}' must be a function"
            ))
        })?
        else {
            continue;
        };
        keys.insert(entry, lua.create_registry_value(func)?);
    }
    Ok(keys)
}

/// Read the credentials this plugin stored for one of its providers.
///
/// The value is whatever the plugin wrote. maki keeps the file, the plugin
/// keeps its shape. Nothing is returned when the provider has never stored
/// any, which is how a first `resolve_auth` tells a fresh install from a
/// logged-in one.
///
/// @param slug string A provider slug this plugin registered.
/// @return (table?, string?) The stored credentials, or nil plus an error.
/// @example
/// local creds = maki.provider.auth.get("acme")
/// if creds then print(creds.access_token) end
#[lua_fn]
fn get(lua: &Lua, #[ctx] slugs: OwnedSlugs, slug: String) -> LuaResult<Pair<Table>> {
    owned(&slugs, &slug)?;
    let dir = try_pair!(StateDir::resolve());
    let Some(data) = load_plugin_auth(&dir, &slug) else {
        return Ok((None, None));
    };
    match json_to_lua(lua, &Value::Object(data))? {
        LuaValue::Table(table) => Ok((Some(table), None)),
        _ => Ok((None, None)),
    }
}

/// Every change to the credential store, off the plugin host's thread.
///
/// [`lock_credentials`] waits on a file lock another maki process may hold, and
/// the host is single threaded: waiting for it here would stall every tool
/// call, timer and provider hook in this process behind one plugin's token
/// rotation. The common case takes no file lock at all, because the host
/// already holds this slug while its hook runs and the lock lets it back in,
/// so the write costs one hop off the thread and back.
async fn write_credentials(
    slug: String,
    write: impl FnOnce(&StateDir, &str) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    smol::unblock(move || {
        let dir = StateDir::resolve().map_err(|e| e.to_string())?;
        let _lock = lock_credentials(&dir, &slug);
        write(&dir, &slug)
    })
    .await
}

/// Store credentials for one of this plugin's providers.
///
/// Written to `~/.local/state/maki/auth/plugins/<slug>.json` with mode 0600,
/// replaced in one atomic step, and serialised against other maki processes
/// touching the same provider's credentials. Any JSON-shaped table works, so a
/// plugin can keep a refresh token, an expiry and whatever else its flow needs.
///
/// @param slug string A provider slug this plugin registered.
/// @param credentials table Any table with string keys.
/// @return (boolean?, string?) True, or nil plus an error string.
/// @example
/// local ok, err = maki.provider.auth.set("acme", { access_token = token })
/// if not ok then maki.log.error(err) end
#[lua_fn]
async fn set(
    lua: Lua,
    #[ctx] slugs: OwnedSlugs,
    slug: String,
    credentials: Table,
) -> LuaResult<Pair<bool>> {
    owned(&slugs, &slug)?;
    let Value::Object(data) = lua_to_json(&lua, &LuaValue::Table(credentials))? else {
        return Err(mlua::Error::runtime(
            "maki.provider.auth.set: credentials must be a table with string keys",
        ));
    };
    try_pair!(
        write_credentials(slug, move |dir, slug| {
            save_plugin_auth(dir, slug, &data).map_err(|e| e.to_string())
        })
        .await
    );
    Ok((Some(true), None))
}

/// Forget the credentials stored for one of this plugin's providers.
///
/// @param slug string A provider slug this plugin registered.
/// @return (boolean?, string?) True, or nil plus an error string.
/// @example
/// maki.provider.auth.clear("acme")
#[lua_fn]
async fn clear(_lua: Lua, #[ctx] slugs: OwnedSlugs, slug: String) -> LuaResult<Pair<bool>> {
    owned(&slugs, &slug)?;
    try_pair!(
        write_credentials(slug, |dir, slug| {
            delete_plugin_auth(dir, slug)
                .map(drop)
                .map_err(|e| e.to_string())
        })
        .await
    );
    Ok((Some(true), None))
}

lua_table! {
    /// Credentials for the providers this plugin registered, kept by maki
    /// apart from its own: one file per slug at
    /// `~/.local/state/maki/auth/plugins/<slug>.json`, created with mode 0600,
    /// replaced in one atomic step, and locked against other maki processes
    /// while a refresh writes.
    ///
    /// The stored value is a free-form JSON object. maki owns where it lives
    /// and who may read it, the plugin owns what is in it.
    ///
    /// A plugin can only reach slugs it registered itself.
    ///
    /// ```lua
    /// maki.provider.auth.set("acme", { access_token = tok, expires = when })
    /// local creds = maki.provider.auth.get("acme")
    /// maki.provider.auth.clear("acme")
    /// ```
    "maki.provider.auth" => pub(crate) fn create_auth_table(slugs: OwnedSlugs), AUTH_DOCS [
        get(slugs),
        set(slugs),
        clear(slugs),
    ]
}

lua_table! {
    /// Providers implemented in Lua.
    ///
    /// A registered provider is a first-class one: its models show up in the
    /// picker and in config, its requests go through maki's usual retry,
    /// pricing and usage accounting, and its credentials live where maki keeps
    /// every other provider's.
    ///
    /// Registration happens while the plugin loads, so it belongs at the top
    /// level of the plugin file rather than inside a callback.
    ///
    /// A plugin that registers a provider needs the `net` permission and a
    /// non-empty `net_hosts` list in its `plugin.toml`. Those hosts are the
    /// only origins maki sends the provider's credentials to.
    ///
    /// ```lua
    /// maki.provider.register({
    ///   slug = "acme",
    ///   display_name = "Acme",
    ///   codec = "openai",
    ///   base_url = "https://api.acme.com/v1",
    ///   api_key_env = "ACME_API_KEY",
    ///   models = { { prefixes = { "acme-large" }, tier = "strong" } },
    /// })
    /// ```
    "maki.provider" => pub(crate) fn create_provider_table(perms: &PluginPermissions, plugin: Arc<str>, hosts: NetHosts, slugs: OwnedSlugs), DOCS [
        register(perms, plugin, hosts, slugs),
    ]
}

/// `maki.provider`, with the credential store bound to the calling plugin.
pub(crate) fn create_provider_namespace(
    lua: &Lua,
    permissions: &PluginPermissions,
    plugin: Arc<str>,
) -> LuaResult<Table> {
    let slugs: OwnedSlugs = Arc::default();
    let provider = create_provider_table(
        lua,
        permissions,
        plugin,
        permissions.net_hosts(),
        Arc::clone(&slugs),
    )?;
    provider.set("auth", create_auth_table(lua, slugs)?)?;
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use maki_providers::plugin::AuthPurpose;
    use serde_json::json;

    use super::*;
    use test_case::test_case;

    const SLUG_NAME: &str = "acme";
    const OTHER_SLUG: &str = "rival";
    const NOT_OWNED: &str = "is not a provider this plugin registered";

    fn keys_with(entries: &[&'static str]) -> LuaHookKeys {
        let lua = Lua::new();
        let mut keys = HashMap::new();
        for entry in entries {
            let func = lua.create_function(|_, ()| Ok(())).unwrap();
            keys.insert(*entry, lua.create_registry_value(func).unwrap());
        }
        LuaHookKeys {
            plugin: Arc::from("test"),
            slug: SLUG_NAME.to_owned(),
            keys,
            requests: flume::unbounded().0,
            release: flume::unbounded().0,
        }
    }

    /// The selector on the wire is whatever serde renders [`AuthPurpose`] as,
    /// and the entry table here spells those three names again. A rename on
    /// either side would fall back to the first entry the plugin wrote, which
    /// is the wrong hook rather than an error, so the two tables are pinned to
    /// each other.
    #[test_case(AuthPurpose::Resolve, RESOLVE_AUTH ; "resolve")]
    #[test_case(AuthPurpose::Refresh, REFRESH_AUTH ; "refresh")]
    #[test_case(AuthPurpose::Reload, RELOAD_AUTH ; "reload")]
    fn a_purpose_picks_the_entry_named_after_it(purpose: AuthPurpose, expected: &'static str) {
        let keys = keys_with(&[RESOLVE_AUTH, REFRESH_AUTH, RELOAD_AUTH]);
        let payload = serde_json::to_value(purpose).unwrap();

        let chosen = keys.entry(HookSlot::Auth, &payload).unwrap();

        assert!(std::ptr::eq(chosen, keys.keys.get(expected).unwrap()));
    }

    /// One host-side hook, three Lua entries, and a plugin that reads its
    /// credentials fresh every time writes only `resolve_auth`.
    #[test_case(&[RESOLVE_AUTH], "refresh", Some(RESOLVE_AUTH) ; "refresh_falls_back_to_resolve")]
    #[test_case(&[RESOLVE_AUTH], "reload", Some(RESOLVE_AUTH) ; "reload_falls_back_to_resolve")]
    #[test_case(&[], "resolve", None ; "no_auth_entry_serves_nothing")]
    fn auth_purpose_selects_an_entry(
        entries: &[&'static str],
        purpose: &str,
        expected: Option<&'static str>,
    ) {
        let keys = keys_with(entries);
        let payload = json!(purpose);
        let chosen = keys.entry(HookSlot::Auth, &payload);
        assert_eq!(
            chosen.is_some(),
            expected.is_some(),
            "{entries:?} for {purpose}"
        );
        if let Some(expected) = expected {
            assert!(std::ptr::eq(
                chosen.unwrap(),
                keys.keys.get(expected).unwrap()
            ));
        }
    }

    #[test_case(HookSlot::Login, &[LOGIN], true ; "a_login_entry_serves_the_login_slot")]
    #[test_case(HookSlot::Login, &[LOGOUT], false ; "a_logout_entry_does_not")]
    #[test_case(HookSlot::Auth, &[RELOAD_AUTH], true ; "any_auth_entry_serves_the_auth_slot")]
    fn slots_are_served_by_their_own_entries(
        slot: HookSlot,
        entries: &[&'static str],
        expected: bool,
    ) {
        assert_eq!(keys_with(entries).serves(slot), expected);
    }

    /// `build_body(body, model, opts)`: the fields the signature names arrive
    /// positionally, the rest ride along in the trailing table.
    #[test]
    fn body_input_is_split_into_the_documented_arguments() {
        let lua = Lua::new();
        let payload = json!({ "body": { "stream": true }, "model": "m-1", "thinking": "high" });
        let args = call_args(&lua, HookSlot::BuildBody.spec(), payload).unwrap();
        let args: Vec<LuaValue> = args.into_iter().collect();

        assert_eq!(args.len(), 3);
        let body = args[0].as_table().unwrap();
        assert!(body.get::<bool>("stream").unwrap());
        assert_eq!(args[1].as_string().unwrap().to_string_lossy(), "m-1");
        let opts = args[2].as_table().unwrap();
        assert_eq!(opts.get::<String>("thinking").unwrap(), "high");
    }

    #[test]
    fn a_login_call_is_handed_a_terminal_ctx() {
        let lua = Lua::new();
        let args = call_args(&lua, HookSlot::Login.spec(), Value::Null).unwrap();
        let args: Vec<LuaValue> = args.into_iter().collect();

        assert_eq!(args.len(), 1);
        let ctx = args[0].as_table().unwrap();
        for name in ["print", "prompt", "open_url"] {
            assert!(ctx.get::<Function>(name).is_ok(), "ctx.{name} is missing");
        }
    }

    fn auth_table(lua: &Lua, owns: &[&str]) -> Table {
        let slugs: OwnedSlugs = Arc::new(Mutex::new(
            owns.iter().map(|slug| (*slug).to_owned()).collect(),
        ));
        create_auth_table(lua, slugs).unwrap()
    }

    /// The scoping this namespace exists to enforce: the plugin's own slugs are
    /// captured when its `maki` global is built, so naming someone else's is
    /// refused before anything reaches the credential store. The owned case is
    /// there so the refusal is about ownership and not about the call shape.
    #[test_case("get", OTHER_SLUG, true ; "get_refuses_a_foreign_slug")]
    #[test_case("set", OTHER_SLUG, true ; "set_refuses_a_foreign_slug")]
    #[test_case("clear", OTHER_SLUG, true ; "clear_refuses_a_foreign_slug")]
    #[test_case("get", SLUG_NAME, false ; "an_owned_slug_gets_through")]
    fn auth_calls_are_scoped_to_the_slugs_the_plugin_registered(
        call: &str,
        slug: &str,
        refused: bool,
    ) {
        let lua = Lua::new();
        lua.globals()
            .set("auth", auth_table(&lua, &[SLUG_NAME]))
            .unwrap();

        let error = lua
            .load(format!(r#"auth.{call}("{slug}", {{}})"#))
            .exec()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(error.contains(NOT_OWNED), refused, "{error}");
    }

    #[test_case("openai" ; "openai")]
    #[test_case("openai-responses" ; "openai_responses")]
    #[test_case("anthropic" ; "anthropic")]
    #[test_case("google" ; "google")]
    fn every_documented_codec_parses(name: &str) {
        let lua = Lua::new();
        let spec = lua.create_table().unwrap();
        spec.set(CODEC, name).unwrap();
        assert!(codec(&spec).unwrap().is_some(), "{name}");
    }

    #[test]
    fn an_unknown_codec_is_refused_where_the_plugin_can_be_named() {
        let lua = Lua::new();
        let spec = lua.create_table().unwrap();
        spec.set(CODEC, "grpc").unwrap();
        let error = codec(&spec).unwrap_err().to_string();
        assert!(error.contains("unknown codec 'grpc'"), "{error}");
    }

    #[test]
    fn registering_without_declared_hosts_is_refused() {
        let lua = Lua::new();
        let table = create_provider_table(
            &lua,
            &PluginPermissions::trusted(),
            Arc::from("p"),
            None,
            Arc::default(),
        )
        .unwrap();
        lua.globals().set("provider", table).unwrap();

        let error = lua
            .load(
                r#"provider.register({ slug = "acme", display_name = "Acme", codec = "openai" })"#,
            )
            .exec()
            .unwrap_err()
            .to_string();
        assert!(error.contains("net_hosts"), "{error}");
    }
}
