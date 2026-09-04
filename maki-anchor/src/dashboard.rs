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
        r#"<div id="install" style="margin:1.5rem 0;padding:1rem;border:1px solid #ddd;border-radius:.5rem;background:#fafafa">
        <h2 style="margin-top:0">Install on a new host</h2>
        <p>Creates an instance and prints one-liners that install <code>maki</code> and write the anchor config.</p>
        <div style="display:flex;gap:.5rem;flex-wrap:wrap;align-items:end;margin:.5rem 0">
          <label>Instance name <input id="inst-name" placeholder="work-laptop" style="padding:.3rem .5rem;border:1px solid #ccc;border-radius:.3rem"></label>
          <button id="inst-create" style="padding:.4rem .8rem">Create &amp; copy install command</button>
          <span id="inst-status" style="color:#666"></span>
        </div>
        <div style="margin:.5rem 0"><strong>Linux / macOS / WSL / Git Bash</strong><pre id="install-cmd" style="background:#fff;border:1px solid #ddd;padding:.6rem;overflow:auto;white-space:pre-wrap;word-break:break-all"></pre></div>
        <div style="margin:.5rem 0"><strong>Windows PowerShell</strong><pre id="install-cmd-ps" style="background:#fff;border:1px solid #ddd;padding:.6rem;overflow:auto;white-space:pre-wrap;word-break:break-all"></pre></div>
        <small>Or without a pre-minted token:<br><code id="install-cmd-notoken"></code> <button id="copy-notoken" style="padding:.2rem .5rem">Copy</button><br><code id="install-cmd-ps-notoken"></code> <button id="copy-notoken-ps" style="padding:.2rem .5rem">Copy</button></small>
        </div>
        <script>
        (() => {
          const origin = location.origin;
          const cmdEl = document.getElementById('install-cmd');
          const cmdPsEl = document.getElementById('install-cmd-ps');
          const cmdNoTokenEl = document.getElementById('install-cmd-notoken');
          const cmdNoTokenPsEl = document.getElementById('install-cmd-ps-notoken');
          const statusEl = document.getElementById('inst-status');
          const nameEl = document.getElementById('inst-name');
          const btn = document.getElementById('inst-create');
          const notoken = `curl -fsSL ${origin}/install.sh | sh -s -- --anchor "${origin}"`;
          const notokenPs = `irm ${origin}/install.ps1 -OutFile install.ps1; .\\install.ps1 -Anchor "${origin}"`;
          cmdNoTokenEl.textContent = notoken;
          cmdNoTokenPsEl.textContent = notokenPs;
          document.getElementById('copy-notoken').onclick = () => navigator.clipboard.writeText(notoken).then(()=>{statusEl.textContent='copied'; setTimeout(()=>statusEl.textContent='',1500)});
          document.getElementById('copy-notoken-ps').onclick = () => navigator.clipboard.writeText(notokenPs).then(()=>{statusEl.textContent='copied'; setTimeout(()=>statusEl.textContent='',1500)});
          const placeholder = `curl -fsSL ${origin}/install.sh | sh -s -- --anchor "${origin}" --name "NAME" --token "TOKEN"`;
          const placeholderPs = `irm ${origin}/install.ps1 -OutFile install.ps1; .\\install.ps1 -Anchor "${origin}" -Name "NAME" -Token "TOKEN"`;
          cmdEl.textContent = placeholder;
          cmdPsEl.textContent = placeholderPs;
          btn.onclick = async () => {
            const name = nameEl.value.trim();
            if (!name) { statusEl.textContent = 'enter a name'; return; }
            statusEl.textContent = 'creating…';
            try {
              const res = await fetch('/api/instances', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({name})});
              const j = await res.json();
              if (!res.ok) { statusEl.textContent = j.error || 'failed'; return; }
              const cmd = `curl -fsSL ${origin}/install.sh | sh -s -- --anchor "${origin}" --name "${j.name}" --token "${j.token}"`;
              const cmdPs = `irm ${origin}/install.ps1 -OutFile install.ps1; .\\install.ps1 -Anchor "${origin}" -Name "${j.name}" -Token "${j.token}"`;
              cmdEl.textContent = cmd;
              cmdPsEl.textContent = cmdPs;
              await navigator.clipboard.writeText(cmd);
              statusEl.textContent = 'copied Linux command (PS below)';
            } catch (e) { statusEl.textContent = 'error: '+e; }
          };
        })();
        </script>
        "#,
    );

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
        r#"<div id="users" style="margin:1.5rem 0;padding:1rem;border:1px solid #ddd;border-radius:.5rem;background:#fafafa">
        <h2 style="margin-top:0">Users &amp; Access (SSO)</h2>
        <div id="sso-status" style="margin:.5rem 0;color:#666"></div>
        <h3>Users</h3>
        <table id="users-table" style="width:100%"><tr><th>ID</th><th>Sub</th><th>Email</th><th>Name</th><th>Admin</th></tr></table>
        <h3>Grants (per-user per-instance)</h3>
        <table id="grants-table" style="width:100%"><tr><th>User ID</th><th>Instance</th><th>Rights</th></tr></table>
        <form id="grant-form" style="margin:.8rem 0;display:flex;gap:.5rem;flex-wrap:wrap;align-items:end">
          <label>User ID <input id="grant-user" type="number" min="1" required style="padding:.3rem .5rem;border:1px solid #ccc;border-radius:.3rem;width:6rem"></label>
          <label>Instance <input id="grant-instance" placeholder="work-laptop" required style="padding:.3rem .5rem;border:1px solid #ccc;border-radius:.3rem"></label>
          <label>Rights <select id="grant-rights" style="padding:.3rem .5rem"><option value="view">view</option><option value="control">control</option></select></label>
          <button type="submit" style="padding:.4rem .8rem">Set grant</button>
          <button type="button" id="grant-revoke" style="padding:.4rem .8rem">Revoke</button>
          <span id="grant-status" style="color:#666"></span>
        </form>
        <small>Admin only: first user to log in via SSO becomes admin. Grants give <code>view</code> (read) or <code>control</code> (prompt + approve) on an instance. Without SSO, grants are unused.</small>
        </div>
        <script>
        (() => {
          const ssoEl = document.getElementById('sso-status');
          const usersTable = document.getElementById('users-table');
          const grantsTable = document.getElementById('grants-table');
          const grantForm = document.getElementById('grant-form');
          const grantStatus = document.getElementById('grant-status');
          const fetchJson = async (u, opts) => {
            try { const r = await fetch(u, opts); const j = await r.json().catch(()=>({})); return {ok: r.ok, j}; } catch(e){ return {ok:false, j:{error:String(e)}}; }
          };
          const escape = s => String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
          const loadSso = async () => {
            const {ok, j} = await fetchJson('/api/sso');
            if (!ok) { ssoEl.textContent = 'SSO: unknown'; return; }
            ssoEl.textContent = j.enabled ? `SSO enabled — issuer ${j.issuer} origin ${j.origin}` : 'SSO disabled (no OIDC config — all users have access, grants unused)';
          };
          const loadUsers = async () => {
            const {ok, j} = await fetchJson('/api/users');
            if (!ok) return;
            usersTable.querySelectorAll('tr:not(:first-child)').forEach(r=>r.remove());
            for (const u of j) {
              const tr = document.createElement('tr');
              tr.innerHTML = `<td>${u.id}</td><td style="max-width:18rem;overflow:hidden;text-overflow:ellipsis">${escape(u.oidc_sub)}</td><td>${escape(u.email||'')}</td><td>${escape(u.name||'')}</td><td>${u.is_admin?'yes':''}</td>`;
              usersTable.appendChild(tr);
            }
            if (j.length===0) {
              const tr=document.createElement('tr'); tr.innerHTML='<td colspan=5 style="color:#666">no users yet — log in via SSO to appear</td>'; usersTable.appendChild(tr);
            }
          };
          const loadGrants = async () => {
            const {ok, j} = await fetchJson('/api/grants');
            if (!ok) return;
            grantsTable.querySelectorAll('tr:not(:first-child)').forEach(r=>r.remove());
            for (const g of j) {
              const tr=document.createElement('tr'); tr.innerHTML=`<td>${g[0]}</td><td>${escape(g[1])}</td><td>${escape(g[2])}</td>`;
              grantsTable.appendChild(tr);
            }
          };
          grantForm.onsubmit = async (e) => {
            e.preventDefault();
            const user_id = parseInt(document.getElementById('grant-user').value,10);
            const instance = document.getElementById('grant-instance').value.trim();
            const rights = document.getElementById('grant-rights').value;
            grantStatus.textContent='saving…';
            const {ok, j} = await fetchJson('/api/grants', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({user_id, instance, rights})});
            grantStatus.textContent = ok ? 'saved' : (j.error||'failed');
            if (ok) loadGrants();
          };
          document.getElementById('grant-revoke').onclick = async () => {
            const user_id = parseInt(document.getElementById('grant-user').value,10);
            const instance = document.getElementById('grant-instance').value.trim();
            if (!user_id || !instance) { grantStatus.textContent='need user ID and instance'; return; }
            grantStatus.textContent='revoking…';
            const {ok, j} = await fetchJson('/api/grants/revoke', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({user_id, instance})});
            grantStatus.textContent = ok ? 'revoked' : (j.error||'failed');
            if (ok) loadGrants();
          };
          loadSso(); loadUsers(); loadGrants();
        })();
        </script>
        "#,
    );

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
