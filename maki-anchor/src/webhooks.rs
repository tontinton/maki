//! Fire-and-forget delivery to configured webhook endpoints (Slack, Discord,
//! ntfy, or a generic JSON POST) when a session's status flips from
//! "working" to "idle" — the same "the run stopped" signal the web UI's own
//! in-page notifications already act on, just server-side, so it fires even
//! when nobody has a browser tab open anywhere.

use std::sync::Arc;
use std::time::Duration;

use crate::store::{Store, WebhookRow};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Looks up every webhook configured for `instance_id` (instance-scoped
/// plus every unscoped one) and delivers to each on its own thread, so one
/// slow or dead endpoint can never hold up the tunnel thread that pushes
/// session-index updates.
pub fn notify_run_finished(
    store: &Arc<Store>,
    instance_id: i64,
    instance_name: &str,
    session_title: &str,
) {
    let hooks = match store.webhooks_for_instance(instance_id) {
        Ok(h) => h,
        Err(err) => {
            tracing::warn!(error = %err, instance_id, "webhook lookup failed");
            return;
        }
    };
    for hook in hooks {
        let instance_name = instance_name.to_owned();
        let session_title = session_title.to_owned();
        let spawned = std::thread::Builder::new()
            .name("webhook-delivery".into())
            .spawn(move || deliver(&hook, &instance_name, &session_title));
        if let Err(err) = spawned {
            tracing::warn!(error = %err, "could not spawn webhook delivery thread");
        }
    }
}

fn deliver(hook: &WebhookRow, instance_name: &str, session_title: &str) {
    let message = format!("maki: \"{session_title}\" on {instance_name} finished");
    let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
    let result = match hook.kind.as_str() {
        "slack" => post_json(&agent, &hook.url, &serde_json::json!({ "text": message })),
        "discord" => post_json(
            &agent,
            &hook.url,
            &serde_json::json!({ "content": message }),
        ),
        "ntfy" => agent
            .post(&hook.url)
            .set("Title", "maki")
            .send_string(&message)
            .map(drop)
            .map_err(Box::new),
        _ => post_json(
            &agent,
            &hook.url,
            &serde_json::json!({
                "event": "run_finished",
                "instance": instance_name,
                "session": session_title,
                "message": message,
            }),
        ),
    };
    if let Err(err) = result {
        tracing::warn!(error = %err, url = %hook.url, kind = %hook.kind, "webhook delivery failed");
    }
}

fn post_json(
    agent: &ureq::Agent,
    url: &str,
    body: &serde_json::Value,
) -> Result<(), Box<ureq::Error>> {
    agent
        .post(url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map(drop)
        .map_err(Box::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_run_finished_is_a_noop_when_nothing_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let instance_id = store.create_instance("host", "hash").unwrap();
        // No webhooks configured: must not panic, spawn nothing that errors
        // loudly, or block — this is the common case on every anchor.
        notify_run_finished(&store, instance_id, "host", "a session");
    }

    #[test]
    fn webhooks_for_instance_includes_scoped_and_unscoped_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let mine = store.create_instance("mine", "hash").unwrap();
        let theirs = store.create_instance("theirs", "hash2").unwrap();
        store
            .add_webhook(None, "generic", "https://example.com/all")
            .unwrap();
        store
            .add_webhook(Some(mine), "slack", "https://example.com/mine")
            .unwrap();
        store
            .add_webhook(Some(theirs), "slack", "https://example.com/theirs")
            .unwrap();

        let hooks = store.webhooks_for_instance(mine).unwrap();
        let urls: Vec<&str> = hooks.iter().map(|h| h.url.as_str()).collect();
        assert!(urls.contains(&"https://example.com/all"));
        assert!(urls.contains(&"https://example.com/mine"));
        assert!(
            !urls.contains(&"https://example.com/theirs"),
            "must not fire another instance's scoped webhook: {urls:?}"
        );
    }
}
