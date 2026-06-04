ci := env("CI", "")

# cargo build --release
[group('build')]
build:
    cargo build --release

# cargo nextest run --release
[group('build')]
test:
    cargo nextest run --release {{ if ci != "" { "--profile ci" } else { "" } }}

# cargo clippy (release, all targets, warnings as errors)
[group('build')]
clippy:
    cargo clippy --release --all-targets -- -D warnings

# cargo fmt (--check in CI, fix locally)
[group('build')]
fmt:
    cargo fmt --all {{ if ci != "" { "-- --check" } else { "" } }}

# cargo deny check (advisories, licenses, bans, sources)
[group('build')]
deny:
    cargo deny check

# build + test + clippy + fmt + deny + coverage (mirrors the CI gate)
[group('build')]
check: build test clippy fmt deny coverage

# cargo llvm-cov (coverage; writes lcov.info in CI)
[group('build')]
coverage:
    cargo llvm-cov nextest --release {{ if ci != "" { "--lcov --output-path lcov.info" } else { "" } }}

# cargo run --release -- {{ARGS}}
[group('run')]
run *ARGS:
    cargo run --release -- {{ARGS}}

# release-plz (dry-run by default; pass `release-plz release` to publish)
[group('release')]
release *ARGS="--help":
    release-plz {{ARGS}}
