+++
title = "Lua Packages"
weight = 11
[extra]
group = "Reference"
+++

# Lua packages

A Lua package lets you add tools, commands, keybindings, and event handlers
without copying its code into `init.lua`.

Clone a package into a Neovim-style package directory:

```text
<maki-data>/site/pack/<group>/start/<name>/
<maki-data>/site/pack/<group>/opt/<name>/
```

A `start` package loads at startup. An `opt` package stays installed until
something activates it.

Placing a package by hand means you trust its code the way you trust your own
`init.lua`. Maki grants such a package exactly the permissions its
`plugin.toml` requests, with no approval step. Read the code before you copy it
in, and keep manual placement for the packages you develop yourself.

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
