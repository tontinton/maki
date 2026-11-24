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

# Same command as CI. lute reads `plugins/.config.luau` from the working directory.
# Not part of `ci` yet: it needs the lute nightly pinned in `.github/workflows/rust.yml`.
check-lua:
    cd plugins && lute check $(find . \( -name '*.lua' -o -name '*.luau' \) ! -name '*.d.luau')

# Same command as CI: lute's default rules plus maki's own in `plugins/lint`. Prefix a deliberately unused variable with `_`.
lint-lua:
    cd plugins && lute lint --sequential $(find . \( -name '*.lua' -o -name '*.luau' \) ! -name '*.d.luau')

# Same command as CI: Luau's built-in lints, minus the type errors `lute check` covers.
analyze-lua:
    cd plugins && ! luau-analyze --formatter=plain $(find . \( -name '*.lua' -o -name '*.luau' \) ! -name '*.d.luau') | grep -vE ': \([EW][0-9]+\) TypeError:'

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
