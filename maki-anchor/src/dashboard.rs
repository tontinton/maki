use std::sync::Arc;

use crate::store::{Store, UserRow};

fn base_style() -> &'static str {
    r#"*{box-sizing:border-box}body{font-family:system-ui,-apple-system,Segoe UI,Roboto,Ubuntu,sans-serif;margin:0;background:#f8fafc;color:#0f172a;line-height:1.5}
a{color:#2563eb;text-decoration:none}a:hover{text-decoration:underline}
header{position:sticky;top:0;z-index:10;display:flex;align-items:center;gap:.8rem;flex-wrap:wrap;padding:.7rem 1.2rem;background:#fff;border-bottom:1px solid #e2e8f0}
header h1{font-size:1.05rem;margin:0;font-weight:700;letter-spacing:-.02em}
header .nav{margin-left:.5rem;display:flex;gap:.4rem}
header .nav a{padding:.3rem .6rem;border-radius:999px;font-size:.85rem}
header .nav a.active{background:#0f172a;color:#fff}
.user{margin-left:auto;color:#64748b;font-size:.85rem}
.card{margin:1.2rem auto;max-width:64rem;padding:1.1rem 1.2rem;background:#fff;border:1px solid #e2e8f0;border-radius:12px;box-shadow:0 1px 2px rgba(0,0,0,.04)}
.card h2{margin:.2rem 0 .6rem;font-size:1rem}
table{border-collapse:collapse;width:100%;margin:.6rem 0}
th,td{border:1px solid #e2e8f0;padding:.45rem .6rem;text-align:left;font-size:.9rem}
th{background:#f8fafc;font-weight:600}
.badge{padding:.15rem .5rem;border-radius:999px;font-size:.8rem;border:1px solid #e2e8f0}
.badge.on{background:#dcfce7;border-color:#bbf7d0}
.badge.off{background:#fee2e2;border-color:#fecaca}
input,select{padding:.4rem .6rem;border:1px solid #cbd5e1;border-radius:8px;background:#fff}
button{padding:.45rem .8rem;border:1px solid #cbd5e1;border-radius:8px;background:#fff;cursor:pointer}
button.primary{background:#0f172a;color:#fff;border-color:#0f172a}
button:hover{filter:brightness(.98)}
pre{background:#fff;border:1px solid #e2e8f0;padding:.7rem;border-radius:8px;overflow:auto}
.small{color:#64748b;font-size:.85rem}
@media (prefers-color-scheme: dark){
 body{background:#0b1220;color:#e2e8f0}
 header{background:#0f172a;border-color:#1e293b}
 .card{background:#0f172a;border-color:#1e293b;box-shadow:none}
 th{background:#1e293b}
 td,th{border-color:#1e293b}
 input,select,pre,button{background:#1e293b;border-color:#334155;color:#e2e8f0}
 .badge{border-color:#334155}
}
"#
}

fn layout_start(title: &str, user: Option<&UserRow>) -> String {
    let mut s = String::new();
    s.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>");
    s.push_str(&html_escape(title));
    s.push_str("</title><style>");
    s.push_str(base_style());
    s.push_str("</style></head><body><header><h1>maki anchor</h1><nav class=\"nav\"><a href=\"/\" class=\"");
    s.push_str(if title == "maki anchor" { "active" } else { "" });
    s.push_str("\">Dashboard</a>");
    if user.is_some_and(|u| u.is_admin) {
        s.push_str("<a href=\"/admin\" class=\"");
        s.push_str(if title == "maki anchor — admin" {
            "active"
        } else {
            ""
        });
        s.push_str("\">Admin</a>");
    }
    s.push_str("</nav>");
    if let Some(user) = user {
        let who = user
            .name
            .as_deref()
            .or(user.email.as_deref())
            .unwrap_or("user");
        s.push_str(&format!(
            "<span class=\"user\">{} (id {}){} · <a href=\"/logout\">log out</a></span>",
            html_escape(who),
            user.id,
            if user.is_admin { " · admin" } else { "" }
        ));
    } else {
        s.push_str("<span class=\"user\"><a href=\"/login\">log in</a></span>");
    }
    s.push_str("</header><main style=\"padding:0 1rem\">");
    s
}

fn layout_end() -> &'static str {
    "</main></body></html>"
}

pub fn render(
    store: &Arc<Store>,
    user: Option<&UserRow>,
    _auth: &crate::auth::Auth,
) -> (u16, String, Vec<u8>) {
    let instances_all = store.list_instances().unwrap_or_default();
    let sessions_all = store.list_sessions().unwrap_or_default();
    let (instances, sessions) = match user {
        Some(u) if !u.is_admin => {
            let inst = store.instances_for_user(u.id, false).unwrap_or_default();
            let sess = store.sessions_for_user(u.id, false).unwrap_or_default();
            (inst, sess)
        }
        _ => (instances_all, sessions_all),
    };
    let now = crate::store::now_unix();
    let mut body = String::with_capacity(8192);
    body.push_str(&layout_start("maki anchor", user));
    body.push_str(
        r#"<div class="card" id="install">
        <h2>Install on a new host</h2>
        <p class="small">Creates an instance and prints one-liners that install <code>maki</code> and write the anchor config. Respects <code>mint_tokens</code> (any/user/admin).</p>
        <div style="display:flex;gap:.5rem;flex-wrap:wrap;align-items:end;margin:.6rem 0">
          <label>Instance name<br><input id="inst-name" placeholder="work-laptop"></label>
          <button id="inst-create" class="primary">Create &amp; copy install command</button>
          <span id="inst-status" class="small"></span>
        </div>
        <div style="margin:.6rem 0"><strong>Linux / macOS / WSL / Git Bash</strong><pre id="install-cmd"></pre></div>
        <div style="margin:.6rem 0"><strong>Windows PowerShell</strong><pre id="install-cmd-ps"></pre></div>
        <details><summary class="small">Without a pre-minted token</summary><code id="install-cmd-notoken"></code> <button id="copy-notoken">Copy</button><br><code id="install-cmd-ps-notoken"></code> <button id="copy-notoken-ps">Copy</button></details>
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
          const notokenPs = `irm ${origin}/install.ps1 -OutFile install.ps1; .\install.ps1 -Anchor "${origin}"`;
          cmdNoTokenEl.textContent = notoken;
          cmdNoTokenPsEl.textContent = notokenPs;
          document.getElementById('copy-notoken').onclick = () => navigator.clipboard.writeText(notoken).then(()=>{statusEl.textContent='copied'; setTimeout(()=>statusEl.textContent='',1500)});
          document.getElementById('copy-notoken-ps').onclick = () => navigator.clipboard.writeText(notokenPs).then(()=>{statusEl.textContent='copied'; setTimeout(()=>statusEl.textContent='',1500)});
          const placeholder = `curl -fsSL ${origin}/install.sh | sh -s -- --anchor "${origin}" --name "NAME" --token "TOKEN"`;
          const placeholderPs = `irm ${origin}/install.ps1 -OutFile install.ps1; .\install.ps1 -Anchor "${origin}" -Name "NAME" -Token "TOKEN"`;
          cmdEl.textContent = placeholder;
          cmdPsEl.textContent = placeholderPs;
          btn.onclick = async () => {
            const name = nameEl.value.trim();
            if (!name) { statusEl.textContent = 'enter a name'; return; }
            statusEl.textContent = 'creating…';
            try {
              const res = await fetch('/api/instances', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({name})});
              const j = await res.json().catch(()=>({}));
              if (!res.ok) { statusEl.textContent = j.error || ('failed '+res.status); return; }
              const cmd = `curl -fsSL ${origin}/install.sh | sh -s -- --anchor "${origin}" --name "${j.name}" --token "${j.token}"`;
              const cmdPs = `irm ${origin}/install.ps1 -OutFile install.ps1; .\install.ps1 -Anchor "${origin}" -Name "${j.name}" -Token "${j.token}"`;
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
    body.push_str("<div class=\"card\"><h2>Instances</h2><table><tr><th>Name</th><th>Status</th><th>Last seen</th></tr>");
    if instances.is_empty() {
        body.push_str("<tr><td colspan=3 class=\"small\">no instances yet — install on a host to appear</td></tr>");
    }
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
    if user.is_some_and(|u| !u.is_admin) {
        body.push_str("<p class=\"small\">Showing only instances you have a grant for. Ask an admin for access to more.</p>");
    }
    body.push_str("</div>");
    body.push_str("<div class=\"card\"><h2>Sessions</h2><table><tr><th>Instance</th><th>Title</th><th>Model</th><th>Status</th><th>Updated</th></tr>");
    let names: std::collections::HashMap<i64, String> =
        instances.iter().map(|i| (i.id, i.name.clone())).collect();
    let all_names: std::collections::HashMap<i64, String> = store
        .list_instances()
        .unwrap_or_default()
        .into_iter()
        .map(|i| (i.id, i.name))
        .collect();
    if sessions.is_empty() {
        body.push_str("<tr><td colspan=5 class=\"small\">no sessions yet</td></tr>");
    }
    for session in &sessions {
        let name_map = if names.contains_key(&session.instance_id) {
            &names
        } else {
            &all_names
        };
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}s ago</td></tr>",
            html_escape(
                name_map
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
    if user.is_some_and(|u| !u.is_admin) {
        body.push_str("<p class=\"small\">Filtered to your grants. Admins see all on <a href=\"/admin\">Admin</a>.</p>");
    } else if user.is_some_and(|u| u.is_admin) {
        body.push_str("<p class=\"small\"><a href=\"/admin\">Admin</a> → manage users, grants, and mint policy.</p>");
    }
    body.push_str("</div>");
    body.push_str(
        "<div class=\"card\"><h2>Share a session</h2>\
         <form method=\"get\" action=\"/links\" style=\"display:flex;gap:.5rem;flex-wrap:wrap;align-items:end\">\
         <label>Instance<br><input name=\"instance\" type=\"number\" min=\"1\" required></label> \
         <label>Session ID<br><input name=\"session\" placeholder=\"optional\"></label> \
         <label>Rights<br><select name=\"rights\"><option>view</option><option>control</option></select></label> \
         <label>Hours<br><input name=\"hours\" type=\"number\" min=\"1\" value=\"2\"></label> \
         <button type=\"submit\" class=\"primary\">Mint link</button></form>\
         <p class=\"small\">Links are proxied via the tunnel; grants can upgrade a view link to control for you.</p></div>",
    );
    body.push_str(layout_end());
    (200, "text/html".to_string(), body.into_bytes())
}

pub fn render_admin(
    _store: &Arc<Store>,
    user: Option<&UserRow>,
    auth: &crate::auth::Auth,
) -> (u16, String, Vec<u8>) {
    let mut body = String::with_capacity(8192);
    body.push_str(&layout_start("maki anchor — admin", user));
    let mint = auth.effective_mint_tokens().as_str().to_owned();
    body.push_str(&format!(
        r##"<div class="card">
        <h2>Admin — User management</h2>
        <div id="sso-status" class="small" style="margin:.5rem 0"></div>
        <div style="display:flex;gap:.6rem;align-items:center;flex-wrap:wrap;margin:.6rem 0">
          <label>Mint tokens<br><select id="mint-select"><option value="any">any (anonymous)</option><option value="user">user (any logged-in)</option><option value="admin">admin only</option></select></label>
          <button id="mint-save">Save</button>
          <span id="mint-status" class="small"></span>
          <span class="small">Current: <code id="mint-current">{}</code></span>
        </div>
        <h3>Users</h3>
        <table id="users-table" style="width:100%"><tr><th>ID</th><th>Sub</th><th>Email</th><th>Name</th><th>Admin</th><th>Actions</th></tr></table>
        <h3>Grants (per-user per-instance)</h3>
        <table id="grants-table" style="width:100%"><tr><th>User ID</th><th>Instance</th><th>Rights</th></tr></table>
        <form id="grant-form" style="margin:.8rem 0;display:flex;gap:.5rem;flex-wrap:wrap;align-items:end">
          <label>User ID<br><input id="grant-user" type="number" min="1" required style="width:6rem"></label>
          <label>Instance<br><input id="grant-instance" placeholder="work-laptop" required></label>
          <label>Rights<br><select id="grant-rights"><option value="view">view</option><option value="control">control</option></select></label>
          <button type="submit" class="primary">Set grant</button>
          <button type="button" id="grant-revoke">Revoke</button>
          <span id="grant-status" class="small"></span>
        </form>
        <div style="margin-top:1rem;padding-top:1rem;border-top:1px solid #e2e8f0">
          <h3>Create local user</h3>
          <form id="user-create-form" style="display:flex;gap:.5rem;flex-wrap:wrap;align-items:end">
            <label>Username<br><input id="new-username" required></label>
            <label>Password<br><input id="new-password" type="password" required></label>
            <label>Email<br><input id="new-email" placeholder="optional"></label>
            <label>Name<br><input id="new-name" placeholder="optional"></label>
            <label><input type="checkbox" id="new-admin"> Admin</label>
            <button type="submit" class="primary">Create</button>
            <span id="user-create-status" class="small"></span>
          </form>
          <div style="margin:.6rem 0;display:flex;gap:.5rem;flex-wrap:wrap">
            <input id="admin-toggle-user" placeholder="username" style="width:10rem"><button id="admin-toggle-btn">Toggle admin</button>
            <input id="delete-user" placeholder="username" style="width:10rem"><button id="delete-btn" style="border-color:#fecaca;color:#dc2626">Delete user</button>
            <span id="user-manage-status" class="small"></span>
          </div>
          <p class="small">First user (SSO or local) becomes admin. Local users log in via <a href="/login">/login</a>. Use <code>maki-anchor users add &lt;username&gt; --admin</code> on server as fallback.</p>
        </div>
        </div>
        <script>
        (() => {{
          const ssoEl = document.getElementById('sso-status');
          const usersTable = document.getElementById('users-table');
          const grantsTable = document.getElementById('grants-table');
          const grantForm = document.getElementById('grant-form');
          const grantStatus = document.getElementById('grant-status');
          const mintSelect = document.getElementById('mint-select');
          const mintCurrent = document.getElementById('mint-current');
          const mintStatus = document.getElementById('mint-status');
          mintSelect.value = "{}";
          const fetchJson = async (u, opts) => {{
            try {{ const r = await fetch(u, opts); const j = await r.json().catch(()=>({{}})); return {{ok: r.ok, j, status: r.status}}; }} catch(e){{ return {{ok:false, j:{{error:String(e)}}}}; }}
          }};
          const escape = s => String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
          const loadSso = async () => {{
            const {{ok, j}} = await fetchJson('/api/sso');
            if (!ok) {{ ssoEl.textContent = 'SSO: unknown'; return; }}
            ssoEl.textContent = j.enabled ? `SSO enabled — issuer ${{j.issuer}} origin ${{j.origin}}` : 'SSO disabled';
          }};
          const loadMint = async () => {{
            const {{ok, j}} = await fetchJson('/api/config/mint_tokens');
            if (ok && j.mint_tokens) {{ mintSelect.value = j.mint_tokens; mintCurrent.textContent = j.mint_tokens; }}
          }};
          document.getElementById('mint-save').onclick = async () => {{
            mintStatus.textContent='saving…';
            const {{ok, j}} = await fetchJson('/api/config/mint_tokens', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{mint_tokens: mintSelect.value}})}});
            mintStatus.textContent = ok ? 'saved '+j.mint_tokens : (j.error||'failed');
            if (ok) mintCurrent.textContent = j.mint_tokens;
          }};
          const loadUsers = async () => {{
            const {{ok, j}} = await fetchJson('/api/users');
            if (!ok) return;
            usersTable.querySelectorAll('tr:not(:first-child)').forEach(r=>r.remove());
            for (const u of j) {{
              const tr = document.createElement('tr');
              tr.innerHTML = `<td>${{u.id}}</td><td style="max-width:18rem;overflow:hidden;text-overflow:ellipsis">${{escape(u.oidc_sub)}}</td><td>${{escape(u.email||'')}}</td><td>${{escape(u.name||'')}}</td><td>${{u.is_admin?'yes':''}}</td><td class="small"><a href="#" data-user="${{escape(u.oidc_sub)}}">copy sub</a></td>`;
              usersTable.appendChild(tr);
            }}
            if (j.length===0) {{
              const tr=document.createElement('tr'); tr.innerHTML='<td colspan=6 style="color:#64748b">no users yet</td>'; usersTable.appendChild(tr);
            }}
          }};
          const loadGrants = async () => {{
            const {{ok, j}} = await fetchJson('/api/grants');
            if (!ok) return;
            grantsTable.querySelectorAll('tr:not(:first-child)').forEach(r=>r.remove());
            for (const g of j) {{
              const tr=document.createElement('tr'); tr.innerHTML=`<td>${{g[0]}}</td><td>${{escape(g[1])}}</td><td>${{escape(g[2])}}</td>`;
              grantsTable.appendChild(tr);
            }}
          }};
          grantForm.onsubmit = async (e) => {{
            e.preventDefault();
            const user_id = parseInt(document.getElementById('grant-user').value,10);
            const instance = document.getElementById('grant-instance').value.trim();
            const rights = document.getElementById('grant-rights').value;
            grantStatus.textContent='saving…';
            const {{ok, j}} = await fetchJson('/api/grants', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{user_id, instance, rights}})}});
            grantStatus.textContent = ok ? 'saved' : (j.error||'failed');
            if (ok) loadGrants();
          }};
          document.getElementById('grant-revoke').onclick = async () => {{
            const user_id = parseInt(document.getElementById('grant-user').value,10);
            const instance = document.getElementById('grant-instance').value.trim();
            if (!user_id || !instance) {{ grantStatus.textContent='need user ID and instance'; return; }}
            grantStatus.textContent='revoking…';
            const {{ok, j}} = await fetchJson('/api/grants/revoke', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{user_id, instance}})}});
            grantStatus.textContent = ok ? 'revoked' : (j.error||'failed');
            if (ok) loadGrants();
          }};
          const userCreateForm = document.getElementById('user-create-form');
          const userCreateStatus = document.getElementById('user-create-status');
          if (userCreateForm) {{
            userCreateForm.onsubmit = async (e) => {{
              e.preventDefault();
              const username = document.getElementById('new-username').value.trim();
              const password = document.getElementById('new-password').value;
              const email = document.getElementById('new-email').value.trim();
              const name = document.getElementById('new-name').value.trim();
              const is_admin = document.getElementById('new-admin').checked;
              userCreateStatus.textContent='creating…';
              const {{ok, j}} = await fetchJson('/api/users', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{username, password, email: email||undefined, name: name||undefined, is_admin}})}});
              userCreateStatus.textContent = ok ? `created ${{j.username}} id ${{j.id}}` : (j.error||'failed');
              if (ok) {{ loadUsers(); userCreateForm.reset(); }}
            }};
          }}
          document.getElementById('admin-toggle-btn').onclick = async () => {{
            const username = document.getElementById('admin-toggle-user').value.trim();
            if (!username) return;
            const s = document.getElementById('user-manage-status');
            s.textContent='toggling…';
            const is_admin = confirm('Make '+username+' admin? OK=yes, Cancel=revoke admin');
            const {{ok, j}} = await fetchJson('/api/users/set-admin', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{username, is_admin}})}});
            s.textContent = ok ? (is_admin?'now admin':'revoked admin') : (j.error||'failed');
            if (ok) loadUsers();
          }};
          document.getElementById('delete-btn').onclick = async () => {{
            const username = document.getElementById('delete-user').value.trim();
            if (!username || !confirm('Delete user '+username+'?')) return;
            const s = document.getElementById('user-manage-status');
            s.textContent='deleting…';
            const {{ok, j}} = await fetchJson('/api/users/delete', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{username}})}});
            s.textContent = ok ? 'deleted' : (j.error||'failed');
            if (ok) {{ loadUsers(); loadGrants(); }}
          }};
          loadSso(); loadMint(); loadUsers(); loadGrants();
        }})();
        </script>
        "##,
        mint, mint
    ));
    body.push_str(layout_end());
    (200, "text/html".to_string(), body.into_bytes())
}

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
