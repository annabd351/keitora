# 0059 — implementation entry point

Proposal: `plans/0059-rail-header-breathing-room.md`. Branch:
`llm/andrew/keitora/0059-rail-header-breathing-room`.

Stages (sequential; each is tested before the next):

1. `001-tail-path.md`: `tailPath` helper + unit tests.
2. `002-header-layout.md`: rail width, three-row header, overlay chip, sticky offset.

Rigor: `yarn test` and `tsc --noEmit` green after each stage. Stage 2 is visual, so it is
verified by a running-app smoke (collapsed/open/active/pinned/hover/drag, both themes).
No Rust changes, so no `#[cfg(test)]` additions are owed; `cargo test --workspace` is run in
Testing for regression only.
