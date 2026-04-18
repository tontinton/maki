default:
    @just --list

build *ARGS:
    cargo build --all-features {{ARGS}}

run *ARGS:
    cargo run --all-features {{ARGS}}

test *ARGS:
    cargo nextest run --all-features --workspace {{ARGS}}

lint:
    cargo clippy --all-features --all --tests -- -D warnings

lint-fix:
    cargo clippy --all-features --all --tests --fix

fmt-check:
    cargo fmt --all -- --check
    stylua --check plugins/

fmt:
    cargo fmt --all
    stylua plugins/

pylint:
    ruff check scripts/
    ty check scripts/

gen-docs:
    cargo run -p maki-docgen

gen-docs-check:
    cargo run -p maki-docgen -- --check

# Full CI check
ci: fmt-check lint pylint test gen-docs-check
