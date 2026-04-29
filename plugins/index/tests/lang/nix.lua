local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("nix_imports", function()
  local src = [==[
    { pkgs ? import <nixpkgs> {} }:
    let
      utils = import ./utils.nix;
    in
    pkgs.stdenv.mkDerivation {
      name = "test";
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "<nixpkgs>",
    "./utils.nix",
  })
end)

case("nix_bindings", function()
  local src = [==[
    rec {
      hello = "world";
      name = "maki";
      version = "0.1.0";
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "consts:",
    "hello",
    "name",
    "version",
  })
end)

case("nix_function_signature", function()
  local src = [==[
    { pkgs ? import <nixpkgs> {}, lib, stdenv }:
    null
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "fns:",
    "pkgs",
    "lib",
    "stdenv",
  })
end)

case("nix_all_sections", function()
  local src = [==[
    { pkgs, lib ? import ./lib.nix, stdenv, ... }:
    let
      utils = import ./utils.nix;
      name = "maki";
      version = "0.1.0";
    in
    pkgs.stdenv.mkDerivation {
      pname = name;
      version = version;
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "lib.nix",
    "utils.nix",
    "fns:",
    "pkgs",
    "lib",
    "stdenv",
    "consts:",
    "name",
    "version",
  })
end)

case("nix_universal_only", function()
  local src = [==[
    x:
    x + 1
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "fns:",
    "x",
  })
end)

case("nix_universal_with_formals", function()
  local src = [==[
    args@{ pkgs, lib, stdenv }:
    pkgs.stdenv.mkDerivation {
      pname = "test";
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "fns:",
    "args",
    "pkgs",
    "lib",
    "stdenv",
  })
end)

case("nix_formals_with_universal", function()
  local src = [==[
    { pkgs, lib }@args:
    null
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "fns:",
    "pkgs",
    "lib",
    "args",
  })
end)

case("nix_binding_with_universal", function()
  local src = [==[
    rec {
      myfun = x:
        x + 1;
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "fns:",
    "myfun(x)",
  })
end)

case("nix_binding_function_no_duplicate_param", function()
  local src = [==[
    rec {
      myfun = x: x + 1;
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "myfun(x)",
  })
  lacks(out, {
    "\n  x ",
  })
end)

case("nix_binding_function_multiple_params_no_duplicate", function()
  local src = [==[
    rec {
      add = { a, b }: a + b;
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "add(a, b)",
  })
  lacks(out, {
    "\n  a ",
    "\n  b ",
  })
end)

case("nix_nested_bindings", function()
  local src = [==[
    rec {
      outer = {
        inner = x: x + 1;
        name = "hello";
      };
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "outer",
    "inner(x)",
    "name",
  })
end)

case("nix_inside_function_body", function()
  local src = [==[
    rec {
      outputs = { self, nixpkgs }: {
        packages.default = "pkg";
        devShells.default = "shell";
      };
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "fns:",
    "outputs(self, nixpkgs)",
    "packages.default",
    "devShells.default",
  })
  lacks(out, {
    "consts:",
  })
end)

case("nix_nested_attrset_no_leak", function()
  local src = [==[
    pkgs.stdenv.mkDerivation {
      pname = name;
      version = version;
    }
  ]==]
  local out = idx(src, "nix")
  lacks(out, {
    "pname",
    "version",
  })
end)

case("nix_nested_function_body_preserves_children", function()
  local src = [==[
    {
      outputs = { self, nixpkgs }: {
        config = { pkgs, lib }: {
          enable = true;
          foo = "bar";
        };
      };
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "outputs(self, nixpkgs)",
    "config(pkgs, lib)",
    "enable",
    "foo",
  })
end)

case("nix_let_in_function_body_preserves_children", function()
  local src = [==[
    {
      outputs = { self, nixpkgs }: let
        inner1 = 1;
      in {
        config = { pkgs, lib }: {
          enable = true;
          foo = "bar";
        };
      };
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "outputs(self, nixpkgs)",
    "inner1",
    "config(pkgs, lib)",
    "enable",
    "foo",
  })
end)

case("nix_imports_list", function()
  local src = [==[
    {
      home.stateVersion = "23.05";
      imports = [ ../modules/core ./shared.nix ];
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "core",
    "shared.nix",
  })
end)

case("nix_imports_list_nested", function()
  local src = [==[
    {
      home-manager.users.barnaby = { pkgs, ... }: {
        imports = [ ../modules/core ];
      };
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "../modules/core",
  })
end)

case("nix_imports_list_with_nested_imports", function()
  local src = [==[
    {
      imports = [
        ./foo.nix
        (import ./bar.nix)
        (builtins.fetchTarball "channel:nixos-21.11")
      ];
    }
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "foo.nix",
    "bar.nix",
    "channel:nixos-21.11",
  })
end)

case("nix_import_in_formal_default", function()
  local src = [==[
    { pkgs, lib ? import ./lib.nix, ... }:
    pkgs
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "lib.nix",
  })
end)

case("nix_nested_import_in_apply_arg", function()
  local src = [==[
    foo (import ./mod.nix)
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "mod.nix",
  })
end)

case("nix_nested_import_in_callpackage", function()
  local src = [==[
    callPackage (import ./pkg.nix) {}
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "pkg.nix",
  })
end)

case("nix_uri_import_preserves_scheme", function()
  local src = [==[
    import https://example.com/a.nix
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "https://example.com/a.nix",
  })
end)

case("nix_absolute_path_import", function()
  local src = [==[
    import /etc/nixos/configuration.nix
  ]==]
  local out = idx(src, "nix")
  has(out, {
    "imports:",
    "/etc/nixos/configuration.nix",
  })
end)
