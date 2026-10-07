# Contributing to zerobrew

Thanks for your interest in contributing to zerobrew! This document provides guidelines for contributing to the project.

## Licensing

By contributing to zerobrew, you agree your contributions will be dual-licensed under either [Apache](./LICENSE-APACHE.md) OR [MIT](./LICENSE-MIT.md), at the licensee's choice.

## Soft Prerequisites

- Rust 1.90 or later
- Access to either a macOS or Linux machine

## Project Structure

zerobrew is organized as a Cargo workspace with three crates:

- `zb_core`: Core data models and domain logic (formula resolution, bottle selection)
- `zb_io`: I/O operations (API client, downloads, extraction, installation)
- `zb_cli`: Command-line interface

Any changes you make that touch several crates should be organized properly. See [commit hygiene](#commit-hygiene)

## General Development Workflow

We prefer that a PR is linked to an open issue or previously discussed through other channels. 
If you are introducing changes that aren't otherwise reported or tracking please either reach 
out in the Discord to give us a heads up or open an issue first to discuss your changes.

**General flow:**
1. Fork the repo
2. Make your changes and ensure, at the _least_:
   - Code is formatted: `cargo fmt --all`
   - No clippy warnings: `cargo clippy --workspace --all-targets -- -D warnings`
   - Unit tests pass: `cargo test --workspace`
   - Integration tests pass: `cargo test --workspace -- --ignored` (or `just test` for all tests)
> [!NOTE] 
> These will run in CI but it's best you clean up your code _before_ opening a PR to ensure a quick 
> turnaround!

### Using Just

This project includes a `Justfile`, Install [just](https://github.com/casey/just) and use these commands instead of `cargo` (for ease of development):

- `just build` Check formatting, lint, then build the binary (Builds debug binary)
- `just install` Build and install zb to $HOME/.local/bin (Customizable with `$ZEROBREW_BIN`)
- `just uninstall` Remove all zerobrew installations and configurations
- `just fmt` Format code with rustfmt
- `just fmt-check` Check code formatting
- `just lint` Run clippy with strict warnings
- `just test` Run all workspace tests (unit & integration)

Before creating a PR make sure you `build` your changes and `test` them.

3. Write tests for new functionality. Each module should have accompanying tests.
4. Commit your changes with clear, descriptive commit messages (see below)
5. Push to your fork and submit a pull request.

## Commit hygiene
We ask that you follow the format below for commits:
```bash
[fix / feat]($crate): description
```

for instance:
```bash
fix(zb_cli): foo bar moo baz
```
Allowed prefixes:
```bash
fix      # -> fixes a bug or regression
feat     # -> new feature
chore    # -> housekeeping (deps, typos in docs, etc.)
tests    # -> added, changed or removed tests
ci       # -> changes to ci
refactor # -> refactored code
perf     # -> performance related
build    # -> changes to build system (i.e. ext deps, tooling, scripts, etc)
```

Generally speaking, we also ask that you please write isolated, [atomic commits](https://en.wikipedia.org/wiki/Atomic_commit). 
This means if you are approaching a PR that touches various parts of the codebase for example, ensure that your commits
are contained and cleanly seperated, properly describing/notating which commits belong where.


## Testing

- Unit tests should be colocated with the code in `mod tests` blocks
- Use `tempfile` for filesystem tests
- Use `wiremock` for HTTP mocking in integration tests
- Tests should be deterministic and not rely on external network access

## Running Benchmarks

To benchmark performance:

```bash
just bench --full
```

This runs a 100-package installation suite comparing zerobrew to Homebrew. This is especially crucial to run if you are 
planning on contributing to performance/optimization related changes.

Useful options:

```bash
just bench --quick
just bench --dry-run
just bench --full results/
just bench --format csv --output benchmark.csv
just bench --log bench.log
```

Notes:
- Defaults to the quick package list (22 packages); use `--full` for all 100.
- `--full [dir]` writes all formats (txt/json/csv/html) to the directory.
- `--output` infers format from file extension when `--format` is omitted.
- Output includes cold + warm cache speedups per package.

### macOS Homebrew permissions

On macOS, Homebrew should be installed with a user-writable prefix. If `just bench` fails with a permission error, fix it by running:

```bash
sudo chown -R "$(whoami)" "$(brew --prefix)"
```

## Questions?

For further questions, open an issue on GitHub.
