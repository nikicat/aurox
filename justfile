# Project recipes. Run `just` to list, `just <recipe>` to invoke.
#
# Coverage uses cargo-llvm-cov (LLVM source-based coverage).
# Install once:  cargo install cargo-llvm-cov
# Or via pacman: pacman -S cargo-llvm-cov

# Filename regex passed to llvm-cov to drop CLI glue + test helpers from the
# report. We measure the *library*: src/main.rs is a thin wrapper around
# cli::run, and src/testing.rs is the shared #[doc(hidden)] fixture module
# consumed by tests/ (see src/lib.rs).
ignore_regex := '(examples/|src/main\.rs|src/testing\.rs)'

# List available recipes.
default:
    @just --list

# Run the full test suite (matches CI).
test:
    cargo test --all-features --locked

# Coverage summary in the terminal.
coverage:
    cargo llvm-cov --all-features --ignore-filename-regex '{{ignore_regex}}'

# HTML report at target/llvm-cov/html/index.html.
coverage-html:
    cargo llvm-cov --all-features --html --ignore-filename-regex '{{ignore_regex}}'

# HTML report + open in browser.
coverage-open:
    cargo llvm-cov --all-features --html --open --ignore-filename-regex '{{ignore_regex}}'

# lcov.info for Codecov upload or external tools.
coverage-lcov:
    cargo llvm-cov --all-features --lcov --output-path lcov.info \
        --ignore-filename-regex '{{ignore_regex}}'

# Drop cached .profraw / .profdata / HTML report.
coverage-clean:
    cargo llvm-cov clean --workspace
