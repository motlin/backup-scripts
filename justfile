set dotenv-filename := ".envrc"

import ".just/console.just"
import ".just/cargo.just"
import ".just/git.just"
import ".just/git-test.just"

# `just --list --unsorted`
default:
    @just --list --unsorted

# `mise install`
mise:
    mise install --quiet
    mise current

# Install the toolchain via mise
[group('setup')]
install:
    mise install

# clean (git only - cargo artifacts live in target/ and are gitignored)
@clean: _clean-git

# `just check` then run all pre-commit hooks
verify: check
    pre-commit run --all-files
    @echo "All pre-commit checks passed!"

# Override this with a command called `woof` which notifies you in whatever ways you prefer.
echo_command := env('ECHO_COMMAND', "echo")
