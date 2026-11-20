## What

<!-- One or two sentences: what does this PR change? -->

## Why

<!-- The problem or motivation. Link the issue if there is one. -->

## How it was tested

<!-- Commands you ran (fmt / clippy / test) and what you clicked through in the demo. -->

## Screenshot (UI changes only)

<!-- Before / after. Use: cargo run -p app -- --theme <t> --screen <s> -->

## Plugin API impact

- [ ] No impact on the plugin API, theme tokens, data model, or capabilities
- [ ] Impact, design note linked from the issue: <!-- link -->

## Checklist

- [ ] `cargo fmt --all -- --check` is clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean
- [ ] `cargo test --workspace` passes, and new logic has new tests
- [ ] No hardcoded colours outside theme tokens (UI changes)
- [ ] CHANGELOG.md updated under Unreleased (user-visible changes)
