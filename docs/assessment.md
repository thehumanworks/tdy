# Tidy — strengths, weaknesses and future ideas

An honest look at v0.1 (October 2026). Numbers come from the benchmark and live runs recorded in this repo. For how it works, see [architecture.md](architecture.md).

## Milestone status

| Milestone | Exit criteria (from [design.md](design.md)) | Status |
|---|---|---|
| M0 Scanner | Beats dust/dua/gdu at 1M files; ≤ 256 MB RSS at 10M entries; allocated bytes match `du` | **Mostly met on Linux:** 1.49 s vs 3.9–6.6 s for dust/gdu/dua (`du` 2.9 s), 151 MB at 11M entries, `du` parity test. No test yet for private bytes on clone/reflink fixtures. **Not run on macOS.** |
| M1 Plans | Plan → apply → undo restores trees; stale entries skipped; crash recovery | **Met:** 10 plan tests covering round-trips, stale entries, crash recovery (one interrupted cross-volume move), dry runs and the editor format |
| M2 Judges | No paid calls on unchanged rescans; `policy test` in CI | **Partly met:** rescans cost 0 tokens, but there is no CI yet |
| M3 Organising | Per-rule precision ≥ 0.95 on a labelled set at the calibrated threshold | **Built; not measured.** Calibration exists, but no labelled set from real files yet |
| M4 Watch + MCP | New file → plan entry in < 5 s; MCP can't apply unapproved entries | **Met:** 2.1–2.6 s; enforced and tested |

## Strengths

### Fast and frugal
- **Scanner speed.** 1M files in 100k directories (Linux, 2 vCPU, warm cache):

  | Tool | Time | Peak RSS |
  |---|---|---|
  | tidy | 1.49 s | 19.5 MiB |
  | du | 2.89 s | 1.8 MiB |
  | dust | 3.89 s | 438 MiB |
  | gdu | 4.02 s | 24.5 MiB |
  | dua | 6.56 s | 10.3 MiB |

- **Memory scales with directories, not files.** It uses 64 bytes per directory plus names, and 151 MB at 11M entries.
- **Lean runtime.** No async runtime and no background daemon unless you start `tidy watch`. One 7.7 MB binary.
- **Platform-native.** It uses each platform's own mechanism instead of a lowest-common-denominator copy:
  - bulk directory reads (`getattrlistbulk`, `getdents64` + `statx`)
  - APFS unshared bytes in the same syscall
  - kernel-verified dedupe (`FIDEDUPERANGE`)
  - `clonefile`, decmpfs and btrfs compression
  - Finder tags and download provenance

### Safe by construction
- **One path to change anything:** plan → review → apply. Dry runs and autonomous mode are the same mechanism, not special cases.
- **Layered authority.** Code guards (L0) beat detectors (L1), which beat the model (L2), which beats nothing without your approval. The model never writes a path.
- **Crash-safe and reversible.**
  - Intent is journalled before every change and fsync'd.
  - Cross-volume moves never delete a source unless the copy verified.
  - Recovery reconciles the journal with the filesystem.
  - Trash, quarantine, moves, renames, tags and archives can all be undone.
- **Staleness detection.** Fingerprints, guards and rule checks are re-run at apply time, so a plan made yesterday can't act on something that changed since.
- **Agent-safe.** Over MCP, agents can propose and reject but never approve (`approved_by` provenance). Plans are addressed by id, and `tidy_plan` paths can't carry flags.

### Smart without being expensive
- **Rules in plain English**, for organising (move, copy, rename, tag, `pick_from` among your folders), not just deleting.
- **Cost-aware engine.**
  - Deterministic rules need no model.
  - Items every action would be blocked on are never sent.
  - Irrelevant keep rules are pruned.
  - Answers are cached per request digest.
