# `just --list --unsorted`
[group('default')]
default:
    @just --list --unsorted

ci := env("CI", "")

# Install toolchain via mise
[group('setup')]
install:
    mise install

# cargo build --release
[group('build')]
build:
    cargo build --release

# cargo test --release
[group('build')]
test:
    cargo test --release

# cargo clippy --release --all-targets -- -D warnings
[group('build')]
clippy:
    cargo clippy --release --all-targets -- -D warnings

# cargo fmt (--check in CI, fix locally)
[group('build')]
fmt:
    cargo fmt --all {{ if ci != "" { "-- --check" } else { "" } }}

# build + test + clippy + fmt
[group('build')]
check: build test clippy fmt

# cargo run --release -- {{ARGS}}
[group('run')]
run *ARGS:
    cargo run --release -- {{ARGS}}

# Run all pre-commit checks
[group('build')]
precommit: check
    pre-commit run --all-files
    @echo "All pre-commit checks passed!"
