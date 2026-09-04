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

```
maki-anchor serve --bind 0.0.0.0:8688
```

This serves the web UI on port 8688 and the instance tunnel on 8689. Put your
reverse proxy in front for TLS, and point the tunnel port at the same host.
The proxy must forward WebSocket upgrades to the tunnel port.

The tunnel is a second listener because `tiny_http` owns the HTTP port's
accept loop, and a tunnel is a long-lived full-duplex socket, not a
request/response. It does not have to be public. With `--ws-bind 127.0.0.1`
the tunnel binds to loopback only and your reverse proxy fronts both behind
one origin, forwarding `/ws` to `127.0.0.1:8689`:

```
maki-anchor serve --bind 0.0.0.0:8688 --ws-bind 127.0.0.1
```

Only the HTTP port then needs a firewall/TLS path. Use `--ws-bind host:port`
to pin the tunnel to an explicit port as well.

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
anchor instead of binding a local port, and prints a share link minted by the
anchor. Without it, `/rc` behaves as described in
[Commands](/docs/commands/).

## Share links

Every tunnel gets a fresh control link when it connects. Traffic and
keepalives slide its expiry forward, so it stays valid while the tunnel lives.
You can also mint links by hand:

```
maki-anchor tokens link work-laptop --rights view --ttl-hours 2
maki-anchor tokens link work-laptop --rights control --session <session id>
maki-anchor tokens revoke <token>
```

A link is `/{token}/` under the anchor domain. `view` links open the session
read-only; `control` links can prompt, answer permission requests, and stop
runs. A `--session` scoped link only opens that one session, and every request
under it is routed to that tab rather than the focused one. The dashboard
Sessions table lists each instance's live sessions with their pushed cost and
an **open** link that mints a two-hour control link scoped to that session.

## Login and roles

The anchor supports OIDC single sign-on. Point it at any standard provider
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

Without `[oidc]` the anchor runs in LAN trust mode: no login, and everyone who
reaches the dashboard is treated as trusted. Only use this mode on a network
you control.

## Data model

| Table | Purpose |
|---|---|
| `instances` | Registered hosts, one per registration token |
| `sessions` | Session index, updated by the instances themselves |
| `links` | Share links with rights and expiry |
| `users` | OIDC identities; the first login becomes admin |
| `oidc_sessions` | Browser cookie sessions |
| `grants` | Per-user rights per instance |