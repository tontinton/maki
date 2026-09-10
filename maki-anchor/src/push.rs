//! Web Push delivery to the dashboard's own installed PWA (dashboard.rs) —
//! a second notification channel alongside webhooks.rs, both fired from the
//! exact same "working -> idle" transition (see server.rs's `handle_push`).
//!
//! This is a genuinely different guarantee than the in-page Notification
//! API maki-remote's own session pages use (index.html's `notify()`, which
//! only ever fires while a browser tab is open, connected, and not the
//! focused tab): a real push arrives even with every tab and the installed
//! dashboard fully closed, as long as the OS's push service can reach the
//! device — that's the whole reason to build this instead of just widening
//! the simpler in-page approach to cover more pages.
//!
//! Uses the `web-push` crate for VAPID signing and RFC8291 payload
//! encryption only (`default-features = false` in Cargo.toml drops its own
//! isahc-based HTTP client entirely) — sending goes through the same `ureq`
//! agent webhooks.rs already uses, via `request_builder::build_request`,
//! which hands back a plain `http::Request` this module sends itself.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jwt_simple::algorithms::{ECDSAP256PublicKeyLike, ES256KeyPair};
use web_push::{ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushMessageBuilder};

use crate::store::{PushSubscriptionRow, Store};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const VAPID_KEY_SETTING: &str = "vapid_private_key";

/// The stored VAPID private key (raw bytes, base64url — the same encoding
/// `VapidSignatureBuilder::from_base64` expects directly, no PEM/DER
/// round-trip needed), generating and persisting a fresh one the first time
/// any anchor on this database ever needs it.
///
/// Must never regenerate once any subscription exists: every subscribed
/// browser's `applicationServerKey` is the public half of whichever key was
/// current when it subscribed, so swapping the key silently breaks every
/// existing subscription (the push service starts rejecting them, typically
/// with 401/403) until each device re-subscribes against the new one.
fn vapid_key_b64(store: &Store) -> Option<String> {
    if let Ok(Some(existing)) = store.get_setting(VAPID_KEY_SETTING) {
        return Some(existing);
    }
    let generated = ES256KeyPair::generate();
    let encoded = URL_SAFE_NO_PAD.encode(generated.to_bytes());
    if let Err(err) = store.set_setting(VAPID_KEY_SETTING, &encoded) {
        tracing::warn!(error = %err, "failed to persist a freshly generated VAPID key");
        return None;
    }
    Some(encoded)
}

/// The public half, base64url-encoded as an uncompressed SEC1 point — the
/// exact bytes a browser's `pushManager.subscribe()` call needs as
/// `applicationServerKey`. Served by the `/api/push/vapid-key` route.
pub fn vapid_public_key(store: &Store) -> Option<String> {
    let raw = URL_SAFE_NO_PAD.decode(vapid_key_b64(store)?).ok()?;
    let key = ES256KeyPair::from_bytes(&raw).ok()?;
    let public = key.public_key().public_key().to_bytes_uncompressed();
    Some(URL_SAFE_NO_PAD.encode(public))
}

/// Looks up every device subscribed by a user who can see `instance_id`
/// (`Store::push_subscriptions_for_instance` — an admin, or a user with a
/// `grants` row on it, the same rule the dashboard itself already uses) and
/// delivers to each on its own thread: one slow or dead endpoint must never
/// hold up the tunnel thread that pushes session-index updates, same
/// reasoning as `webhooks::notify_run_finished`.
pub fn notify_run_finished(
    store: &Arc<Store>,
    instance_id: i64,
    instance_name: &str,
    session_title: &str,
) {
    let Some(key_b64) = vapid_key_b64(store) else {
        return;
    };
    let subs = match store.push_subscriptions_for_instance(instance_id) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, instance_id, "push subscription lookup failed");
            return;
        }
    };
    let body = format!("\"{session_title}\" on {instance_name} finished");
    for sub in subs {
        let store = Arc::clone(store);
        let key_b64 = key_b64.clone();
        let body = body.clone();
        let spawned = std::thread::Builder::new()
            .name("push-delivery".into())
            .spawn(move || deliver(&store, &key_b64, sub, &body));
        if let Err(err) = spawned {
            tracing::warn!(error = %err, "could not spawn push delivery thread");
        }
    }
}

fn deliver(store: &Store, vapid_key_b64: &str, sub: PushSubscriptionRow, body: &str) {
    let info = SubscriptionInfo::new(&sub.endpoint, &sub.p256dh, &sub.auth);

    let sig_builder = match VapidSignatureBuilder::from_base64(vapid_key_b64, &info) {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(error = %err, "vapid signature builder failed");
            return;
        }
    };
    let signature = match sig_builder.build() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "vapid signing failed");
            return;
        }
    };

    // The service worker's `push` handler (dashboard.rs's DASHBOARD_SW_JS)
    // expects exactly this shape: `event.data.json()` -> {title, body}.
    let payload = serde_json::json!({ "title": "maki", "body": body }).to_string();
    let mut builder = WebPushMessageBuilder::new(&info);
    builder.set_payload(ContentEncoding::Aes128Gcm, payload.as_bytes());
    builder.set_vapid_signature(signature);
    let message = match builder.build() {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(error = %err, endpoint = %sub.endpoint, "push message build failed");
            return;
        }
    };

    // build_request only ever sets method POST (see request_builder.rs in
    // the web-push crate) — sent through `ureq`, the same HTTP client
    // webhooks.rs already uses, instead of web-push's own isahc-based
    // client (excluded entirely via default-features = false).
    let request = web_push::request_builder::build_request::<Vec<u8>>(message);
    let (parts, body_bytes) = request.into_parts();
    let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
    let mut req = agent.post(&parts.uri.to_string());
    for (name, value) in parts.headers.iter() {
        if let Ok(v) = value.to_str() {
            req = req.set(name.as_str(), v);
        }
    }
    match req.send_bytes(&body_bytes) {
        Ok(_) => {}
        Err(ureq::Error::Status(410, _)) | Err(ureq::Error::Status(404, _)) => {
            // The push service reports the subscription gone or unknown —
            // the browser unregistered, uninstalled, or the endpoint
            // rotated. Nothing left to retry; drop it so it isn't tried
            // again on every future run.
            let _ = store.remove_push_subscription(&sub.endpoint);
        }
        Err(err) => {
            tracing::warn!(error = %err, endpoint = %sub.endpoint, "push delivery failed");
        }
    }
}