- **Measured cost.** Live runs cost about $0.0006 for 13 Downloads items and $0.0009 for 8 requests with all presets on. Rescans cost $0.
- **Rules are testable like code.** `policy test` runs cases, presets ship their own, and `calibrate` fits thresholds per judge and rule. `bench judges` compares backends on agreement, latency and cost.
- **Provider-neutral.** One question IR serves OpenAI Decisions, TypeSafe Jev and Cloudflare Clef. The cascade sends only uncertain answers to a stronger model.
- **Fail-safe defaults.** Unanswered keep rules protect. A model-decided `delete` becomes `trash` unless the item is clearly regenerable and cheap to lose. Uncertain entries are never pre-approved. Every preset is off until you list it.

## Weaknesses

Ranked by impact on you today.

| # | Weakness | Impact | Evidence |
|---|---|---|---|
| 1 | **macOS has never run.** The macOS code is only type-checked from Linux: `getattrlistbulk` reader, `clonefile` dedupe, decmpfs compression, Finder tags, `~/.Trash`, FSEvents, `sips`. | High: macOS is the primary target (your Mac Studio) | `scripts/check-macos.sh` only runs `cargo check` |
| 2 | **No CI.** Tests, clippy and the macOS check run by hand. | High | No workflow files in the repo |
| 3 | **Privacy of evidence.** File names, home-relative paths and download URLs go to a third-party API for every judged item. Only the home prefix is redacted. Content excerpts and thumbnails are opt-in, except that a criterion mentioning a subject (`{me}`) always sends the item's thumbnail. | Medium–high for sensitive folders | `items::evidence_with` |
| 4 | **Uncalibrated by default.** Thresholds (0.8, 0.15 for keep) are defaults until you label items. The first `protect.personal` wording over-protected installers until reworded. A provider silently changing a model under the same name would leave stale cached answers. | Medium | `policy test` history; cache keyed by judge id |
| 5 | **SystemOne backends are untested live** (Jev, Clef). Only request/response shapes are tested against the documentation. | Medium (only if you switch provider) | `body_and_parse_match_docs` |
| 6 | **L2 item listing is sequential.** Each rule scope is listed on one thread, to depth 64 for file rules without a `depth`, and doesn't reuse the parallel walker or a saved index. A broad scope like `~` is slow. | Medium | `items::list`, `l2::collect` |
| 7 | **Decision cache grows forever.** Append-only JSONL, loaded fully into memory at start, with no compaction or TTL. | Low now; grows with use | `Cached::open` |
| 8 | **Apply is sequential.** One entry at a time, rewriting the whole plan every 64 entries. Fine for hundreds of entries; slow for 100k dedupes. | Low–medium | `exec::apply` |
| 9 | **Watch mode limits.** It runs in the foreground, with no launchd or systemd unit. Changes made while it isn't running are missed (no FSEvents event-id resume). Linux inotify needs one watch per directory. | Medium for autonomous use | `watch.rs` |
| 10 | **Presets are English and default-path only.** Localised macOS screenshot names and custom capture folders aren't recognised. `installers` includes `.zip` and relies on the model to tell app zips from personal archives. | Low–medium | `presets.rs` |
| 11 | **Thin CLI test coverage.** `tidy-cli` has one test (MCP approval). `policy test`, `calibrate`, `watch` and the TUI were only checked by hand or in live runs. Parsers for untrusted input (zip central directory, bplist, `getattrlistbulk` buffers) are bounds-checked but not fuzzed. | Medium | 44 tests in total; 1 in `tidy-cli` |
| 12 | **Hard-coded judge prices.** Cost estimates will drift as prices change. | Low | `price_per_mtok` |
| 13 | **Some actions are one-way.** `delete`, `dedupe` and `compress` can't be undone. Dedupe and compress keep content identical, and `delete` is kept for regenerable artefacts. | Low | `undo ✗` in plans |
| 14 | **Dependency weight.** 174 packages in the dependency graph (image decoders, TUI, TLS, notify), which is heavy for a "frugal" tool. `memmap2` is declared in the workspace but not used directly. | Low | `cargo tree` |
| 15 | **Gaps from the spec.** Per-rule judges are accepted but ignored, with a warning. No video keyframes, no PGO, no io_uring batching, no Windows. | Low | `judge_cmds.rs`, design.md |
| 16 | **MCP `tidy_dupes` passes paths unchecked.** A path starting with `-` becomes a flag of `tidy dupes` (for example `--plan`). Any plan it creates still starts pending, so nothing is applied, but it should reject such paths as `tidy_plan` does. | Low–medium | `mcp.rs`, `tidy_dupes` |
| 17 | **Quarantine on other volumes isn't purged.** `tidy purge` only empties `<data>/quarantine`; `<volume>/.tidy-quarantine/<uid>` buckets stay until removed by hand. | Low | `trash.rs`, `plan_cmds.rs` |
| 18 | **`compress` skips the open-file, recent-change and git guards**, because it counts as non-destructive (content is preserved via an atomic swap). A file being written during the swap is the edge case. | Low | `Action::destructive` |

