default:
    @just --list

build *ARGS:
    cargo build {{ARGS}}

# Types only, no codegen, no lints. Add `-p <crate>` to make it cheaper still.
check *ARGS:
    cargo check --workspace --all-targets {{ARGS}}

run *ARGS:
    cargo run {{ARGS}}

install *ARGS:
    cargo install --locked --path . {{ARGS}}

test *ARGS:
    cargo nextest run --workspace {{ARGS}}

lint:
    cargo clippy --all --all-targets -- -D warnings

# Narrow it down, e.g. `just bench -p maki-ui --bench input`.
bench *ARGS:
    cargo bench {{ARGS}}

# Dev profile: builds fast, to see a bench run. Its numbers mean little.
bench-quick *ARGS:
    cargo bench --profile dev {{ARGS}}

lint-fix:
    cargo clippy --all --tests --fix

fmt-check:
    cargo fmt --all -- --check
    stylua --check plugins/

fmt:
    cargo fmt --all
    stylua plugins/

# The files the Lua checks run on, from `plugins/`. Definition files are left out, since lute's lint
# parser rejects their `declare` syntax.
lua_files := "$(find . \\( -name '*.lua' -o -name '*.luau' \\) ! -name '*.d.luau')"

# CI runs the Lua checks through these recipes. lute reads `plugins/.config.luau` from the working directory.
# Not part of `ci` yet: they need the lute nightly pinned in `.github/workflows/rust.yml`.
check-lua:
    cd plugins && lute check {{lua_files}}

# lute's default rules plus maki's own in `plugins/lint`. Prefix a deliberately unused variable with `_`.
# `--sequential`: the default parallel mode (one `vm.create` worker per core) races in lute's
# cross-VM runtime and crashes ~10% of runs (SIGSEGV, or "Cannot resume a non-thread reference").
lint-lua:
    cd plugins && lute lint --sequential {{lua_files}}

# Luau's built-in lints, which `lute check` doesn't run. luau-analyze can't load `maki.d.luau`,
# so its type errors are left to `lute check`, and it exits 0 on findings.
analyze-lua:
    cd plugins && ! luau-analyze --formatter=plain {{lua_files}} | grep -vE ': \([EW][0-9]+\) TypeError:'

pylint:
    ruff check scripts/
    ty check scripts/

gen-docs:
    cargo run -p maki-docgen

gen-docs-check:
    cargo run -p maki-docgen -- --check

machete:
    cargo machete

# Full CI check
ci: fmt-check lint pylint test gen-docs-check machete
