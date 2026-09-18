//! `maki.model`. The event loop owns the model slot and the per-session
//! request options, so most calls round-trip to it; `info` resolves
//! locally and works without a UI.

use maki_agent::completion::{
    self, CompletionRequest, DEFAULT_COMPLETE_MAX_OUTPUT_TOKENS, DEFAULT_COMPLETE_TIMEOUT_MS,
};
use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::{Message, Model, Timeouts, TokenUsage};
use mlua::{Error as LuaError, Lua, Result as LuaResult, Table, Value};
use serde_json::json;

use crate::api::util::command::{ModelRequest, UiAction, ui_json_roundtrip, ui_send};
use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, err_pair};

const SET_ARG_ERR: &str = "expected a model spec string or an options table";
const MODEL_REQUIRED_ERR: &str = "complete: 'model' must be a provider/model-id spec";
const NO_INPUT_ERR: &str = "complete: give it a 'prompt' string or a 'messages' list";

/// What one [`complete`] call spent, on its way to whoever bills this
/// session. Hosts install a sink with `EventHandle::install_model_spend`;
/// the interactive UI takes it over its own action channel instead.
#[derive(Debug, Clone)]
pub struct ModelSpend {
    /// Resolved `provider/id`, so a per-model breakdown can name it.
    pub model: String,
    pub usage: TokenUsage,
    /// What the account pays, and the un-subsidised list price.
    pub cost: Option<f64>,
    pub list_cost: Option<f64>,
}

pub type ModelSpendFn = Box<dyn Fn(ModelSpend) + Send + Sync + 'static>;

pub struct ModelSpendSlot(pub ModelSpendFn);

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: ModelRequest,
) -> LuaResult<Pair<Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Model {
        req,
        reply_tx,
    })
    .await
}

fn model_info_table(lua: &Lua, model: &Model) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    tbl.set("spec", model.spec())?;
    tbl.set("id", model.id.clone())?;
    tbl.set("provider", model.provider.to_string())?;
    tbl.set("provider_display", model.provider_display_name())?;
    tbl.set("tier", model.tier.to_string())?;
    tbl.set("context_window", model.context_window)?;
    if let Some(max) = model.max_output_tokens {
        tbl.set("max_output_tokens", max)?;
    }
    // `None` writes nil, which is the third state: a plugin reads false as
    // "metered" and nil as "we never learned a price".
    tbl.set("free", model.free())?;
    // Top level, not under `pricing`: the subsidy is a property of the route
    // to the provider, and `pricing` is absent on exactly the subsidised
    // models whose rates never resolved, which is the case it exists for.
    tbl.set("subsidised_by", model.subsidy_source())?;
    if !model.pricing.is_zero() {
        let pricing = lua.create_table()?;
        pricing.set("input", model.pricing.input)?;
        pricing.set("output", model.pricing.output)?;
        pricing.set("cache_write", model.pricing.cache_write)?;
        pricing.set("cache_read", model.pricing.cache_read)?;
        if let Some(fast) = &model.pricing.fast {
            let f = lua.create_table()?;
            f.set("input", fast.input)?;
            f.set("output", fast.output)?;
            pricing.set("fast", f)?;
        }
        tbl.set("pricing", pricing)?;
    }
    Ok(tbl)
}

/// Resolve a model spec to everything maki knows about it: identity, tier,
/// context window, and the price table the session would be billed by --
/// including rates resolved from provider config or the bundled catalog
/// (e.g. subsidised custom providers), which the provider's own /v1/models
/// endpoint may never report. Purely local -- no UI round-trip, no network
/// -- so it also works from slash commands and headless embeddings.
///
/// @param spec string `"provider/id"`, as listed by `available()`.
/// @return (table|nil, string|nil) `{spec, id, provider, provider_display,
///   tier, subsidised_by?, context_window, max_output_tokens?, free?,
///   pricing?}`, or nil and an error.
///
///   `free` has three states: `true` when the model is known to cost nothing,
///   `false` when it is metered, and nil when no source ever quoted a rate.
///   Check `~= nil` before trusting it.
///
///   `subsidised_by` names the subscription prepaying this provider (billed
///   cost is $0, the rates are the list-price reference). It sits at the top
///   level because it holds whether or not rates resolved.
///
///   `pricing` is present only when rates are known:
///   `{input, output, cache_write, cache_read}` in USD per million tokens,
///   plus optional `fast = {input, output}`.
/// @example
/// local m, err = maki.model.info("anthropic/claude-opus-4-6")
/// if m and m.subsidised_by then print(m.subsidised_by, m.pricing.input) end
#[lua_fn]
fn info(lua: &Lua, spec: String) -> LuaResult<Pair<Table>> {
    match Model::from_spec(&spec) {
        Ok(model) => Ok((Some(model_info_table(lua, &model)?), None)),
        Err(e) => Ok(err_pair(e)),
    }
}

