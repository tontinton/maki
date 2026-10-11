default:
    @just --list

build *ARGS:
    cargo build {{ARGS}}

# Types only, no codegen, no lints. Add `-p <crate>` to make it cheaper still.
check *ARGS:
    cargo check --workspace --tests {{ARGS}}

run *ARGS:
    cargo run {{ARGS}}

install *ARGS:
    cargo install --locked --path . {{ARGS}}

test *ARGS:
    cargo nextest run --workspace {{ARGS}}

lint:
    cargo clippy --all --tests -- -D warnings

# Dev profile on purpose: a release build of maki-ui takes too long to wait on.
# Narrow it down, e.g. `just bench -p maki-ui --bench input`.
bench *ARGS:
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
    cd plugins && lute check $(find . -name '*.lua')

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
