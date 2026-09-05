+++
title = "Anchor"
weight = 13
[extra]
group = "Reference"
+++

# Anchor

The anchor is a small server that lets many maki instances share one domain,
one login, and one dashboard. An instance dials the anchor over an outbound
WebSocket, so it needs no inbound port and works behind NAT or a firewall.

```
browser ──HTTPS──> anchor (login, grants, dashboard, routing)
                     ▲            ▲
        outbound WS  │            │  outbound WS
                     │            │
               maki host A   maki host B
```

The anchor stores session metadata only: title, model, cost, status.
Transcripts are fetched live from the owning instance when you open a page, so
nothing sensitive lands on the anchor host. An offline instance still shows in
the index, but its transcript is unavailable until it reconnects.

## Run the anchor

One line installs or updates the anchor as a systemd service. As root it
writes a system unit under `/var/lib/maki-anchor`; as a regular user it writes
a user unit under `~/.local/state/maki-anchor` and enables linger. Re-run the
same line to update: the binary swaps and the service restarts.

```
curl -fsSL https://raw.githubusercontent.com/wmantly/maki/main/install-anchor.sh | sh
```

Environment overrides: `MAKI_INSTALL_REPO` (release source), `MAKI_ANCHOR_VERSION`
(a tag, default latest), `MAKI_ANCHOR_BIND` and `MAKI_ANCHOR_ALLOW_LOCAL`
(both only seed a fresh config file).

The first install writes a config file: `/etc/maki/anchor.toml` for a system
unit, `~/.config/maki-anchor/anchor.toml` for a user unit. It sets the bind
address, the local-login switch, the mint policy, and (commented out) the OIDC
block. Updates never touch it. Edit and restart:

```toml
bind = "127.0.0.1:8688"   # keep local behind a reverse proxy

[auth]
allow_local_users = true
# mint_tokens = "admin"   # any | user | admin
```

```
systemctl restart maki-anchor              # system unit
systemctl --user restart maki-anchor       # user unit
```

Or run it directly (the `--bind` flag beats the file):

```
maki-anchor serve --config /etc/maki/anchor.toml
```

One port serves everything: the dashboard, the browser API, and the instance
tunnels (WebSocket upgrades on `/ws`). Put your reverse proxy in front for TLS
and forward all traffic to that one listener; no special routing for `/ws` is
needed beyond ordinary WebSocket forwarding.

The anchor writes its database next to the working directory as
`maki-anchor.sqlite3`, and reads optional OIDC settings from
`maki-anchor.toml`.

## Register an instance

Create a registration token, then put it in maki's config on the instance
host:

```
maki-anchor tokens add work-laptop
```

In `~/.config/maki/init.lua`:

```lua
maki.setup {
  anchor = {
    url = "https://maki.example.com",
    name = "work-laptop",
    token = "<the token from tokens add>",
  },
}
```

All three fields are required together. With `[anchor]` set, `/rc` dials the
anchor instead of binding a local port, and prints the full share URL minted
by the anchor. Without it, `/rc` behaves as described in
[Commands](/docs/commands/).

To make every session remote from launch without typing `/rc`, set
`remote_control = { auto_start = true }` in `maki.setup`.

If the link drops (an anchor restart, a network blip), the client reconnects on
its own with a capped backoff and re-registers under the same URL. The status
bar shows `remote` while the link is up, and `remote·N` while N browsers watch
that tab.

`/rc` in the TUI speaks three verbs: bare `/rc` starts the link, or reports it
(URL, QR, watchers per tab) when one is already running; `/rc off` disconnects
and keeps the link alive for the next start; `/rc down` disconnects and has the
anchor revoke the link, so shared URLs stop working immediately.

## Share links

Every tunnel registers under a control link. Reconnects and repeated `/rc`
calls reuse the still-live link, so one URL survives anchor restarts and
client flaps. Traffic and keepalives slide its expiry forward, so it stays
valid while the tunnel lives.
You can also mint links by hand:

```
maki-anchor tokens link work-laptop --rights view --ttl-hours 2
maki-anchor tokens link work-laptop --rights control --session <session id>
maki-anchor tokens revoke <token>
```

A link is `/{token}/` under the anchor domain. `view` links open the session
read-only; `control` links can prompt, answer permission requests, and stop
runs. A `--session` scoped link only opens that one session, and every request
under it is routed to that tab rather than the focused one. Share links are
capability-only: anyone holding one reaches the session without an anchor
login. The management pages below the domain root are a separate surface and
always require a login.

Every link the anchor created appears on the **Links** page (and on the
dashboard home while it is live) with an open button, so minted URLs never have
to becopied from scrollback.

## Pages

| Page | Contents |
|---|---|
| `/` (home) | Live shares: every unexpired link, its instance, tunnel state, and an open button. Below it, each instance's sessions with pushed cost and an **open** action that mints a two-hour control link scoped to that session |
| `/instances` | The install wizard (create an instance, copy the one-liner) and the fleet roster |
| `/links` | Mint a share link and revoke live ones |
| `/admin` | Users, grants, mint policy (admins only) |

Non-admins see the pages filtered to instances they hold a grant for.

## The remote session page

Beyond the transcript, the page (both anchor and standalone) carries a control
center under the top-left menu: live watcher counts per tab, and, for anchor
users with a controller grant on the instance, invite links (mint and revoke)
plus a button to close the current link. Callers without a login just see
watchers.

The composer takes uploads: a `+` button attaches images to the next prompt
for vision models, or saves any file into the session's working directory and
tells the agent where it landed. Each chip toggles between the two modes.
`/rc` and the link pages render the share URL as a QR, scannable straight off
the terminal.

## Login and roles

A fresh anchor opens on a setup page: the first username and password you
submit becomes the admin account, and the page closes behind it. Every
management page then requires a login. Share links stay reachable by token
alone.

The anchor also supports OIDC single sign-on. Point it at any standard provider
(Authelia, Authentik, Keycloak, Pocket ID, Google):

```toml
[oidc]
issuer = "https://auth.example.com/realms/main"
client_id = "maki-anchor"
client_secret = "..."
origin = "https://maki.example.com"
```

The callback URL to register with your provider is `{origin}/callback`. The
first user to log in becomes an admin; everyone after that is a regular user
until you grant them access.

Access is per instance. A user with a grant sees that instance on the
dashboard and can open its sessions:

```
maki-anchor grants set <user id> work-laptop control
maki-anchor grants list
maki-anchor grants revoke <user id> work-laptop
maki-anchor grants lookup <oidc sub>   # find the user id after first login
```

Local accounts work with or without OIDC: the setup page creates one, and so
does `maki-anchor users add <name> --admin`. Log in at `/login`, which offers
the password form whenever a local account exists, alongside SSO when
configured.

## Data model

| Table | Purpose |
|---|---|
| `instances` | Registered hosts, one per registration token |
| `sessions` | Session index, updated by the instances themselves |
| `links` | Share links with rights and expiry |
| `users` | OIDC and local identities; first-run setup creates the admin |
| `oidc_sessions` | Browser cookie sessions |
| `grants` | Per-user rights per instance |