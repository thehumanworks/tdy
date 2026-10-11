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

Delete/quarantine combination (thresholds from `tidy calibrate`, per judge+model version):

```
any protect.* ≥ τp                                        → KEEP
regenerable ≥ τr ∧ any allow.* ≥ τa ∧ loss_cost ≤ τl      → PROPOSE trash/quarantine
otherwise                                                 → ASK
```

## Policy file

```toml
mode = "manual"            # or "autonomous": entries are written pre-approved

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
me = { refs = ["~/.config/tidy/subjects/me-1.jpg"] }

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
tidy plan ~ [--policy tidy.toml]        → plan id
tidy review <id>                        # TUI, one by one
tidy review <id> --edit                 # $EDITOR, rebase-style
tidy review <id> --approve-all | --reject-all | --approve --rule R --min-p P
tidy apply <id> [--dry-run]
tidy undo <id> [--entry E]
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