## Future ideas

### Now: prove it on the Mac and lock it in
1. **Run the full suite and benchmarks on macOS (APFS)** and fix what breaks: reader, private bytes, `clonefile`, decmpfs, Finder tags, trash, FSEvents.
2. **CI matrix:** GitHub Actions on macOS (arm64) and Linux (ext4 plus an xfs or btrfs loop device). Run tests, clippy, `check-macos.sh` and a small benchmark smoke test.
3. **Fuzz the parsers** with `cargo-fuzz`: zip central directory, bplist, `getattrlistbulk` decoding, journal recovery.
4. **Cache hygiene:** compaction, a TTL or size cap, and prices read from config.

### Next: capability
- **One scan feeds L1 and L2.** List rule scopes with the parallel walker, or from a fresh snapshot, instead of sequential reads.
- **Parallel apply.** Per-volume queues for independent entries, with the journal still ordered per entry.
- **Incremental everything.**
  - FSEvents `sinceWhen` event ids plus the saved index give near-instant rescans.
  - launchd and systemd units turn `tidy watch` into a quiet background service.
- **More lossless checks.** Each one lets `protect.personal` stay on without blocking safe cleanups:
  - `installed-app`: a `.dmg` or `.pkg` whose app, at the same or a newer version, is already in `/Applications`
  - `duplicate-elsewhere`: an identical copy exists elsewhere
  - `in-photos-library`
  - `extracted-sibling` for `.tar`, `.7z` and `.rar`
- **Better presets.**
  - Locale-aware screenshot names, and reading the capture folder from `com.apple.screencapture`.
  - New packs: old iOS backups, Xcode archives, large screen recordings, and a Downloads-by-type organiser.
- **Privacy and choice of judge.**
  - Per-rule judges: a cheap model for easy rules, a stronger one for sensitive ones.
  - A fully local mode (Clef-flash on localhost).
  - Redaction options: hash names, drop URLs.
- **Media.** Video keyframes (AVFoundation or ffmpeg) so video rules see content, not just metadata.
- **Space you can't see.** Report APFS local snapshots and purgeable space; Time Machine snapshots keep deleted bytes allocated.
- **Review UX.** Inline thumbnails in the TUI (kitty or iTerm2 graphics), views grouped by rule, and per-entry "why" from the stored evidence.
- **Lean builds.** Feature-gate the image decoders and the TUI; drop the direct `memmap2` declaration or use it for snapshots.

### Later
- **Performance:** PGO via `cargo-pgo`, and io_uring `statx` batching on Linux.
- **Routines:** scheduled plans and a weekly "what changed / what's reclaimable" report.
- **Agents:** plans exposed as MCP resources, and a streamable HTTP transport.
- **Windows:** NTFS reader, Recycle Bin, ReFS block cloning.
