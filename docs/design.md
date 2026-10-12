# Tidy — design spec (v0.1)

Tidy analyses, cleans up, compresses and organises files on disk. It is fast (parallel, syscall-batched), frugal (bounded memory), and safe (nothing touches disk without an approved plan; every action is journalled and reversible where the filesystem allows).

Personal tool. macOS + Linux first-class; Windows later. Rust, release builds tuned for speed.

## Principles

1. **The model chooses, templates write.** Decision models return probabilities/choices/scores — never paths or names. Destinations come from templates or a `choice` among existing folders.
2. **Plans are the only way anything happens.** Every proposed action is an entry in a plan file. Nothing runs until an entry is `approve`d. Dry-runs and autonomy are the same mechanism.
3. **Code guards outrank models.** L0 guards are absolute; models can only suggest or veto within what guards allow.
4. **Reversible by default.** `trash`/`quarantine` over `delete`; journal before act; undo per plan or entry.
5. **Measure what you get back.** Track logical, allocated and private (unshared) bytes.

## Pipeline

```
scan → index → detect (L1) → guard (L0) → judge (L2) → plan → review → apply → journal
  ▲                                                                     │
  └─────────────── incremental rescan (FSEvents / inotify) ◄── undo ◄───┘
```

## Components

| Component | Design |
|---|---|
| Walker | Work-stealing over directories. Linux: `openat` + `getdents64` + `statx` (`AT_STATX_DONT_SYNC`). macOS: `getattrlistbulk` (name, type, size, alloc size, inode, link count, mtime in one buffer). Fallback: `std::fs`. Never follows symlinks; stays on one device by default. |
| Index | Directories only. Flat arena, ids assigned at discovery (child id > parent id ⇒ one reverse pass rolls totals up). Names in one byte buffer. Per-thread top-K heaps for largest files. Hardlinks counted once. Memory scales with dirs, not files. Snapshot to disk (mmap-able) for diffs. |
| Sizes | `logical` (st_size), `allocated` (st_blocks·512), `private` (APFS `ATTR_CMNEXT_PRIVATESIZE`; Linux FIEMAP shared flag on btrfs/xfs; else = allocated). |
| Detectors (L1) | Deterministic regenerable artifacts: `target/`+`Cargo.toml`, `node_modules`+`package.json`, `.venv`/`venv` with `pyvenv.cfg`, `__pycache__`, `.gradle`, `build/` with build files, Xcode DerivedData, `.next`, `.turbo`, `.parcel-cache`, tool caches (`~/Library/Caches`, `~/.cache`, Homebrew, pip, npm, Go, cargo registry). |
| Guards (L0) | Absolute blocks: system paths, bundles/packages (`.app`, `.photoslibrary`, …), files open/recently modified (< 7 d default), git work trees with uncommitted/unpushed work, cloud placeholders, destinations of move rules, anything outside scan scope. |
| Judge (L2) | `Judge` trait over a question IR (predicate / choice / score, text + images). Backends: OpenAI Decisions (`gpt-6-luna`), SystemOne-compatible (TypeSafe Jev; Cloudflare Clef/Clef-flash hosted or local). Capabilities per judge (images, max questions, context). Cascade cheap→expensive on the uncertain band. Cache keyed by (judge id, policy hash, item fingerprint). |
| Plans | JSONL source of truth. Views: TUI, `$EDITOR` (rebase-style), bulk flags, MCP. Entries carry fingerprint (dev, ino, size, mtime, hash), per-rule probabilities, alternatives, undo capability. |
| Executor | `apply` = verify fingerprints → journal intent → act → journal done. Same-volume moves are atomic renames; cross-volume = copy, fsync, verify, remove. Stale entries skipped. Crash recovery on next start. |
| Space actions | Dedupe: size → head/tail sample hash → BLAKE3; replace dup with clone (`clonefile` / `FICLONE`/`FIDEDUPERANGE`). Compress: zstd-1 sample ratio; APFS decmpfs / btrfs compression where possible, else `.tar.zst`. |

## Decision layers

| Layer | Authority |
|---|---|
| L0 Guards (code) | Absolute block |
| L1 Detectors (code) | Propose regenerable cleanups |
| L2 Policy (judge) | Propose or veto |
| L3 Human | Approves plan entries (manual) or pre-approves via `mode = "autonomous"` |

Delete/quarantine combination (thresholds from `tdy calibrate`, per judge+model version):

```
any protect.* ≥ τp                                        → KEEP
regenerable ≥ τr ∧ any allow.* ≥ τa ∧ loss_cost ≤ τl      → PROPOSE trash/quarantine
otherwise                                                 → ASK
```

