# 0058 rail group polish — implementation entry point

Proposal: `proposal.md` (this directory). Branch: `llm/andrew/keitora/0058-rail-group-polish`.

Stages run **sequentially**; each is tested before the next and committed on its own.

1. `001-pure-logic.md` — pure helpers + model fields, with unit tests.
2. `002-drag-model.md` — frozen-layout drag with insertion line (pointer + keyboard).
3. `003-section-cards.md` — identity hue, cards, monogram, two-line header.
4. `004-verify.md` — full test/type/lint pass and manual checks list.

Test rigor: every new pure function in `src/lib` gets vitest coverage in the same commit;
`yarn test` and `yarn tsc --noEmit` stay green after every stage. No Rust changes are
expected; if one happens, `cargo test --workspace` must pass too. Do not launch
`dev:isolated` / herdr-backed runs on this machine (isolation leaks on Windows).
