Run the linter with project-specific configuration.

## Steps

Run these two commands in sequence:

```bash
cargo fmt -- --check
cargo clippy --workspace --features="" -- -D warnings
```

These are equivalent to `make lint`.

## On failure

**Format failure** (`cargo fmt -- --check` fails):
- Show which files need formatting
- Run `cargo fmt` to fix automatically, then show the diff

**Clippy failure** (`cargo clippy` fails):
- Show the exact clippy error with file and line number
- Explain what the warning means
- Either apply the suggested fix or explain why it should be suppressed with `#[allow(...)]`

## On success

Confirm: "lint passed — fmt and clippy clean"

## Notes

- `make lint` runs both commands as a single shortcut
- Always run lint before opening a PR
- Clippy warnings treated as errors (`-D warnings`) — do not suppress without a comment explaining why