## Policy file

```toml
mode = "manual"            # or "autonomous": entries are written pre-approved
presets = ["screenshots"]  # built-in rule packs, all off by default

[judge]
default = "luna"
[judge.backends.luna]
kind = "openai-decisions"
model = "gpt-6-luna"
[judge.backends.clef-local]
kind = "systemone"
endpoint = "http://localhost:11434/…"
model = "clef-flash"

[subjects]
me = { refs = ["~/.config/tdy/subjects/me-1.jpg"] }

[[rule]]
id          = "photos.dogs"
action      = "move"
scope       = ["~/Downloads", "~/Desktop"]
filter      = { kind = "image", min_size = "50KB" }
when.any    = ["picture of dogs", "picture of {me} next to a dog", "picture of a walk in the park"]
to          = { path = "~/photos/dogs/{exif.date:%Y}", create_if_missing = true }
on_conflict = "dedupe-else-suffix"

[[rule]]
id     = "protect.tax"
action = "keep"
when.any = ["Tax returns, invoices, contracts, payslips, or anything finance/legal."]
```

Semantics: rules evaluated in file order. For exclusive actions (move, rename, delete, trash, archive) the **first match above threshold wins**; non-exclusive actions (tag) accumulate. The runner-up is recorded as an alternative in the plan entry and can be chosen in review.

## Plans

```
tdy plan ~ [--policy tdy.toml]        → plan id
tdy review <id>                        # TUI, one by one
tdy review <id> --edit                 # $EDITOR, rebase-style
tdy review <id> --approve-all | --reject-all | --approve --rule R --min-p P
tdy apply <id> [--dry-run]
tdy undo <id> [--entry E]
```

## Interfaces

CLI (NDJSON output for agents), TUI review, `tidyd` watcher (settle window for in-flight downloads), MCP server (`scan`, `top`, `explain`, `plan`, `review`, `apply`, `undo`, `policy_test`; `apply` only runs entries a human approved unless `mode = "autonomous"`).

## Platform matrix

| Capability | macOS (APFS) | Linux | Fallback |
|---|---|---|---|
| Bulk walk | `getattrlistbulk` | `getdents64` + `statx` | `readdir` + `lstat` |
| Private bytes | `ATTR_CMNEXT_PRIVATESIZE` | FIEMAP shared flag (btrfs/xfs) | allocated |
| Clone dedupe | `clonefile` | `FICLONE` / `FIDEDUPERANGE` | report only |
| Transparent compression | decmpfs | btrfs | `.tar.zst` |
| Change feed | FSEvents | inotify (watched dirs) | rescan |
| Provenance | Spotlight `kMDItemWhereFroms`, last-used | `user.xdg.origin.url` | — |
| Trash | `~/.Trash` | XDG trash spec | quarantine dir |

## Milestones (exit criteria; CI on macOS APFS + Linux ext4/xfs)

| M | Scope | Done when |
|---|---|---|
| M0 | Scanner, index, sizes, bench harness | Beats dust/dua/gdu on fixed 1M-file tree (warm). ≤256 MB RSS @ 10M entries. Allocated matches `du`. Private correct on clone/reflink fixtures. |
| M1 | Plans, review, apply, undo, journal, ≥10 detectors | Plan→apply→undo restores fixture trees byte-for-byte. Stale entries skipped. Kill mid-apply recovers. |
| M2 | Judge IR, OpenAI + SystemOne adapters, cascade, cache, calibrate, policy test, bench judges | Zero paid calls on unchanged rescan. `policy test` in CI. |
| M3 | move/copy/tag/rename, `pick_from`, templates, image pipeline, subjects | Per-rule precision ≥ 0.95 on labelled set at calibrated threshold. |
| M4 | `tidyd` watcher, MCP server | New file → plan entry < 5 s. MCP cannot apply unapproved entries. |

