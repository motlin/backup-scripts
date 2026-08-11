# Repository Conventions

## Toolchain

Toolchain versions are pinned in `.mise/config.toml`. Run `mise install` (or `just install`) to provision them.

## Build & Check

Use `just` recipes — they wrap the underlying commands so local and CI invocations stay in sync.

- `just check` — build, test, clippy, fmt
- `just verify` — `just check` plus `pre-commit run --all-files`

## Pre-commit

`.pre-commit-config.yaml` configures hooks. Run `pre-commit install` once after cloning to enable the git hook, or `pre-commit run --all-files` to lint the whole tree on demand.

## Style

- LF line endings everywhere (enforced via `.gitattributes` and the `mixed-line-ending` pre-commit hook).
- Markdown files are linted by `markdownlint-cli2` against `.markdownlint.jsonc`.
- YAML files follow the rules in `.yamllint.yaml`.
