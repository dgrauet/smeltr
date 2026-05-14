# Contributing to smeltr

Thanks for your interest. Issues, bug reports, and PRs are welcome.

## Requirements

- macOS 14+ on Apple Silicon (M1/M2/M3/…). The Metal hook and most probes are macOS-specific.
- Rust 1.88+ — pinned via `rust-toolchain.toml`, installed automatically by `rustup`.
- Python 3.10+ if you touch the sidecar (`python/`). MLX is an optional extra and only works on Apple Silicon.

If you're on Linux/Windows you can still review code and open issues, but you can't run the test suite end-to-end.

## Setup

```bash
git clone https://github.com/dgrauet/smeltr && cd smeltr
cargo build --workspace          # also runs `make -C metal-hook` via build.rs
pip install -e 'python/[mlx,dev]'  # or `uv pip install ...` in a venv

# Optional but recommended: install the pre-commit hooks
pip install pre-commit
pre-commit install --hook-type pre-commit --hook-type commit-msg
```

## Run what CI runs

```bash
# Rust
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Python
cd python && ruff check . && ruff format --check . && ty check && pytest tests
```

`make -C metal-hook clean all` rebuilds the dylib on its own (rare — `cargo build` does it for you).

## Conventions

These are enforced by CI and reviewers — please follow them.

- **TDD.** Write a failing test, then the minimal code to pass it, then refactor. New features and bug fixes both need a test that demonstrates the change.
- **Conventional Commits.** `<type>(<scope>): <description>` with types `feat`, `fix`, `chore`, `docs`, `test`, `refactor`, `build`, `ci`. Validated by `commitlint` on PRs and by the local pre-commit hook. Header limit: 100 characters.
- **No `unwrap` / `expect`** outside `main.rs` and tests. Use `?` or surface errors with context via `anyhow`/`thiserror`.
- **`#[serial_test::serial]`** on any test that mutates env vars (`SMELTR_HOME`, `SMELTR_SOCKET`, `SMELTR_DYLIB`, …).
- **Small focused files.** If a file you're modifying is growing past a screen or two of unrelated logic, prefer splitting it.
- **Don't add features beyond the task.** YAGNI: bug fixes don't need refactors; one-shot operations don't need helpers.

## Pull request flow

1. Fork the repo, create a topic branch (`git checkout -b feat/my-thing`).
2. Make your change with TDD; commit using Conventional Commits.
3. Run the CI checklist locally before pushing.
4. Open a PR. The CI workflow runs:
   - `Rust workspace` (fmt, clippy, harness build, full test suite) on macOS Apple Silicon.
   - `Python sidecar` (ruff, ty, pytest) on macOS Apple Silicon.
   - `intendant audit` (governance rules).
   - `commitlint` (Conventional Commits).
5. Address review feedback in additional commits — don't force-push during review unless asked. Squash-on-merge keeps history tidy.

## Repo layout

| Path | What lives there |
|---|---|
| `crates/` | Rust workspace — daemon, CLI, analyzer, replay, TUI, MCP, probes |
| `metal-hook/` | ObjC++ dylib injected via `DYLD_INSERT_LIBRARIES` |
| `python/` | Optional Python sidecar (`smeltr` package) |
| `docs/` | Usage guide, ADRs, dogfood findings |
| `.github/workflows/` | CI |

## Reporting bugs

A useful bug report includes:

- Hardware (M1/M2/M3, RAM).
- macOS version (`sw_vers -productVersion`).
- `smeltr --version` and `smeltrd --version`.
- The exact command that reproduces it.
- Relevant lines from `~/.smeltr/smeltrd.log` and the captured session if applicable.
- Whether the target binary is `arm64`, `arm64e`, or hardened (`lipo -archs <bin>` and `codesign -dv <bin>`).

## License

By contributing, you agree your contribution is licensed under the MIT License (see [`LICENSE`](LICENSE)).