/// Reads the focused session's model, thinking level, and fast mode.
/// `thinking` comes back in the spelling `set` accepts, so a table from here
/// can go straight back in.
///
/// `thinking_options` is every thinking value this model accepts, cheapest
/// first: `{name, tokens?}` per row, where `tokens` is the budget maki would
/// send for that row and is absent on `off` and `adaptive`. It is empty exactly
/// when `supports_thinking` is false, so a picker can render the ladder from it
/// without knowing the levels.
///
/// @return (table|nil, string|nil) `{spec, id, provider, thinking,
///   thinking_options, fast, supports_thinking, supports_fast}`, or nil and an
///   error.
/// @example
/// local m = maki.model.get()
/// if m.spec ~= "anthropic/claude-opus-4-6" then ... end
/// for _, option in ipairs(m.thinking_options) do
///   print(option.name, option.tokens)
/// end
#[lua_fn]
async fn get(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, ModelRequest::Get).await
}

/// Lists the model specs you can switch to: what the providers you are logged
/// into offer, minus what your model policy blocks. The list fills in the
/// background at startup, so right after launch it can still be empty.
///
/// @return (table|nil, string|nil) Array of `"provider/id"` specs, or nil and an error.
/// @example
/// local specs = maki.model.available()
#[lua_fn]
async fn available(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, ModelRequest::Available).await
}

/// Switches the focused session's model, thinking level, or fast mode. Fields
/// you leave out stay as they are, so this doubles as a thinking-only switch.
/// Answers with the new state, in the same shape `get` returns.
///
/// @param opts string|table A model spec, or a table with any of:
///   `spec` (string) `"provider/id"`, as listed by `available()`;
///   `thinking` (string|number) `"off"`, `"adaptive"`, an effort level
///   (`"minimal"` to `"max"`), a token budget, or `""` to toggle it on and off;
///   `fast` (boolean) Anthropic fast mode.
/// @return (table|nil, string|nil) The new state, or nil and an error.
/// @example
/// maki.model.set("anthropic/claude-opus-4-6")
/// maki.model.set({ spec = "zai/glm-5", thinking = "high" })
/// maki.keymap.set("n", "<M-t>", function() maki.model.set({ thinking = "" }) end)
#[lua_fn]
async fn set(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Value,
) -> LuaResult<Pair<Value>> {
    let req = match opts {
        Value::String(spec) => ModelRequest::Set {
            spec: Some(spec.to_str()?.to_owned()),
            thinking: None,
            fast: None,
        },
        Value::Table(opts) => ModelRequest::Set {
            spec: opts.get("spec")?,
            thinking: opts.get("thinking")?,
            fast: opts.get("fast")?,
        },
        other => {
            return Err(LuaError::runtime(format!(
                "{SET_ARG_ERR}, got {}",
                other.type_name()
            )));
        }
    };
    roundtrip(lua, tx, req).await
}

