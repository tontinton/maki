+++
title = "Lua Packages"
weight = 11
[extra]
group = "Reference"
+++

# Lua packages

A Lua package lets you add tools, commands, keybindings, and event handlers
without copying its code into `init.lua`. Maki can load a package that you put
on disk, or it can install one from a Git repository and lock it to one commit.

## Install from Git

Declare managed packages in the global `init.lua`, normally
`~/.config/maki/init.lua`:

```lua
maki.pack.add({
  "https://github.com/example/maki-goal",
  {
    src = "https://github.com/example/maki-review",
    version = "v1.2.0",
  },
})
```

Maki shows all new packages in one install prompt. It writes the selected Git
commit to `pack-lock.json` in your config directory. Commit this file if you
want another machine to install the same revisions.

Package installation is global. A project `.maki/init.lua` cannot add packages
because all projects share the same lockfile and package directory. Project
config and packages can inspect installed state with `maki.pack.get` and can
activate an installed package with `maki.packadd`.

Maki restores a missing checkout only when the global config still declares
the package and its source matches the lockfile entry. Package and approval
changes use one process lock. The kernel releases this lock when the process
exits, including after a crash or forced termination.

Set `confirm = false` only when the package source is already trusted and Maki
must run without a terminal:

```lua
maki.pack.add({ "https://github.com/example/maki-goal" }, {
  confirm = false,
})
```

This option skips the install prompt. It does not approve package permissions.
Maki rejects an HTTP source that contains a username, password, or token because
Git and the lockfile would store it. Use a Git credential helper or an SSH agent
instead.

Set `load = false` to install a package without loading it at startup. Set
`load` to a function when the package needs a custom entry point:

```lua
maki.pack.add({
  {
    src = "https://github.com/example/maki-review",
    data = { module = "review" },
  },
}, {
  load = function(package)
    require(package.spec.data.module).setup()
  end,
})
```

The function runs as the package owner. It receives the package `spec` and its
installed `path`. The `data` field can contain any Lua value.

## Package permissions

A managed package can ask for guarded APIs in `plugin.toml`:

```toml
[permissions]
fs_read = true
fs_write = true
net = true
run = true
env = true
```

The file is a request, not an approval. Maki asks about new permissions in a
separate prompt. An approval applies only to the same package name and source.
Maki keeps approvals in `<maki-data>/site/pack-approvals.json`. They describe
trust on this machine and must not be committed with `pack-lock.json`.
Non-interactive modes report the package as unavailable until the install and
permission decisions are made in an interactive terminal.

## Install by hand

Clone a package into a Neovim-style package directory:

```text
<maki-data>/site/pack/<group>/start/<name>/
<maki-data>/site/pack/<group>/opt/<name>/
```

A `start` package loads at startup. An `opt` package stays installed until
something activates it.

Placing a package by hand means you trust its code the way you trust your own
`init.lua`. Maki grants such a package exactly the permissions its
`plugin.toml` requests, with no approval step. A managed install is the
opposite: Maki asks before it clones a source and again before it grants the
permissions. Read the code before you copy it in, and keep manual placement for
the packages you develop yourself.

Activate an `opt` package with `maki.packadd`, from `init.lua` or from another
package:

```lua
maki.packadd("my-package")
```

Maki loads the named package after the calling Lua task returns. A package
that `maki.packadd` activates can activate another one in turn. Maki reports a
name that no installed package matches, and refuses a package that
`plugins.<name>.enabled = false` disabled.

A package directory can contain sorted `plugin/*.lua` entry files and modules
at `lua/<module>.lua` or `lua/<module>/init.lua`.

Managed packages use immutable revision directories under
`<maki-data>/site/pack/core/<name>/<commit>/`. A new revision creates a new
directory. A running package holds a shared lock on its revision. At startup,
Maki removes each stale revision only when it can take the matching exclusive
lock. Revisions used by another Maki process stay on disk.
