default:
    @just --list

# cargo build --release
build:
    cargo build --release

# cargo test --release
test:
    cargo test --release

# cargo clippy --release --all-targets -- -D warnings
clippy:
    cargo clippy --release --all-targets -- -D warnings

# cargo fmt
fmt:
    cargo fmt

# cargo fmt --check
fmt-check:
    cargo fmt --check

# build + test + clippy (matches git-test default)
check: build test clippy

# cargo run --release --
run *ARGS:
    cargo run --release -- {{ARGS}}
