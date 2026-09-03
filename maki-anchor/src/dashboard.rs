//! The fleet dashboard: one HTML page with the instance list and the session
//! index, plus the link-minting form.

use std::sync::Arc;

use crate::store::{Store, UserRow};

/// Renders the dashboard. No templating engine: the page is small, and
/// `format!` keeps the dependency set tiny.
pub fn render(store: &Arc<Store>, user: Option<&UserRow>) -> (u16, String, Vec<u8>) {
    let instances = store.list_instances().unwrap_or_default();
    let sessions = store.list_sessions().unwrap_or_default();
    let now = crate::store::now_unix();

    let mut body = String::with_capacity(4096);
    body.push_str(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <title>maki anchor</title><style>body{font-family:system-ui;margin:2rem auto;max-width:60rem;padding:0 1rem}\
         table{border-collapse:collapse;width:100%;margin:1rem 0}\
         th,td{border:1px solid #ddd;padding:.4rem .6rem;text-align:left}\
         th{background:#f6f6f6}.pill{padding:.1rem .5rem;border-radius:1rem;background:#eee;font-size:.85rem}\
         .on{background:#d4f7d4}.off{background:#f7d4d4}\
         form{margin:1rem 0;padding:1rem;border:1px solid #ddd;border-radius:.5rem}\
         .user{float:right;color:#666}</style></head><body>",
    );
    if let Some(user) = user {
        let who = user
            .name
            .as_deref()
            .or(user.email.as_deref())
            .unwrap_or("user");
        body.push_str(&format!(
            "<span class=\"user\">{who} (id {}){} · <a href=\"/logout\">log out</a></span>",
            user.id,
            if user.is_admin { " · admin" } else { "" },
        ));
    }
    body.push_str("<h1>maki anchor</h1>");

    body.push_str(
        "<h2>Instances</h2><table><tr><th>Name</th><th>Status</th><th>Last seen</th></tr>",
    );
    for instance in &instances {
        let online = now - instance.last_seen < 90;
        body.push_str(&format!(
            "<tr><td>{}</td><td><span class=\"badge {}\">{}</span></td><td>{}s ago</td></tr>",
            html_escape(&instance.name),
            if online { "on" } else { "off" },
            if online { "online" } else { "offline" },
            now - instance.last_seen,
        ));
    }
    body.push_str("</table>");

    body.push_str("<h2>Sessions</h2><table><tr><th>Instance</th><th>Title</th><th>Model</th><th>Status</th><th>Updated</th></tr>");
    let names: std::collections::HashMap<i64, String> =
        instances.iter().map(|i| (i.id, i.name.clone())).collect();
    for session in &sessions {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}s ago</td></tr>",
            html_escape(
                names
                    .get(&session.instance_id)
                    .map(String::as_str)
                    .unwrap_or("?")
            ),
            html_escape(&session.title),
            html_escape(&session.model),
            html_escape(&session.status),
            now - session.updated_at,
        ));
    }
    body.push_str("</table>");

    body.push_str(
        "<h2>Share a session</h2>\
         <form method=\"get\" action=\"/links\">\
         Instance ID: <input name=\"instance\" type=\"number\" min=\"1\" required> \
         Session ID: <input name=\"session\" placeholder=\"optional\"> \
         Rights: <select name=\"rights\"><option>view</option><option>control</option></select> \
         Hours: <input name=\"hours\" type=\"number\" min=\"1\" value=\"2\"> \
         <button type=\"submit\">Mint link</button></form>",
    );
    body.push_str("</body></html>");

    (200, "text/html".to_string(), body.into_bytes())
}

/// GET /links: mint a link for the named instance and show it. The anchor
/// trusts the browser here for the MVP; grants gate write actions in P2.
pub fn render_link(
    store: &Arc<Store>,
    user: Option<&UserRow>,
    instance: &str,
    session: Option<&str>,
    rights: &str,
    hours: u64,
) -> (u16, String, Vec<u8>) {
    let Some(role) = crate::store::Role::parse(rights) else {
        return (
            400,
            "text/plain".to_string(),
            b"rights must be view or control".to_vec(),
        );
    };
    let Ok(instance_row) = store.instance_by_name(instance) else {
        return (
            404,
            "text/plain".to_string(),
            format!("unknown instance {instance}").into_bytes(),
        );
    };
    let _ = user;
    let token = crate::server::mint_link(
        store,
        instance_row.id,
        session,
        role,
        std::time::Duration::from_secs(hours * 3600),
    );
    let mut body = String::from(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>link</title>\
         <style>body{font-family:system-ui;margin:2rem}</style></head><body>",
    );
    body.push_str(&format!(
        "<h2>Link for {instance} ({})</h2><p><code>{token}</code></p>\
         <p>Open: <a href=\"/{token}/\">/{token}/</a> — expires in {hours}h</p>",
        role.as_str(),
    ));
    body.push_str("<p><a href=\"/\">back</a></p></body></html>");
    (200, "text/html".to_string(), body.into_bytes())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