## Release profile

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```
Personal builds: `RUSTFLAGS="-C target-cpu=native"`. PGO later via `cargo-pgo`. No async runtime on non-network paths.

## Implementation notes (v0.1, as built)

Decisions made while building, beyond the spec above:

- **Approval provenance.** Entries carry `approved_by`: `human` (TUI, `$EDITOR`, CLI) or `policy` (autonomous mode). MCP `tdy_apply` only runs those. Agents can propose and reject, never approve. Plans are addressed by id only, and paths passed to subprocesses (`tdy_plan`, `tdy_dupes`) can't carry flags.
- **Two age floors.** Removals (delete, trash, quarantine, archive, dedupe) require `min_age` (default 7d) since the newest change. Organising actions (move, rename, copy, tag) only wait for a settle time (120 s, or the watcher's `--settle`).
- **Rule combination.**
  - Keep rules veto at τ = 0.15, and also when their question goes unanswered (fail-safe).
  - Exclusive actions: the first rule with p ≥ τ wins. Otherwise the best rule with p ≥ max(0.5, τ − 0.25) is proposed as *uncertain*; uncertain entries are never auto-approved.
  - Tags accumulate.
  - A model-decided `delete` is downgraded to `trash` unless `safety.regenerable` ≥ 0.8 and `safety.loss` ≤ 1 (Minor). A loss ≥ 2.5, or no loss answer, marks the entry uncertain.
- **Overlaps.** L1 entries win over L2 entries at or inside the same path.
- **Presets.** Built-in rule packs compiled into the binary (`installers`, `screenshots`, `stale-downloads`, `archive-leftovers`, `protect.personal`). All are off unless listed in `presets = [...]`. A user rule with the same id replaces the preset's rule. Each preset carries its own `policy test` cases, which run while it is enabled and unmodified. Calibration overrides a preset's threshold; a user's explicit threshold overrides calibration.
- **Removal-only keep rules.** `protects = "removal"` vetoes delete, trash, quarantine and archive, but not move, rename, copy or tag. Such a rule is pruned from an item's request unless a lossy removal rule applies too, so organising-only items cost no extra tokens.
- **Rule checks.** `check = "extracted-sibling"` is a code predicate (zip central directory vs. the extracted folder: size + CRC-32), evaluated at plan time and again at apply time. Removals backed by a check that proves nothing is lost are exempt from removal-only keep rules, never from full keep rules.
- **No judge configured.** Model-decided rules are skipped with a warning rather than failing the plan; deterministic rules still run, and unanswered keep rules still protect.
- **Decision cache.** Keyed by judge id and a digest of the request (evidence text, image hashes, questions). Rewording a criterion only re-asks the affected items.
- **Cascade.** A later stage re-asks whenever any predicate lands in (0.2, 0.8) or is refused. The band is configurable.
- **Evidence.** Metadata only by default: name, home-relative path, size, age, provenance xattrs (`kMDItemWhereFroms`, quarantine agent, last-used date; `user.xdg.origin.url` on Linux). `content = true` adds a 2 KB text excerpt or a 512 px JPEG thumbnail. Subject reference photos are attached when a criterion mentions `{name}`.
- **Fingerprints.** (dev, ino) plus size and mtime for files; for directories, a fresh subtree summary (files, apparent bytes, newest mtime); a BLAKE3 hash for dedupe entries. The apply-time guard re-checks age against the fingerprint's mtime, and current file flags.
- **Journal.** Records `begin` (with undo state such as previous tags and the archive path), `copied` (cross-volume, after fsync), `done` (with the full outcome) or `fail`. Recovery never deletes a source unless the destination is intact, and dry runs never recover.
- **Filesystem safety.** `remove_tree` and `copy_tree` never follow symlinks or cross mount points. Guards block directories that contain mount points. Comparisons use resolved paths.
- **Space actions.**
  - Dedupe uses `clonefile` + rename (macOS) or `FIDEDUPERANGE` (Linux, kernel-verified); filesystems without clones get `trash` instead.
  - Archive writes `.tar.zst` (zstd multithreaded, verified by re-reading) and quarantines the original.
  - Compress uses APFS decmpfs via `ditto` with an atomic swap, or btrfs defragment.

Measured (Linux, 2 vCPU):
- **Scanner:** 1.55 s for 1.1M entries warm (2× du, 2.5× dust/gdu). Peak RSS is 151 MB at 11M entries.
- **gpt-6-luna via the Decisions API:** p50 537 ms / p95 628 ms through a proxy. About 700 input tokens per item for metadata-only requests, about 740 with a thumbnail.

Built after the first release (October 2026):
- an automatic index cache: instant starts, `--cached` answers, subfolders cut out of a cached parent
- patching cached indexes after apply and undo, instead of rescanning
- huge folders stat-ed on several threads (Linux)
- `tdy ask` and the UI's New plan screen: plans from plain-English requests, cleanups, duplicates and archives without leaving `tdy ui`
- the command renamed from `tidy` to `tdy` (macOS ships HTML Tidy as `/usr/bin/tidy`), with folders and variables to match

Tried and dropped: io_uring statx batching (1.7–2.3× slower than plain statx on warm metadata).

Not built yet:
- per-rule judges
- FSEvents event-id incremental rescans (the index cache exists; this would tell it what changed)
- PGO
- video keyframes
- Windows
