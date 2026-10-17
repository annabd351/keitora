# 005 - UBT tailer fingerprint

## Goal
Detect log rotation by file content identity so a new build's log is read from its start even when it is already longer than the previous offset.

## Prerequisites
004 committed (no code dependency; keep strict order).

## Implementation
Proposal B1, in `src-tauri/src/activity/progress/tail.rs` only: `TailCursor.head` (first line, up to 256 bytes, read separately from offset 0 on first attach), common-prefix comparison on every read, partial-first-line extension, `created: Option<SystemTime>` weak secondary signal, one `reset` per rotation even when the shrink rule also applies, shrink rule retained. `read_new_lines` signature and `TailRead` are unchanged. Update existing tail tests for the new fields.

## Validation
```
cargo check -p keitora
cargo clippy -p keitora
cargo test -p keitora tail
```
Required tests: rotation to a longer file resets via the fingerprint; a growing file does not false-reset; a creation-time change resets and a `None` creation time never does; a partial first line that later completes does not false-reset; an empty file then a first write captures the fingerprint without a reset; shrink and first-attach behaviour unchanged; rotation plus shrink yields a single reset.

## Commit message
```
fix(activity): detect build log rotation by content fingerprint

Remember the head of the tailed log and compare it on every read so a
rotated log that has already grown past the old offset is re-read from the
start instead of being joined mid-file.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```
