use std::time::Duration;

use maki_providers::provider::BoxFuture;

use crate::AgentError;

pub const REQUEST_STAGE: &str = "request";
pub const RESPONSE_END_STAGE: &str = "response_end";

pub(crate) const PROVIDER_HOOKS_TIMEOUT: Duration = Duration::from_secs(5);

pub trait ProviderHookSink: Send + Sync {
    fn run_hooks<'a>(
        &'a self,
        stage: &'a str,
        slug: &'a str,
        ctx: serde_json::Value,
    ) -> BoxFuture<'a, Result<serde_json::Value, AgentError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopSink;

    impl ProviderHookSink for NoopSink {
        fn run_hooks<'a>(
            &'a self,
            _stage: &'a str,
            _slug: &'a str,
            ctx: serde_json::Value,
        ) -> BoxFuture<'a, Result<serde_json::Value, AgentError>> {
            Box::pin(async move { Ok(ctx) })
        }
    }

    #[test]
    fn run_hooks_returns_input_unchanged() {
        const STAGE: &str = REQUEST_STAGE;
        const SLUG: &str = "test-provider";
        smol::block_on(async {
            let sink = NoopSink;
            let ctx = serde_json::json!({ "ping": 1 });
            let out = sink.run_hooks(STAGE, SLUG, ctx).await.unwrap();
            assert_eq!(out, serde_json::json!({ "ping": 1 }));
        });
    }
}
