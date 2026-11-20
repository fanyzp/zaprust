## What

<!-- One or two sentences: what does this PR change? -->

## Why

<!-- The problem or motivation. Link the issue if there is one. -->

## How it was tested

<!-- The gate from CONTRIBUTING.md section 4. Check every command you ran. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo check --workspace --all-targets`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo check -p backend`
- [ ] `cargo check -p backend --features calls`

<!-- Anything you exercised by hand, for example in demo mode: -->

## Screenshots (required for UI or design changes)

<!-- Before and after, side by side where you can. Screenshots are
     reproducible with: cargo run -p app -- --demo --screen <name>
     A UI or design PR without before and after screenshots is sent
     back unreviewed. Delete this section only if the PR touches no
     UI at all. -->

| Before | After |
|---|---|
|  |  |

## Checklist

- [ ] One concern only; no unrelated changes in this PR
- [ ] New or changed behavior is covered by tests
- [ ] Design-note surfaces (the Backend trait, theme tokens, the data model, the settings registry) are unchanged, or a design note is linked: <!-- link -->
- [ ] No hardcoded colours outside theme tokens (UI changes)
- [ ] CHANGELOG.md updated under Unreleased (user-visible changes)
- [ ] No em dash in new or edited text
