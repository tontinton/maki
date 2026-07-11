use maki_providers::AgentError;
use maki_providers::provider::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

pub trait Hooks: Send + Sync {
    fn fire<'a>(
        &'a self,
        event: &'a str,
        payload: Value,
    ) -> BoxFuture<'a, Result<Value, AgentError>>;
}

pub type DynHooks = Arc<dyn Hooks>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Capturing {
        seen: Mutex<Vec<String>>,
    }

    impl Hooks for Capturing {
        fn fire<'a>(
            &'a self,
            event: &'a str,
            payload: Value,
        ) -> BoxFuture<'a, Result<Value, AgentError>> {
            let event = event.to_owned();
            Box::pin(async move {
                self.seen.lock().unwrap().push(event);
                Ok(payload)
            })
        }
    }

    #[test]
    fn fire_returns_payload_unchanged() {
        let hooks = Capturing {
            seen: Mutex::new(Vec::new()),
        };
        let payload = Value::Null;
        let result = smol::block_on(hooks.fire("PostTurn", payload.clone())).unwrap();
        assert_eq!(result, payload);
        assert_eq!(hooks.seen.lock().unwrap().as_slice(), ["PostTurn"]);
    }
}