/// Reads the message list out of the options table. A bare `prompt` is the
/// one-liner; `messages` is the same thing with room for a prior exchange.
fn messages_from(opts: &Table) -> Result<Vec<Message>, String> {
    if let Some(rows) = opts
        .get::<Option<Vec<Table>>>("messages")
        .map_err(|_| "complete: 'messages' must be a list of {role, content} tables".to_owned())?
    {
        return rows
            .iter()
            .map(|row| {
                let role: String = row
                    .get("role")
                    .map_err(|_| "complete: message 'role' must be a string".to_owned())?;
                let content: String = row
                    .get("content")
                    .map_err(|_| "complete: message 'content' must be a string".to_owned())?;
                completion::message(&role, content)
            })
            .collect();
    }
    let prompt: Option<String> = opts
        .get("prompt")
        .map_err(|_| "complete: 'prompt' must be a string".to_owned())?;
    match prompt {
        Some(prompt) => Ok(vec![completion::message("user", prompt)?]),
        None => Err(NO_INPUT_ERR.to_owned()),
    }
}

/// Hands the spend to whoever bills this session: a host sink when one is
/// installed (`maki -p`, sdk mode), the UI's action channel otherwise. A
/// call with neither is nobody's bill, so it is dropped rather than raised:
/// the caller already has the answer it asked for.
fn report_spend(lua: &Lua, tx: Option<&flume::Sender<UiAction>>, spend: ModelSpend) {
    if let Some(slot) = lua.app_data_ref::<ModelSpendSlot>() {
        (slot.0)(spend);
        return;
    }
    let _ = ui_send(tx, UiAction::ModelSpend(Box::new(spend)));
}

/// Asks a model one question and hands back what it said. No system prompt
/// of maki's, no tools, no turn: this is the plain call a plugin needs to
/// classify, summarise, or judge something on its own.
///
/// The tokens are billed to the session like any other model call, so the
/// spend shows up in the TUI status line, in `maki -p`'s result, and in sdk
/// mode's usage. A reviewer firing on every tool call is exactly where an
/// unnoticed bill grows, so it is never silent.
///
/// The call is answered on the Lua thread, so it works with or without an
/// interactive UI.
///
/// @param opts table Options:
///   `model` (string) Required. `"provider/model-id"`.
///   `prompt` (string) The single user message to send.
///   `messages` (table) Instead of `prompt`: `{role, content}` rows, where
///   `role` is `"user"` or `"assistant"`.
///   `system` (string) System prompt. You own every word of it.
///   `max_output_tokens` (integer) Output ceiling, default 1024. Raise it for
///   models that emit reasoning tokens whatever you ask: one that spends its
///   whole budget thinking answers with nothing and still bills.
///   `timeout_ms` (integer) How long to wait, default 30000.
/// @return (table|nil, string|nil) `{text, model, usage, cost, list_cost}`,
///   or nil and an error. `usage` carries the four token counts.
/// @example
/// local answer, err = maki.model.complete({
///   model = "anthropic/claude-haiku-4-5-20251001",
///   system = "Answer with one word.",
///   prompt = "Is `rm -rf /` safe?",
///   max_output_tokens = 16,
/// })
#[lua_fn]
async fn complete(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Table,
) -> LuaResult<Pair<Value>> {
    let spec: Option<String> = opts
        .get("model")
        .map_err(|_| LuaError::runtime(MODEL_REQUIRED_ERR))?;
    let Some(spec) = spec.filter(|s| s.contains('/')) else {
        return Ok(err_pair(MODEL_REQUIRED_ERR));
    };
    let messages = match messages_from(&opts) {
        Ok(messages) => messages,
        Err(e) => return Ok(err_pair(e)),
    };
    let request = CompletionRequest {
        spec,
        system: opts.get("system").unwrap_or_default(),
        messages,
        max_output_tokens: opts
            .get::<Option<u32>>("max_output_tokens")
            .unwrap_or_default()
            .filter(|budget| *budget > 0)
            .unwrap_or(DEFAULT_COMPLETE_MAX_OUTPUT_TOKENS),
        timeout_ms: opts
            .get::<Option<u64>>("timeout_ms")
            .unwrap_or_default()
            .filter(|ms| *ms > 0)
            .unwrap_or(DEFAULT_COMPLETE_TIMEOUT_MS),
    };
    let done = match completion::complete(request, Timeouts::default()).await {
        Ok(done) => done,
        Err(e) => return Ok(err_pair(e)),
    };
    report_spend(
        &lua,
        tx.as_ref(),
        ModelSpend {
            model: done.model.clone(),
            usage: done.usage,
            cost: done.billed_cost,
            list_cost: done.list_cost,
        },
    );
    let value = json!({
        "text": done.text,
        "model": done.model,
        "usage": done.usage,
        "cost": done.billed_cost,
        "list_cost": done.list_cost,
    });
    Ok((Some(json_to_lua(&lua, &value)?), None))
}

