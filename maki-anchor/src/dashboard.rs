use std::sync::Arc;

use crate::store::{Store, UserRow};

/// Installing from the dashboard is a separate, deliberately distinct thing
/// from installing a single session (maki-remote/src/dispatch.rs's own
/// PWA_MANIFEST, scoped to that session's token path and reachable without
/// any anchor login at all — a share link recipient never touches this
/// one). This is "maki, the whole account" — `start_url`/`scope` are the
/// bare root, so the installed icon always reopens the dashboard, and the
/// dashboard itself is already login-gated (`route_authorized` in
/// server.rs), same as visiting "/" in a normal tab.
pub(crate) const DASHBOARD_MANIFEST: &str = r##"{
  "name": "maki anchor",
  "short_name": "maki",
  "start_url": "/",
  "scope": "/",
  "display": "standalone",
  "background_color": "#f8fafc",
  "theme_color": "#0f172a",
  "icons": [
    {"src": "/icon.svg", "sizes": "any", "type": "image/svg+xml", "purpose": "any"},
    {"src": "/icon.svg", "sizes": "any", "type": "image/svg+xml", "purpose": "maskable"}
  ]
}"##;

/// No caching, no offline shell — just enough (a registered worker with a
/// fetch handler) to satisfy install criteria. Mirrors maki-remote's own
/// service worker exactly, and the same reasoning: a real offline cache
/// would need its own staleness story, not worth the risk for what it buys
/// a page this simple.
pub(crate) const DASHBOARD_SW_JS: &str = "\
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));
self.addEventListener('fetch', () => {});
";

/// Same icon as maki-remote's installed sessions — one "maki" mark for
/// both installable surfaces, distinguished by name/short_name instead.
pub(crate) const DASHBOARD_ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
  <rect width="100" height="100" rx="18" fill="#16181d"/>
  <text x="50" y="68" font-family="ui-monospace,Menlo,Consolas,monospace" font-size="56" font-weight="700" fill="#7aa2f7" text-anchor="middle">m</text>
</svg>"##;

