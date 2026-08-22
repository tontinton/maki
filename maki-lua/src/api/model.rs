//! `maki.model`. The event loop owns the model slot and the per-session
//! request options, so every call has to round-trip to it.

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Error as LuaError, Lua, Result as LuaResult, Value};

use crate::api::util::command::{ModelRequest, UiAction, ui_json_roundtrip};
use crate::api::util::pair::Pair;

const SET_ARG_ERR: &str = "expected a model spec string or an options table";

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

/// Reads the focused session's model, thinking level, and fast mode.
/// `thinking` comes back in the spelling `set` accepts, so a table from here
/// can go straight back in.
///
/// @return (table|nil, string|nil) `{spec, id, provider, thinking, fast,
///   supports_thinking, supports_fast}`, or nil and an error.
/// @example
/// local m = maki.model.get()
/// if m.spec ~= "anthropic/claude-opus-4-6" then ... end
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

lua_table! {
    /// The model behind the focused session. Good for a keybind that flips
    /// between your two go-to models, or lifts thinking for one hard question.
    /// Without an interactive UI every function returns
    /// `nil, "no interactive UI attached"`.
    "maki.model" => pub(crate) fn create_model_table(tx: Option<flume::Sender<UiAction>>),
    DOCS [get(tx), available(tx), set(tx)]
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
}