lua_table! {
    /// The model behind the focused session. Good for a keybind that flips
    /// between your two go-to models, or lifts thinking for one hard question.
    /// `get`, `available` and `set` return `nil, "no interactive UI attached"`
    /// without one; `complete` needs no UI, it calls a model itself.
    "maki.model" => pub(crate) fn create_model_table(tx: Option<flume::Sender<UiAction>>),
    DOCS [get(tx), available(tx), set(tx), info(), complete(tx)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, UI_DROPPED_ERR, UiReply};
    use crate::api::util::convert::lua_to_json;
    use serde_json::{Value as Json, json};
    use test_case::test_case;

    const SPEC: &str = "anthropic/claude-opus-4-6";
    const THINKING: &str = "high";
    const UI_FAILURE: &str = "Model is not allowed by policy: anthropic/claude-opus-4-6";

    fn lua_with_model(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_model_table(&lua, tx).unwrap();
        lua.globals().set("model", t).unwrap();
        lua
    }

    /// The receiver is dropped up front, so the request cannot even leave.
    fn closed_ui() -> Lua {
        let (tx, rx) = flume::unbounded::<UiAction>();
        drop(rx);
        lua_with_model(Some(tx))
    }

    /// Answering `None` drops the reply channel, like a UI that took the
    /// request and then vanished.
    fn stub_ui(answer: fn(ModelRequest) -> Option<UiReply>) -> Lua {
        let (tx, rx) = flume::unbounded::<UiAction>();
        std::thread::spawn(move || {
            while let Ok(UiAction::Model { req, reply_tx }) = rx.recv() {
                if let Some(reply) = answer(req) {
                    let _ = reply_tx.send(reply);
                }
            }
        });
        lua_with_model(Some(tx))
    }

    /// Echoes the request back, so a test can assert on what the UI would have
    /// acted on. Fields left out stay `nil` on the way back.
    fn echo(req: ModelRequest) -> Option<UiReply> {
        Some(Ok(match req {
            ModelRequest::Get => json!({ "spec": SPEC, "thinking": THINKING, "fast": true }),
            ModelRequest::Available => json!([SPEC]),
            ModelRequest::Set {
                spec,
                thinking,
                fast,
            } => json!({ "spec": spec, "thinking": thinking, "fast": fast }),
        }))
    }

    /// The value comes back as JSON so assertions outlive the Lua state.
    fn eval(lua: &Lua, script: &str) -> (Json, Option<String>) {
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(script).eval_async()).unwrap();
        (lua_to_json(lua, &val).unwrap(), err)
    }

    /// `set` forwards only the fields it was given, so whatever you leave out
    /// the UI leaves alone. `false` and `""` are values though, not omissions:
    /// `""` is the thinking toggle. The last case is the documented loop, `get`
    /// straight back into `set`, read-only extras and all.
    #[test_case("return model.get()", json!({ "spec": SPEC, "thinking": THINKING, "fast": true }) ; "get")]
    #[test_case("return model.available()", json!([SPEC]) ; "available")]
    #[test_case("return model.set('anthropic/claude-opus-4-6')", json!({ "spec": SPEC }) ; "set_bare_spec_string")]
    #[test_case("return model.set({ thinking = 8192, fast = true })", json!({ "thinking": "8192", "fast": true }) ; "set_table_without_spec")]
    #[test_case("return model.set({ thinking = '', fast = false })", json!({ "thinking": "", "fast": false }) ; "set_empty_thinking_and_false_fast")]
    #[test_case("local m = model.get() return model.set(m)", json!({ "spec": SPEC, "thinking": THINKING, "fast": true }) ; "set_fed_by_get")]
    fn requests_cross_the_channel_and_answer_with_the_new_state(script: &str, expected: Json) {
        assert_eq!(eval(&stub_ui(echo), script), (expected, None));
    }

    /// Every way of not getting an answer lands in the error slot, instead of
    /// throwing or parking forever.
    #[test_case(lua_with_model(None), NO_UI_ERR ; "no_ui_attached")]
    #[test_case(closed_ui(), NO_UI_ERR ; "event_loop_closed")]
    #[test_case(stub_ui(|_| None), UI_DROPPED_ERR ; "reply_channel_dropped")]
    #[test_case(stub_ui(|_| Some(Err(UI_FAILURE.to_owned()))), UI_FAILURE ; "ui_refused")]
    fn unanswered_request_returns_an_error_pair(lua: Lua, expected: &str) {
        assert_eq!(
            eval(&lua, "return model.get()"),
            (Json::Null, Some(expected.to_owned()))
        );
    }

    /// `info` resolves locally: no UI required, and a builtin model answers
    /// with its identity and price table.
    #[test]
    fn info_resolves_a_builtin_model_without_a_ui() {
        let lua = lua_with_model(None);
        let (val, err) = eval(&lua, "return model.info('deepseek/deepseek-v4-pro')");
        assert_eq!(err, None);
        assert_eq!(val["spec"], json!("deepseek/deepseek-v4-pro"));
        assert_eq!(val["provider"], json!("deepseek"));
        assert!(val["context_window"].as_u64().unwrap() > 0);
        assert!(val["pricing"]["input"].as_f64().unwrap() > 0.0);
        assert!(val["pricing"]["output"].as_f64().unwrap() > 0.0);
        assert_eq!(val["subsidised_by"], Json::Null);
        assert_eq!(val["free"], json!(false));
    }

    /// The three states have to read apart: a known `$0`, a metered model, and
    /// one nothing ever quoted a price for.
    #[test_case("zai/glm-4.7-flash",       json!(true)  ; "builtin_zero_priced_is_free")]
    #[test_case("deepseek/deepseek-v4-pro", json!(false) ; "metered_is_not_free")]
    #[test_case("deepseek/my-custom-model", Json::Null   ; "no_price_table_is_unknown")]
    fn info_free_separates_zero_from_unknown(spec: &str, expected: Json) {
        let lua = lua_with_model(None);
        let (val, err) = eval(&lua, &format!("return model.info('{spec}')"));
        assert_eq!(err, None);
        assert_eq!(val["free"], expected);
    }

    /// An unresolvable spec answers `(nil, err)` instead of throwing.
    #[test]
    fn info_unknown_provider_returns_an_error_pair() {
        let lua = lua_with_model(None);
        let (val, err) = eval(&lua, "return model.info('no-such-provider/nope')");
        assert_eq!(val, Json::Null);
        assert!(err.is_some());
    }

    /// The subsidy sits at the top level so pickers can render "$0 (Max)"
    /// rows without re-deriving it.
    #[test]
    fn info_table_carries_the_subsidy_source() {
        let lua = Lua::new();
        let mut model = Model::from_spec("deepseek/deepseek-v4-pro").unwrap();
        model.subsidised_by = Some(std::sync::Arc::from("Max"));
        let tbl = model_info_table(&lua, &model).unwrap();
        let json = lua_to_json(&lua, &Value::Table(tbl)).unwrap();
        assert_eq!(json["subsidised_by"], json!("Max"));
    }

    /// The case the feature exists for: a subsidised provider whose rates
    /// never resolved. Nesting the source under `pricing` hid it exactly
    /// here, because `pricing` is omitted at zero rates.
    #[test]
    fn info_table_reports_a_subsidy_with_no_price_table() {
        let lua = Lua::new();
        let mut model = Model::from_spec("deepseek/my-custom-model").unwrap();
        model.subsidised_by = Some(std::sync::Arc::from("Max"));
        let tbl = model_info_table(&lua, &model).unwrap();
        let json = lua_to_json(&lua, &Value::Table(tbl)).unwrap();
        assert_eq!(json["pricing"], Json::Null);
        assert_eq!(json["subsidised_by"], json!("Max"));
    }

    /// A non-spec argument is a programmer error, so it throws instead of
    /// answering with a pair.
    #[test_case("return model.set(42)" ; "number")]
    #[test_case("return model.set()" ; "no_argument")]
    #[test_case("return model.set(nil)" ; "explicit_nil")]
    fn set_throws_on_a_non_spec_argument(script: &str) {
        let lua = lua_with_model(None);
        let err = smol::block_on(lua.load(script).eval_async::<Value>()).unwrap_err();
        assert!(err.to_string().contains(SET_ARG_ERR));
    }

    fn spend() -> ModelSpend {
        ModelSpend {
            model: SPEC.to_owned(),
            usage: TokenUsage {
                input: 40,
                output: 2,
                ..Default::default()
            },
            cost: Some(0.25),
            list_cost: Some(0.5),
        }
    }

    /// The ledger attribution every driver hangs off: a headless host
    /// installs a sink and the spend goes straight to it, no UI needed.
    #[test]
    fn an_installed_sink_takes_the_spend() {
        let lua = lua_with_model(None);
        let (tx, rx) = flume::unbounded();
        lua.set_app_data(ModelSpendSlot(Box::new(move |spend| {
            let _ = tx.send(spend);
        })));

        report_spend(&lua, None, spend());

        let billed = rx.try_recv().expect("the sink is billed");
        assert_eq!(billed.model, SPEC);
        assert_eq!(billed.usage.input, 40);
        assert_eq!(billed.cost, Some(0.25));
    }

    /// The interactive UI installs no sink: it owns the session totals, so
    /// the spend rides its action channel instead, the same way every other
    /// `maki.model` call reaches it.
    #[test]
    fn without_a_sink_the_spend_goes_to_the_ui() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_model(Some(tx.clone()));

        report_spend(&lua, Some(&tx), spend());

        let Ok(UiAction::ModelSpend(billed)) = rx.try_recv() else {
            panic!("expected the spend to cross the UI channel");
        };
        assert_eq!(billed.model, SPEC);
        assert_eq!(billed.usage.output, 2);
    }

    /// A call maki cannot make is an error pair, not a throw and not a
    /// silent nothing: the caller is mid-verdict and needs to know.
    #[test_case("return model.complete({ prompt = 'hi' })", MODEL_REQUIRED_ERR ; "no_model")]
    #[test_case("return model.complete({ model = 'haiku', prompt = 'hi' })", MODEL_REQUIRED_ERR ; "model_is_not_a_spec")]
    #[test_case("return model.complete({ model = 'p/m' })", NO_INPUT_ERR ; "nothing_to_say")]
    #[test_case("return model.complete({ model = 'p/m', messages = { { role = 'system', content = 'x' } } })", "unknown message role 'system'" ; "unknown_role")]
    fn complete_answers_bad_options_with_an_error_pair(script: &str, expected: &str) {
        let lua = lua_with_model(None);
        let (value, error) = eval(&lua, script);
        assert_eq!(value, Json::Null);
        assert!(
            error.as_deref().is_some_and(|e| e.contains(expected)),
            "got {error:?}"
        );
    }

    /// `prompt` is the one-liner and `messages` the long form of the same
    /// thing, so both have to land on the same message list.
    #[test]
    fn prompt_and_messages_build_the_same_request() {
        let lua = Lua::new();
        let bare: Table = lua
            .load("return { model = 'p/m', prompt = 'why' }")
            .eval()
            .unwrap();
        let rows: Table = lua
            .load("return { model = 'p/m', messages = { { role = 'user', content = 'why' } } }")
            .eval()
            .unwrap();
        let from_prompt = messages_from(&bare).unwrap();
        let from_rows = messages_from(&rows).unwrap();
        assert_eq!(from_prompt.len(), 1);
        assert_eq!(
            from_prompt[0].first_text_content(),
            from_rows[0].first_text_content()
        );
        assert!(matches!(
            (&from_prompt[0].role, &from_rows[0].role),
            (maki_providers::Role::User, maki_providers::Role::User)
        ));
    }
}
