+++
title = "Notifications"
weight = 9
[extra]
group = "Reference"
+++

# Notifications

Maki can tell you when a session finishes or needs your input. This is useful
when you move to another terminal while Maki works.

Notifications are enabled by default. A prompt that waits on you always
notifies, because the agent stays parked until you answer. `Agent turn
complete` comes once Maki has nothing left to do. If background tasks still
run when a turn ends, it waits for them and for any turn their results start.
It is skipped when Maki can tell you are watching:
the terminal reported focus, or you typed in the last 30 seconds.

Maki uses these messages:

- `Agent turn complete` or a preview of the response, up to 200 characters.
- `Permission requested: <tool>` for a permission prompt.
- `Authentication required` when authentication needs attention.
- `Question requested` for a question prompt.
- `Plan ready` when a plan is ready.

Response previews can appear in your operating system's notification history.
Maki does not include tool arguments, permission scopes, question bodies, plan
content, or error details. Use `bell` for a message-free alert, or use `off`
to disable notifications if response text should not reach notification
history.

## Configuration

Set `ui.notifications` in `~/.config/maki/init.lua`:

```lua
maki.setup({
  ui = {
    notifications = "auto",
  },
})
```

| Value | Behavior |
| --- | --- |
| `auto` | Use OSC 9 in a supported terminal. Use BEL otherwise. |
| `osc9` | Always send an OSC 9 notification. |
| `bell` | Always send the terminal bell. |
| `notify` | Show a desktop notification. |
| `off` | Do not send notifications. |

`auto` supports Ghostty, iTerm2, Kitty, Warp, and WezTerm. An unknown terminal
uses BEL. Your terminal settings decide whether BEL makes a sound or shows a
visual alert.

Maki also recognizes `xterm-ghostty` and `xterm-kitty` from `TERM`. This lets
OSC 9 work when an SSH connection does not preserve `TERM_PROGRAM`.

`notify` works on Linux, macOS, BSD, and Windows. On Linux it needs a D-Bus
session bus. The notification appears on the machine that runs Maki, so it
does not reach you over SSH.

## Program status

Maki also reports the current state to the terminal with OSC 7501, the
[Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status).
A supported terminal can show it next to the tab, so you can see which tab
waits on you. Other terminals ignore it.

| State | When |
| --- | --- |
| `idle` | Nothing runs or waits. |
| `working` | A turn or a background task runs. |
| `blocked` | A permission prompt, question, plan, or login waits on you. |
| `done` | The last turn finished. |
| `error` | The last turn stopped with an error. |

With several sessions open, Maki reports the most urgent one. The status clears
when Maki exits. With `--exit-on-done` the final `done` or `error` stays.

Set `ui.program_status = false` to turn it off.

## tmux

OSC 9 and OSC 7501 need passthrough:

```tmux
set -g allow-passthrough all
```

Use `allow-passthrough all`, not `allow-passthrough on`. The `on` value permits
passthrough only while the Maki pane is visible. tmux drops the notification
or status report after you change to another tmux window. Each pane reports
its own status, so several Maki panes do not overwrite each other.

Focus events are a separate setting:

```tmux
set -g focus-events on
```

This lets Maki suppress a turn completion you are already watching. Without it
Maki falls back to your last keypress and notifies for anything slower than 30
seconds.

Add the settings to `~/.tmux.conf`, then reload the file or restart tmux.

## Other terminal multiplexers

Maki wraps OSC 9 and OSC 7501 for GNU screen. GNU screen does not pass terminal
focus events to Maki, so Maki does not suppress notifications there. A notification
can appear while the GNU screen window has focus.

Maki sends OSC 9 and OSC 7501 directly through Zellij.

## Herdr

[Herdr](https://herdr.dev) forwards BEL to the terminal you attach from, so
`auto` uses BEL there. Herdr does not show OSC 9, so `osc9` shows nothing
inside Herdr.

Herdr tracks Maki panes and plays its own sound when one is blocked or done.
With both on you can hear two alerts. Set `off` to leave alerts to Herdr, or
keep `bell` if you want the terminal window flagged too.

Maki detects Herdr from `TERM_PROGRAM=herdr`. Inside tmux or GNU screen started
in a Herdr pane, Maki talks to that multiplexer instead.

## Focus on Windows

This terminal focus protocol is not available on Windows. Maki treats the
terminal as unfocused so an explicit `bell` or `osc9` setting still works.