/// Mirrors maki-remote's own install banner (maki-remote/src/index.html),
/// scaled down for a classic server-rendered page with no other JS: no
/// notification opt-in here (there's nothing running to notify you about
/// on the dashboard itself), just the beforeinstallprompt flow and the iOS
/// Share-sheet text fallback (iOS Safari never fires that event).
const INSTALL_JS: &str = r#"<script>
(() => {
  const banner = document.getElementById('install-banner');
  const text = document.getElementById('install-banner-text');
  const action = document.getElementById('install-banner-action');
  const dismissBtn = document.getElementById('install-banner-dismiss');
  let deferred = null;
  const isStandalone = () =>
    (window.matchMedia && window.matchMedia('(display-mode: standalone)').matches) ||
    window.navigator.standalone === true;
  const isIosSafari = () => /iphone|ipad|ipod/i.test(navigator.userAgent) && !window.MSStream;
  const dismissed = () => { try { return localStorage.getItem('maki_anchor_install_dismissed') === '1'; } catch { return false; } };
  const dismiss = () => {
    banner.hidden = true;
    try { localStorage.setItem('maki_anchor_install_dismissed', '1'); } catch {}
  };
  const update = () => {
    if (dismissed() || isStandalone()) { banner.hidden = true; return; }
    if (deferred) {
      text.textContent = 'Install maki for quick access to your sessions.';
      action.hidden = false;
      banner.hidden = false;
    } else if (isIosSafari()) {
      text.textContent = 'Add maki to your Home Screen: Share → Add to Home Screen.';
      action.hidden = true;
      banner.hidden = false;
    } else {
      banner.hidden = true;
    }
  };
  window.addEventListener('beforeinstallprompt', (ev) => {
    ev.preventDefault();
    deferred = ev;
    update();
  });
  window.addEventListener('appinstalled', () => { deferred = null; dismiss(); });
  action.onclick = async () => {
    if (!deferred) return;
    deferred.prompt();
    try { await deferred.userChoice; } catch {}
    deferred = null;
    update();
  };
  dismissBtn.onclick = dismiss;
  update();
  if ('serviceWorker' in navigator) navigator.serviceWorker.register('/sw.js').catch(() => {});
})();
</script>"#;

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
footer{margin:2.5rem auto 1.2rem;max-width:64rem;padding:.9rem 1.2rem 0;border-top:1px solid #e2e8f0;color:#64748b;font-size:.85rem;display:flex;gap:1.1rem;flex-wrap:wrap}
footer a{color:#64748b;border-bottom:1px solid #cbd5e1}
footer a:hover{color:#2563eb;border-color:#2563eb;text-decoration:none}
.card.narrow{max-width:30rem}
.card label{display:block;color:#64748b;font-size:.85rem}
.card input,.card select{width:100%;margin-top:.15rem}
.card form p{margin:.6rem 0}
button.primary,a.btn{background:#0f172a;color:#fff;border-color:#0f172a}
a.btn{display:inline-block;padding:.45rem .8rem;border-radius:8px;text-decoration:none}
a.btn:hover{filter:brightness(1.35);text-decoration:none}
button.danger{border-color:#fca5a5;color:#b91c1c;background:#fef2f2;padding:.25rem .55rem;font-size:.8rem}
#install-banner{display:none;align-items:center;gap:.8rem;padding:.5rem 1.2rem;background:#eff6ff;border-bottom:1px solid #bfdbfe;font-size:.85rem}
#install-banner:not([hidden]){display:flex}
#install-banner span{flex:1;color:#1e40af}
#install-banner button{flex:none}
#install-banner-dismiss{background:none;border:none;cursor:pointer;color:#64748b;font-size:1rem;padding:0 .3rem}
@media (prefers-color-scheme: dark){
 body{background:#0b1220;color:#e2e8f0}
 header{background:#0f172a;border-color:#1e293b}
 .card{background:#0f172a;border-color:#1e293b;box-shadow:none}
 th{background:#1e293b}
 td,th{border-color:#1e293b}
 input,select,pre,button{background:#1e293b;border-color:#334155;color:#e2e8f0}
 button.primary,a.btn{background:#e2e8f0;color:#0f172a;border-color:#e2e8f0}
 button.danger{background:#2a1517;border-color:#7f1d1d;color:#fca5a5}
 .badge{border-color:#334155}
 .badge.on{background:#052e16;border-color:#14532d;color:#86efac}
 .badge.off{background:#450a0a;border-color:#7f1d1d;color:#fca5a5}
 footer{border-color:#1e293b}
 #install-banner{background:#0c1e3d;border-color:#1e3a5f}
 #install-banner span{color:#93c5fd}
}
@media (max-width:680px){
 .card{margin:.8rem .5rem;padding:.9rem .8rem}
 table{display:block;overflow-x:auto;white-space:nowrap}
 header{padding:.6rem .7rem}
 .user{margin-left:0;width:100%}
 .card form label{margin-bottom:.4rem}
 button.primary{width:auto}
}
"#
}

pub(crate) fn layout_start(title: &str, user: Option<&UserRow>, page: &str) -> String {
    let mut s = String::from(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>",
    );
    s.push_str(&html_escape(title));
    s.push_str(
        "</title>\
         <link rel=\"manifest\" href=\"/manifest.json\">\
         <link rel=\"icon\" href=\"/icon.svg\" type=\"image/svg+xml\">\
         <link rel=\"apple-touch-icon\" href=\"/icon.svg\">\
         <meta name=\"theme-color\" content=\"#0f172a\">\
         <meta name=\"apple-mobile-web-app-capable\" content=\"yes\">\
         <meta name=\"apple-mobile-web-app-status-bar-style\" content=\"black-translucent\">\
         <style>",
    );
    s.push_str(base_style());
    s.push_str("</style></head><body><header><h1>maki anchor</h1><nav class=\"nav\">");
    let item = |href: &str, label: &str, id: &str| -> String {
        let cls = if page == id { "active" } else { "" };
        format!("<a href=\"{href}\" class=\"{cls}\">{label}</a>")
    };
    s.push_str(&item("/", "Sessions", "sessions"));
    s.push_str(&item("/instances", "Instances", "instances"));
    s.push_str(&item("/links", "Links", "links"));
    if user.is_some_and(|u| u.is_admin) {
        s.push_str(&item("/admin", "Admin", "admin"));
    }
    s.push_str("</nav>");
    if let Some(user) = user {
        let who = user
            .name
            .as_deref()
            .or(user.email.as_deref())
            .unwrap_or_else(|| {
                user.oidc_sub
                    .strip_prefix("local:")
                    .unwrap_or(&user.oidc_sub)
            });
        s.push_str(&format!(
            "<span class=\"user\">{}{} · <a href=\"/logout\">log out</a></span>",
            html_escape(who),
            if user.is_admin { " (admin)" } else { "" }
        ));
    } else {
        s.push_str("<span class=\"user\"><a href=\"/login\">log in</a></span>");
    }
    s.push_str(
        "</header>\
         <div id=\"install-banner\" hidden>\
         <span id=\"install-banner-text\"></span>\
         <button class=\"primary\" id=\"install-banner-action\" hidden>Install</button>\
         <button id=\"install-banner-dismiss\" title=\"Dismiss\">×</button>\
         </div>\
         <main style=\"padding:0 1rem\">",
    );
    s
}

/// Every anchored page signs off with credit, links (each opening in its own
/// tab, since they lead off the dashboard), and the running build's version.
const FOOTER: &str = concat!(
    "<footer><span><a href=\"https://github.com/tontinton/maki\" target=\"_blank\" rel=\"noopener\">maki original</a> · ",
    "<a href=\"https://github.com/wmantly/maki\" target=\"_blank\" rel=\"noopener\">maki anchor fork</a> · ",
    "brought to you by <a href=\"https://community.theta42.com/\" target=\"_blank\" rel=\"noopener\">Theta42 community</a></span>",
    "<span class=\"small\">v",
    env!("CARGO_PKG_VERSION"),
    "</span></footer>"
);

pub(crate) fn layout_end() -> String {
    format!("{FOOTER}{INSTALL_JS}</main></body></html>")
}

/// A page without the nav (setup, login, refusals): same skin, centered card,
/// same footer.
pub fn standalone_page(status: u16, title: &str, content: &str) -> (u16, String, Vec<u8>) {
    let body = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{}</style></head><body><header><h1>maki anchor</h1></header><main><div class=\"card narrow\">{}</div>{}</body></html>",
        html_escape(title),
        base_style(),
        content,
        FOOTER
    );
    (status, "text/html".to_string(), body.into_bytes())
}

/// What this viewer may see: everything for admins, granted rows otherwise.
fn visible(
    store: &Store,
    user: Option<&UserRow>,
) -> (
    Vec<crate::store::InstanceRow>,
    Vec<crate::store::SessionRow>,
) {
    match user {
        Some(u) if !u.is_admin => (
            store.instances_for_user(u.id, false).unwrap_or_default(),
            store.sessions_for_user(u.id, false).unwrap_or_default(),
        ),
        _ => (
            store.list_instances().unwrap_or_default(),
            store.list_sessions().unwrap_or_default(),
        ),
    }
}

/// The home page: which shares are live right now, and the sessions the
/// instances have reported.
pub fn render_sessions(store: &Store, user: Option<&UserRow>) -> (u16, String, Vec<u8>) {
    let (instances, sessions) = visible(store, user);
    let now = crate::store::now_unix();
    let mut body = String::with_capacity(8192);
    body.push_str(&layout_start("maki anchor", user, "sessions"));
    body.push_str(
        "<div class=\"card\"><h2>Sessions</h2>\
         <div style=\"margin-bottom:.5rem\"><input id=\"session-search\" type=\"search\" \
         placeholder=\"search titles and transcripts…\" style=\"width:100%;max-width:24rem\"></div>\
         <table><tr><th>Instance</th><th>Title</th><th>Model</th><th>Status</th><th>Cost</th><th>Updated</th><th></th></tr><tbody id=\"sessions-tbody\">",
    );
    let names: std::collections::HashMap<i64, String> =
        instances.iter().map(|i| (i.id, i.name.clone())).collect();
    if sessions.is_empty() {
        body.push_str("<tr><td colspan=7 class=\"small\">no sessions yet</td></tr>");
    }
    for session in &sessions {
        let instance_name = names
            .get(&session.instance_id)
            .map(String::as_str)
            .unwrap_or("?");
        let cost = if session.cost_cents == 0 && session.tokens_in == 0 {
            String::new()
        } else {
            format!("${:.2}", session.cost_cents as f64 / 100.0)
        };
        // An Open link mints a two-hour control link scoped to just this
        // session, so the dashboard's scoped-share path needs no hand-typed ids.
        let open = format!(
            "/links?instance={}&amp;session={}&amp;rights=control&amp;hours=2",
            urlencode(instance_name),
            urlencode(&session.external_id),
        );
        let detail = format!(
            "/session/{}/{}",
            urlencode(instance_name),
            urlencode(&session.external_id),
        );
        body.push_str(&format!(
            "<tr><td>{}</td><td><a href=\"{detail}\">{}</a></td><td>{}</td><td>{}</td><td>{cost}</td><td>{}s ago</td>\
             <td><a href=\"{open}\">open</a></td></tr>",
            html_escape(instance_name),
            html_escape(&session.title),
            html_escape(&session.model),
            html_escape(&session.status),
            now - session.updated_at,
        ));
    }
    body.push_str("</tbody></table>");
    body.push_str(&format!(
        "<script>
        (() => {{
          const names = {};
          const tbody = document.getElementById('sessions-tbody');
          const initial = tbody.innerHTML;
          const input = document.getElementById('session-search');
          const esc = s => s.replace(/[&<>\"']/g, c => ({{'&':'&amp;','<':'&lt;','>':'&gt;','\"':'&quot;',\"'\":'&#39;'}})[c]);
          const enc = encodeURIComponent;
          let inflight = 0;
          const render = rows => {{
            if (!rows.length) {{ tbody.innerHTML = '<tr><td colspan=7 class=\"small\">no matches</td></tr>'; return; }}
            const now = Date.now() / 1000;
            tbody.innerHTML = rows.map(s => {{
              const instance = names[s.instance_id] || '?';
              const detail = `/session/${{enc(instance)}}/${{enc(s.external_id)}}`;
              const open = `/links?instance=${{enc(instance)}}&session=${{enc(s.external_id)}}&rights=control&hours=2`;
              const cost = (s.cost_cents || s.tokens_in) ? `$${{(s.cost_cents/100).toFixed(2)}}` : '';
              return `<tr><td>${{esc(instance)}}</td><td><a href=\"${{detail}}\">${{esc(s.title)}}</a></td><td>${{esc(s.model)}}</td><td>${{esc(s.status)}}</td><td>${{cost}}</td><td>${{Math.max(0,Math.floor(now - s.updated_at))}}s ago</td><td><a href=\"${{open}}\">open</a></td></tr>`;
            }}).join('');
          }};
          let timer = null;
          input.oninput = () => {{
            clearTimeout(timer);
            const q = input.value.trim();
            if (!q) {{ tbody.innerHTML = initial; return; }}
            timer = setTimeout(async () => {{
              const mine = ++inflight;
              try {{
                const res = await fetch(`/api/sessions/search?q=${{enc(q)}}`);
                const rows = await res.json();
                if (mine === inflight && res.ok) render(rows);
              }} catch (e) {{ /* leave the table as-is on a transient failure */ }}
            }}, 250);
          }};
        }})();
        </script>",
        serde_json::to_string(&names).unwrap_or_else(|_| "{}".to_string()),
    ));
    if user.is_some_and(|u| !u.is_admin) {
        body.push_str("<p class=\"small\">Filtered to your grants. Admins see all on <a href=\"/admin\">Admin</a>.</p>");
    }
    body.push_str("</div>");
    body.push_str(&layout_end());
    (200, "text/html".to_string(), body.into_bytes())
}

/// The Instances page: the install wizard and the fleet roster.
pub fn render_instances(store: &Store, user: Option<&UserRow>) -> (u16, String, Vec<u8>) {
    let (instances, _) = visible(store, user);
    let now = crate::store::now_unix();
    let mut body = String::with_capacity(8192);
    body.push_str(&layout_start("maki anchor — instances", user, "instances"));
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
              statusEl.textContent = 'created — copy the command below';
              // Clipboard writes can hang or reject outside a trusted user
              // gesture (e.g. some embedded/automated contexts); the instance
              // is already created and the command is on screen either way,
              // so a clipboard failure only gets a quiet note, never blocks
              // the success status above.
              navigator.clipboard.writeText(cmd)
                .then(() => { statusEl.textContent = 'copied Linux command (PS below)'; })
                .catch(() => { statusEl.textContent = 'created — copy the command below (clipboard blocked)'; });
            } catch (e) { statusEl.textContent = 'error: '+e; }
          };
        })();
        </script>
        "#,
    );
    let is_admin = user.is_some_and(|u| u.is_admin);
    body.push_str("<div class=\"card\"><h2>Instances</h2><table><tr><th>Name</th><th>Status</th><th>Last seen</th><th></th></tr>");
    if instances.is_empty() {
        body.push_str("<tr><td colspan=4 class=\"small\">no instances yet — install on a host to appear</td></tr>");
    }
    for instance in &instances {
        let online = now - instance.last_seen < 90;
        let bulk_delete = if is_admin {
            format!(
                "<button class=\"mini danger-arm\" data-instance-id=\"{}\" data-instance-name=\"{}\">delete all sessions</button>",
                instance.id,
                html_escape(&instance.name),
            )
        } else {
            String::new()
        };
        body.push_str(&format!(
            "<tr><td>{}</td><td><span class=\"badge {}\">{}</span></td><td>{}s ago</td><td>{bulk_delete}</td></tr>",
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
    if is_admin && !instances.is_empty() {
        // Same arm-then-confirm pattern as the links page's "revoke all" and
        // the remote file panel's delete: a bulk, destructive action gets a
        // second click instead of a confirm() dialog that blocks the tab.
        body.push_str(
            "<script>(() => { \
             for (const b of document.querySelectorAll('.danger-arm')) { \
               let armed = false, timer = null; \
               b.onclick = async () => { \
                 if (!armed) { \
                   armed = true; b.dataset.label = b.textContent; b.textContent = 'sure?'; b.classList.add('danger'); \
                   timer = setTimeout(() => { armed = false; b.textContent = b.dataset.label; b.classList.remove('danger'); }, 3000); \
                   return; \
                 } \
                 clearTimeout(timer); \
                 b.disabled = true; b.textContent = 'deleting…'; \
                 const id = Number(b.dataset.instanceId); \
                 const name = b.dataset.instanceName; \
                 const res = await fetch('/api/sessions'); \
                 const rows = res.ok ? await res.json() : []; \
                 const mine = rows.filter((r) => r.instance_id === id); \
                 await Promise.all(mine.map((r) => fetch('/api/sessions/delete', { \
                   method: 'POST', headers: {'Content-Type': 'application/json'}, \
                   body: JSON.stringify({instance: name, session: r.external_id}), \
                 }))); \
                 location.reload(); \
               }; \
             } \
             })();</script>",
        );
    }
    body.push_str("</div>");
    body.push_str(&layout_end());
    (200, "text/html".to_string(), body.into_bytes())
}

/// The Links page: mint a share, and see every live one.
pub fn render_links(
    store: &Store,
    hub: &crate::hub::Hub,
    user: Option<&UserRow>,
) -> (u16, String, Vec<u8>) {
    let (instances, _) = visible(store, user);
    let mut body = String::with_capacity(8192);
    body.push_str(&layout_start("maki anchor — links", user, "links"));
    body.push_str(
        "<div class=\"card\"><h2>Share a session</h2>\
         <form method=\"get\" action=\"/links\" style=\"display:flex;gap:.5rem;flex-wrap:wrap;align-items:end\">\
         <label>Instance<br><input name=\"instance\" list=\"instance-names\" required></label> \
         <datalist id=\"instance-names\">",
    );
    for instance in &instances {
        body.push_str(&format!(
            "<option value=\"{}\"></option>",
            html_escape(&instance.name)
        ));
    }
    body.push_str(
        "</datalist> \
         <label>Session ID<br><input name=\"session\" placeholder=\"optional\"></label> \
         <label>Rights<br><select name=\"rights\"><option>view</option><option>control</option></select></label> \
         <label>Hours<br><input name=\"hours\" type=\"number\" min=\"1\" value=\"2\"></label> \
         <button type=\"submit\" class=\"primary\">Mint link</button></form>\
         <p class=\"small\">Links are proxied via the tunnel; grants can upgrade a view link to control for you.</p></div>",
    );
    body.push_str(&links_card(store, hub, user, &instances));
    body.push_str(&layout_end());
    (200, "text/html".to_string(), body.into_bytes())
}

/// One table of live links, shared by the home and links pages. Admins get a
/// revoke button: links carry no owner column, so revoking stays an admin
/// privilege rather than a guess about who minted what.
fn links_card(
    store: &Store,
    hub: &crate::hub::Hub,
    user: Option<&UserRow>,
    instances: &[crate::store::InstanceRow],
) -> String {
    let now = crate::store::now_unix();
    let is_admin = user.is_some_and(|u| u.is_admin);
    let links: Vec<crate::store::LinkView> = store
        .list_links()
        .unwrap_or_default()
        .into_iter()
        .filter(|link| {
            is_admin
                || instances
                    .iter()
                    .any(|i| i.id == link.instance_id && i.name == link.instance_name)
        })
        .collect();
    let revoke_all = if is_admin && !links.is_empty() {
        " <button id=\"revoke-all\" class=\"mini\" type=\"button\">revoke all</button>"
    } else {
        ""
    };
    let mut s = format!(
        "<div class=\"card\" id=\"links\"><h2>Live shares{revoke_all}</h2><table><tr><th>Instance</th><th>Scope</th><th>Rights</th><th>Tunnel</th><th>Expires in</th><th></th></tr>",
    );
    if links.is_empty() {
        s.push_str("<tr><td colspan=6 class=\"small\">no live links — mint one or wait for a tunnel</td></tr>");
    }
    for link in &links {
        let open = match &link.token {
            Some(token) => {
                // `external_session_id` can carry whatever a link was minted
                // with (the mint path accepts arbitrary text), so this table
                // renders it back out on every visit — escape it here too,
                // not just where it was typed in.
                let href = html_escape(&match &link.external_session_id {
                    Some(session) => format!("/{token}/s/{session}/"),
                    None => format!("/{token}/"),
                });
                format!("<a href=\"{href}\">open</a>")
            }
            None => "<span class=\"small\">minted before tokens were shown</span>".into(),
        };
        let revoke = if is_admin {
            format!(
                "<button class=\"danger revoke\" data-hash=\"{}\">revoke</button>",
                html_escape(&link.token_hash)
            )
        } else {
            String::new()
        };
        s.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td><span class=\"badge {}\">{}</span></td><td>{}h</td><td>{open} {revoke}</td></tr>",
            html_escape(&link.instance_name),
            html_escape(link.external_session_id.as_deref().unwrap_or("all tabs")),
            html_escape(&link.rights),
            if hub.is_online(link.instance_id) { "on" } else { "off" },
            if hub.is_online(link.instance_id) { "online" } else { "offline" },
            (link.expires_at - now).div_euclid(3600),
        ));
    }
    s.push_str("</table>");
    if is_admin {
        // "revoke all" arms on a first click and only acts on a second one
        // (auto-disarming after a few seconds) instead of a confirm()
        // dialog — same reasoning as the file panel's delete button: no
        // modal that blocks the tab (or anything scripting the page) until
        // dismissed, for an action just as easy to walk back from by
        // re-minting.
        s.push_str(
            "<script>(() => { \
             const revoke = (hash) => fetch('/api/links/revoke', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({token_hash: hash})}); \
             for (const b of document.querySelectorAll('.revoke')) b.onclick = async () => { await revoke(b.dataset.hash); location.reload(); }; \
             const allBtn = document.getElementById('revoke-all'); \
             if (!allBtn) return; \
             let armed = false, timer = null; \
             allBtn.onclick = async () => { \
               if (!armed) { \
                 armed = true; allBtn.textContent = 'sure?'; allBtn.classList.add('danger'); \
                 timer = setTimeout(() => { armed = false; allBtn.textContent = 'revoke all'; allBtn.classList.remove('danger'); }, 3000); \
                 return; \
               } \
               clearTimeout(timer); \
               const hashes = [...document.querySelectorAll('.revoke')].map((b) => b.dataset.hash); \
               await Promise.all(hashes.map(revoke)); \
               location.reload(); \
             }; \
             })();</script>",
        );
    }
    s.push_str("</div>");
    s
}

pub fn render_admin(
    store: &Arc<Store>,
    user: Option<&UserRow>,
    auth: &crate::auth::Auth,
) -> (u16, String, Vec<u8>) {
    let mut body = String::with_capacity(8192);
    body.push_str(&layout_start("maki anchor — admin", user, "admin"));
    let mint = auth.effective_mint_tokens().as_str().to_owned();
    let tunnel_link_ttl_hours = crate::server::tunnel_link_ttl(store).as_secs() / 3600;
    body.push_str(&format!(
        r##"<div class="card">
        <h2>Admin — User management</h2>
        <div id="sso-status" class="small" style="margin:.5rem 0"></div>
        <h3>Single sign-on (OIDC)</h3>
        <div style="display:flex;gap:.5rem;flex-wrap:wrap;align-items:end;margin:.4rem 0">
          <label style="flex:2;min-width:14rem">Issuer<br><input id="sso-issuer" placeholder="https://auth.example.com/realms/main"></label>
          <label style="flex:1;min-width:8rem">Client ID<br><input id="sso-client" placeholder="maki-anchor"></label>
          <label style="flex:1;min-width:8rem">Client secret<br><input id="sso-secret" type="password" placeholder="kept on the server"></label>
          <label style="flex:1;min-width:8rem">Origin<br><input id="sso-origin" placeholder="https://maki.example.com"></label>
        </div>
        <div style="display:flex;gap:.5rem;align-items:center">
          <button id="sso-save" class="primary">Save SSO</button>
          <button id="sso-clear">Disable</button>
          <span id="sso-form-status" class="small"></span>
        </div>
        <p class="small">Callback URL to register with the provider: <code id="sso-callback-hint">(fill in Origin above)</code>. Changes apply on the next anchor restart. A config file's [oidc] block is used until you save here.</p>
        <div style="display:flex;gap:.6rem;align-items:center;flex-wrap:wrap;margin:.6rem 0">
          <label>Mint tokens<br><select id="mint-select"><option value="any">any (anonymous)</option><option value="user">user (any logged-in)</option><option value="admin">admin only</option></select></label>
          <button id="mint-save">Save</button>
          <span id="mint-status" class="small"></span>
          <span class="small">Current: <code id="mint-current">{}</code></span>
        </div>
        <div style="display:flex;gap:.6rem;align-items:center;flex-wrap:wrap;margin:.6rem 0">
          <label>Tunnel link lifetime (hours)<br><input id="ttl-hours" type="number" min="1" max="720" value="{}" style="width:6rem"></label>
          <button id="ttl-save">Save</button>
          <span id="ttl-status" class="small"></span>
          <span class="small">A tunnel's own control link renews on every reconnect and every request, so this is really "how long an instance can go unreachable before its share URL changes."</span>
        </div>
        <h3>Users</h3>
        <table id="users-table" style="width:100%"><tr><th>Name</th><th>Subject</th><th>Admin</th><th>Actions</th></tr></table>
        <h3>Grants (per-user per-instance)</h3>
        <table id="grants-table" style="width:100%"><tr><th>User</th><th>Instance</th><th>Rights</th></tr></table>
        <form id="grant-form" style="margin:.8rem 0;display:flex;gap:.5rem;flex-wrap:wrap;align-items:end">
          <label>User<br><select id="grant-user" required></select></label>
          <label>Instance<br><input id="grant-instance" placeholder="work-laptop" required></label>
          <label>Rights<br><select id="grant-rights"><option value="view">view</option><option value="control">control</option></select></label>
          <button type="submit" class="primary">Set grant</button>
          <button type="button" id="grant-revoke">Revoke</button>
          <span id="grant-status" class="small"></span>
        </form>
        <h3>Webhooks</h3>
        <p class="small">Fires when a session goes from working to idle — the same "the run stopped" signal the web UI's own notifications use, delivered server-side so it works with no browser open anywhere.</p>
        <table id="webhooks-table" style="width:100%"><tr><th>Instance</th><th>Kind</th><th>URL</th><th></th></tr></table>
        <form id="webhook-form" style="margin:.8rem 0;display:flex;gap:.5rem;flex-wrap:wrap;align-items:end">
          <label>Instance<br><input id="webhook-instance" placeholder="blank = every instance"></label>
          <label>Kind<br><select id="webhook-kind"><option value="generic">generic JSON</option><option value="slack">Slack</option><option value="discord">Discord</option><option value="ntfy">ntfy</option></select></label>
          <label style="flex:2;min-width:16rem">URL<br><input id="webhook-url" placeholder="https://…" required></label>
          <button type="submit" class="primary">Add webhook</button>
          <span id="webhook-status" class="small"></span>
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
          const escape = s => String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;');
          const originEl = document.getElementById('sso-origin');
          const callbackHint = document.getElementById('sso-callback-hint');
          const updateCallbackHint = () => {{
            const origin = originEl.value.trim();
            callbackHint.textContent = origin ? `${{origin.replace(/\/+$/, '')}}/callback` : '(fill in Origin above)';
          }};
          originEl.addEventListener('input', updateCallbackHint);
          const loadSso = async () => {{
            const {{ok, j}} = await fetchJson('/api/sso');
            if (!ok) {{ ssoEl.textContent = 'SSO: unknown'; return; }}
            ssoEl.textContent = j.enabled ? `SSO enabled — issuer ${{j.issuer}} origin ${{j.origin}}` : 'SSO disabled';
            if (j.enabled) {{
              document.getElementById('sso-issuer').value = j.issuer || '';
              document.getElementById('sso-client').value = j.client_id || '';
              originEl.value = j.origin || '';
            }}
            updateCallbackHint();
          }};
          const ssoSend = async (body, busy) => {{
            const st = document.getElementById('sso-form-status');
            st.textContent = busy;
            const {{ok, j}} = await fetchJson('/api/config/oidc', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify(body)}});
            st.textContent = ok ? ((j.cleared ?? 'saved') + ' — restart the anchor to apply') : (j.error || 'failed');
            if (ok) loadSso();
          }};
          document.getElementById('sso-save').onclick = () => ssoSend({{
            issuer: document.getElementById('sso-issuer').value.trim(),
            client_id: document.getElementById('sso-client').value.trim(),
            client_secret: document.getElementById('sso-secret').value,
            origin: document.getElementById('sso-origin').value.trim(),
          }}, 'saving…');
          document.getElementById('sso-clear').onclick = () => ssoSend({{issuer:'', client_id:'', client_secret:'', origin:''}}, 'clearing…');
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
          const ttlHoursEl = document.getElementById('ttl-hours');
          const ttlStatus = document.getElementById('ttl-status');
          const loadTunnelTtl = async () => {{
            const {{ok, j}} = await fetchJson('/api/config/tunnel_link_ttl_hours');
            if (ok && j.hours) ttlHoursEl.value = j.hours;
          }};
          document.getElementById('ttl-save').onclick = async () => {{
            ttlStatus.textContent='saving…';
            const {{ok, j}} = await fetchJson('/api/config/tunnel_link_ttl_hours', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{hours: Number(ttlHoursEl.value)}})}});
            ttlStatus.textContent = ok ? 'saved '+j.hours+'h' : (j.error||'failed');
            if (ok) ttlHoursEl.value = j.hours;
          }};
          const loadUsers = async () => {{
            const {{ok, j}} = await fetchJson('/api/users');
            if (!ok) return;
            usersTable.querySelectorAll('tr:not(:first-child)').forEach(r=>r.remove());
            for (const u of j) {{
              const tr = document.createElement('tr');
              const label = u.name || u.email || u.oidc_sub;
              tr.innerHTML = `<td><b>${{escape(label)}}</b></td><td class="small" style="max-width:18rem;overflow:hidden;text-overflow:ellipsis">${{escape(u.oidc_sub)}}</td><td>${{u.is_admin?'yes':''}}</td><td class="small"><a href="#" data-user="${{escape(u.oidc_sub)}}">copy sub</a></td>`;
              usersTable.appendChild(tr);
            }}
            if (j.length===0) {{
              const tr=document.createElement('tr'); tr.innerHTML='<td colspan=4 style="color:#64748b">no users yet</td>'; usersTable.appendChild(tr);
            }}
            const grantUser = document.getElementById('grant-user');
            grantUser.innerHTML = j.map(u => `<option value="${{u.id}}">${{escape(u.name||u.email||u.oidc_sub)}}</option>`).join('');
            grantUser.dataset.ready = '1';
          }};
          const loadGrants = async () => {{
            const {{ok, j}} = await fetchJson('/api/grants');
            if (!ok) return;
            grantsTable.querySelectorAll('tr:not(:first-child)').forEach(r=>r.remove());
            for (const g of j) {{
              const tr=document.createElement('tr'); tr.innerHTML=`<td>${{escape(g.user)}}</td><td>${{escape(g.instance)}}</td><td>${{escape(g.rights)}}</td>`;
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
          const webhooksTable = document.getElementById('webhooks-table');
          const webhookForm = document.getElementById('webhook-form');
          const webhookStatus = document.getElementById('webhook-status');
          const loadWebhooks = async () => {{
            const {{ok, j}} = await fetchJson('/api/webhooks');
            if (!ok) return;
            webhooksTable.querySelectorAll('tr:not(:first-child)').forEach(r=>r.remove());
            for (const w of j) {{
              const tr = document.createElement('tr');
              tr.innerHTML = `<td>${{escape(w.instance_name || 'all instances')}}</td><td>${{escape(w.kind)}}</td><td class="small" style="max-width:20rem;overflow:hidden;text-overflow:ellipsis">${{escape(w.url)}}</td><td><button class="small" data-webhook-delete="${{w.id}}">delete</button></td>`;
              webhooksTable.appendChild(tr);
            }}
            if (j.length===0) {{
              const tr=document.createElement('tr'); tr.innerHTML='<td colspan=4 style="color:#64748b">no webhooks configured</td>'; webhooksTable.appendChild(tr);
            }}
            for (const b of webhooksTable.querySelectorAll('[data-webhook-delete]')) {{
              b.onclick = async () => {{
                const {{ok, j}} = await fetchJson('/api/webhooks/delete', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{id: Number(b.dataset.webhookDelete)}})}});
                webhookStatus.textContent = ok ? 'deleted' : (j.error||'failed');
                if (ok) loadWebhooks();
              }};
            }}
          }};
          webhookForm.onsubmit = async (e) => {{
            e.preventDefault();
            const instance = document.getElementById('webhook-instance').value.trim();
            const kind = document.getElementById('webhook-kind').value;
            const url = document.getElementById('webhook-url').value.trim();
            webhookStatus.textContent = 'saving…';
            const {{ok, j}} = await fetchJson('/api/webhooks', {{method:'POST', headers:{{'Content-Type':'application/json'}}, body: JSON.stringify({{instance: instance||undefined, kind, url}})}});
            webhookStatus.textContent = ok ? 'saved' : (j.error||'failed');
            if (ok) {{ loadWebhooks(); webhookForm.reset(); }}
          }};
          loadSso(); loadMint(); loadTunnelTtl(); loadUsers(); loadGrants(); loadWebhooks();
        }})();
        </script>
        "##,
        mint, tunnel_link_ttl_hours, mint
    ));
    body.push_str(&layout_end());
    (200, "text/html".to_string(), body.into_bytes())
}

pub fn render_link(
    store: &Store,
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
            format!("unknown instance {}", html_escape(instance)).into_bytes(),
        );
    };
    // Only admins and users with a grant on the instance can share it; a
    // logged-in user with no rights gets nothing. A minted link's rights
    // must not exceed the caller's own grant either, or a viewer could mint
    // themselves a control link on an instance they were only given view
    // access to — `rights` is a query param the caller picks freely.
    if let Some(u) = user
        && !u.is_admin
    {
        let grant = store.grant_for(u.id, instance_row.id).ok().flatten();
        let allowed = match grant {
            Some(crate::store::Role::Controller) => true,
            Some(crate::store::Role::Viewer) => role != crate::store::Role::Controller,
            None => false,
        };
        if !allowed {
            return (
                403,
                "text/plain".to_string(),
                b"you have no access to this instance".to_vec(),
            );
        }
    }
    let hours = hours.clamp(1, MAX_LINK_HOURS);
    let token = match crate::server::mint_link(
        store,
        instance_row.id,
        session,
        role,
        std::time::Duration::from_secs(hours * 3600),
    ) {
        Ok(token) => token,
        Err(err) => {
            tracing::warn!(error = %err, "link mint failed");
            return (500, "text/plain".to_string(), b"link mint failed".to_vec());
        }
    };
    // `session` is caller-controlled (a raw query-string value with no
    // charset restriction) and rides straight into markup below — escape the
    // whole path, not just the pieces that felt risky, or a session id like
    // `"><script>...` breaks out of the `data-path` attribute into a live
    // script tag on the anchor's own origin.
    let open_path = html_escape(&match session {
        Some(s) => format!("/{token}/s/{s}/"),
        None => format!("/{token}/"),
    });
    let mut body = layout_start("maki anchor — links", user, "links");
    body.push_str(&format!(
        "<h2>Link for {} ({})</h2><p><code>{token}</code></p>\
         <p>Open: <a href=\"{open_path}\">{open_path}</a> — expires in {hours}h</p>\
         <p><img id=\"qr\" data-path=\"{open_path}\" width=\"160\" height=\"160\" alt=\"share link qr\">\
         <script>const q=document.getElementById('qr');q.src='/qr?text='+encodeURIComponent(location.origin+q.dataset.path);</script></p>",
        html_escape(instance),
        role.as_str(),
    ));
    body.push_str("<p><a href=\"/links\">all links</a></p>");
    body.push_str(&layout_end());
    (200, "text/html".to_string(), body.into_bytes())
}

/// Cap hand-minted link lifetimes; the instance control link still refreshes
/// itself while connected.
pub(crate) const MAX_LINK_HOURS: u64 = 24 * 30;

pub(crate) fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Percent-encode a value for a query string, leaving the unreserved set.
fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Role;

    fn test_store() -> Store {
        // Leak the tempdir so the sqlite file lives for the test.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        std::mem::forget(dir);
        Arc::into_inner(Store::open(&path).unwrap()).expect("no other refs")
    }

    fn user(id: i64, is_admin: bool) -> UserRow {
        UserRow {
            id,
            oidc_sub: format!("sub-{id}"),
            email: None,
            name: None,
            is_admin,
        }
    }

    #[test]
    fn render_link_refuses_users_without_a_grant_on_the_instance() {
        let store = test_store();
        let instance = store.create_instance("gated", "hash").unwrap();
        store.upsert_user("admin-seed", None, None).unwrap();
        let stranger = store.upsert_user("sub-2", None, None).unwrap();
        assert!(!stranger.is_admin, "second user is a plain user");
        let (status, _, _) = render_link(&store, Some(&stranger), "gated", None, "control", 2);
        assert_eq!(status, 403, "a user with no grant must not mint");
        store
            .set_grant(stranger.id, instance, Role::Viewer)
            .unwrap();
        let (status, _, _) = render_link(&store, Some(&stranger), "gated", None, "view", 2);
        assert_eq!(status, 200, "a grant opens minting");
    }

    #[test]
    fn render_link_scopes_open_path_and_clamps_hours() {
        let store = test_store();
        store.create_instance("open", "hash").unwrap();
        let (status, _, body) = render_link(
            &store,
            Some(&user(1, true)),
            "open",
            Some("sid-9"),
            "view",
            100_000,
        );
        assert_eq!(status, 200);
        let html = String::from_utf8(body).unwrap();
        assert!(
            html.contains("/s/sid-9/"),
            "scoped link must carry the session: {html}"
        );
        assert!(
            html.contains(&format!("expires in {MAX_LINK_HOURS}h")),
            "hours clamped"
        );
    }

    #[test]
    fn render_link_escapes_the_instance_name() {
        let store = test_store();
        // CLI names are charset-checked, but a legacy row could carry HTML.
        store
            .register_instance("<img src=x onerror=alert(1)>", "hash")
            .unwrap();
        let (_, _, body) = render_link(
            &store,
            Some(&user(1, true)),
            "<img src=x onerror=alert(1)>",
            None,
            "view",
            2,
        );
        let html = String::from_utf8(body).unwrap();
        assert!(
            !html.contains("<img src=x"),
            "instance name must be escaped: {html}"
        );
        assert!(html.contains("&lt;img"), "escaped form expected: {html}");
    }

    #[test]
    fn render_link_escapes_the_session_id() {
        // `session` is a raw, caller-controlled query-string value with no
        // charset restriction — it must never ride straight into the
        // href/data-path markup it lands in.
        let store = test_store();
        store.create_instance("open", "hash").unwrap();
        let payload = "\"><script>alert(1)</script>";
        let (_, _, body) = render_link(
            &store,
            Some(&user(1, true)),
            "open",
            Some(payload),
            "view",
            2,
        );
        let html = String::from_utf8(body).unwrap();
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "session id must be escaped: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;"),
            "escaped form expected: {html}"
        );
    }

    #[test]
    fn render_link_refuses_a_viewer_minting_a_control_link() {
        // Only the rights a caller already holds (or admin) may be minted —
        // `rights` is a query param the caller picks freely, so a viewer
        // grant must not be enough to hand themselves a control link.
        let store = test_store();
        let instance = store.create_instance("gated", "hash").unwrap();
        store.upsert_user("admin-seed", None, None).unwrap(); // consumes the first-user-is-admin slot
        let viewer = store.upsert_user("sub-viewer", None, None).unwrap();
        assert!(!viewer.is_admin, "second user is a plain user");
        store.set_grant(viewer.id, instance, Role::Viewer).unwrap();
        let (status, _, _) = render_link(&store, Some(&viewer), "gated", None, "control", 2);
        assert_eq!(status, 403, "a viewer grant must not mint a control link");
        let (status, _, _) = render_link(&store, Some(&viewer), "gated", None, "view", 2);
        assert_eq!(status, 200, "a viewer grant may still mint a view link");

        let controller = store.upsert_user("sub-controller", None, None).unwrap();
        store
            .set_grant(controller.id, instance, Role::Controller)
            .unwrap();
        let (status, _, _) = render_link(&store, Some(&controller), "gated", None, "control", 2);
        assert_eq!(status, 200, "a controller grant may mint a control link");
    }

    #[test]
    fn urlencode_leaves_safe_chars_and_percent_encodes_the_rest() {
        assert_eq!(urlencode("work-laptop_1.local~"), "work-laptop_1.local~");
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn render_shows_pushed_cost_and_a_scoped_open_link() {
        let store = Arc::new(test_store());
        let instance = store.create_instance("host-a", "hash").unwrap();
        store
            .upsert_session(&crate::store::SessionRow {
                instance_id: instance,
                external_id: "sid-1".into(),
                title: "Refactor parser".into(),
                model: "claude".into(),
                cwd: "/work".into(),
                status: "idle".into(),
                cost_cents: 421,
                tokens_in: 1000,
                tokens_out: 200,
                context_window: 200_000,
                updated_at: crate::store::now_unix(),
                transcript: None,
                otr: None,
                prune_after: None,
            })
            .unwrap();
        let (_, _, body) = render_sessions(&store, None);
        let html = String::from_utf8(body).unwrap();
        assert!(html.contains("$4.21"), "cost should render: {html}");
        assert!(
            html.contains("/links?instance=host-a&amp;session=sid-1&amp;rights=control"),
            "open link should mint a scoped control link: {html}"
        );
    }

    #[test]
    fn the_pages_share_a_nav_and_split_the_old_dashboard() {
        let store = test_store();
        let instance = store.create_instance("nav-host", "hash").unwrap();
        store
            .create_link(
                "tok123",
                instance,
                None,
                "controller",
                std::time::Duration::from_secs(7200),
            )
            .unwrap();
        let hub = crate::hub::Hub::new();
        let admin = UserRow {
            id: 9,
            oidc_sub: "local:root".into(),
            email: None,
            name: None,
            is_admin: true,
        };
        let sessions_html = String::from_utf8(render_sessions(&store, Some(&admin)).2).unwrap();
        assert!(
            !sessions_html.contains("Live shares"),
            "shares moved off home to /links, so home isn't a redundant copy of it"
        );
        assert!(
            !sessions_html.contains("<h2>Instances</h2>"),
            "roster moved off home"
        );
        let instances_html = String::from_utf8(render_instances(&store, Some(&admin)).2).unwrap();
        assert!(instances_html.contains("nav-host"));
        assert!(
            instances_html.contains("Install on a new host"),
            "wizard lives here"
        );
        assert!(
            !instances_html.contains("Live shares"),
            "and only on /links"
        );
        let links_html = String::from_utf8(render_links(&store, &hub, Some(&admin)).2).unwrap();
        assert!(links_html.contains("Share a session"), "mint form");
        assert!(
            links_html.contains("tok123"),
            "live links carry their open token"
        );
        assert!(
            links_html.contains("href=\"/tok123/\""),
            "open href: {links_html}"
        );
        assert!(
            links_html.contains("revoke"),
            "admins get the revoke button"
        );
        for html in [&sessions_html, &instances_html, &links_html] {
            assert!(html.contains("class=\"active\""), "nav marks the page");
        }
    }

    #[test]
    fn revoke_all_and_bulk_delete_buttons_are_admin_only() {
        let store = test_store();
        let instance = store.create_instance("bulk-host", "hash").unwrap();
        store
            .create_link(
                "tok-bulk",
                instance,
                None,
                "view",
                std::time::Duration::from_secs(7200),
            )
            .unwrap();
        let hub = crate::hub::Hub::new();
        let admin = user(1, true);
        store.upsert_user("admin-seed", None, None).unwrap();
        let plain = store.upsert_user("plain-sub", None, None).unwrap();
        assert!(!plain.is_admin);
        store.set_grant(plain.id, instance, Role::Viewer).unwrap();

        let admin_links = String::from_utf8(render_links(&store, &hub, Some(&admin)).2).unwrap();
        assert!(
            admin_links.contains("id=\"revoke-all\""),
            "admin gets the bulk revoke button: {admin_links}"
        );
        let plain_links = String::from_utf8(render_links(&store, &hub, Some(&plain)).2).unwrap();
        assert!(
            !plain_links.contains("id=\"revoke-all\""),
            "a non-admin gets no bulk revoke, same as the single revoke button: {plain_links}"
        );

        let admin_instances = String::from_utf8(render_instances(&store, Some(&admin)).2).unwrap();
        assert!(
            admin_instances.contains("delete all sessions"),
            "admin gets the bulk session delete button: {admin_instances}"
        );
        let plain_instances = String::from_utf8(render_instances(&store, Some(&plain)).2).unwrap();
        assert!(
            !plain_instances.contains("delete all sessions"),
            "a non-admin must not get a bulk-delete button: {plain_instances}"
        );
    }

    #[test]
    fn non_admins_see_links_only_for_granted_instances() {
        let store = test_store();
        let mine = store.create_instance("mine", "hash").unwrap();
        let theirs = store.create_instance("theirs", "hash2").unwrap();
        store
            .create_link(
                "t-mine",
                mine,
                None,
                "view",
                std::time::Duration::from_secs(7200),
            )
            .unwrap();
        store
            .create_link(
                "t-theirs",
                theirs,
                None,
                "view",
                std::time::Duration::from_secs(7200),
            )
            .unwrap();
        store.upsert_user("stranger", None, None).unwrap();
        store
            .create_local_user("stranger", "pw", None, None, false)
            .unwrap();
        let stranger = store.verify_local_user("stranger", "pw").unwrap();
        assert!(!stranger.is_admin);
        let hub = crate::hub::Hub::new();
        let html = String::from_utf8(render_links(&store, &hub, Some(&stranger)).2).unwrap();
        assert!(
            !html.contains("t-mine") && !html.contains("t-theirs"),
            "no grants yet: {html}"
        );
        store
            .set_grant(stranger.id, mine, crate::store::Role::Viewer)
            .unwrap();
        let html = String::from_utf8(render_links(&store, &hub, Some(&stranger)).2).unwrap();
        assert!(html.contains("t-mine"), "granted instance shows");
        assert!(!html.contains("t-theirs"), "the rest stays invisible");
        assert!(!html.contains("revoke"), "revoking is an admin privilege");
    }
}
