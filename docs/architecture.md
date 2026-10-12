# Tidy — architecture

How Tidy is built, as of v0.1 (October 2026). For the original spec and the decisions taken while building it, see [design.md](design.md). For strengths, weaknesses and what comes next, see [assessment.md](assessment.md).

## 1. Goal

Tidy is a command-line tool that **analyses, cleans up, compresses and organises** the files on your disk. It has to be:

| Requirement | What it means in practice |
|---|---|
| Blazingly fast | Parallel, syscall-batched directory walks. The release build uses fat LTO and `panic = "abort"`. On Linux it scans about 2× faster than `du`. |
| Resource-friendly | Memory grows with the number of directories, not files: about 150 MB for 10M files. There is no async runtime and no daemon unless you run `tidy watch`. |
| Safe | Nothing changes on disk without an approved plan. Every change is journalled, and reversible where the filesystem allows. Code guards outrank any model. |
| Smart | Rules are written in plain English. A decision model (OpenAI `gpt-6-luna` via the Decisions API, or SystemOne-compatible TypeSafe Jev / Cloudflare Clef) judges them. |
| Personal first | macOS (APFS) and Linux are first-class. Windows comes later. Single user, local state, CLI-first, with an MCP server for agents. |

**What it does:**
- **Analyse:** `scan`, `top` and `diff` show where the space goes and what changed.
- **Clean up:** built-in detectors find regenerable build artefacts and caches, and your policy rules propose trash, quarantine or delete.
- **Compress:** copy-on-write dedupe of identical files, `.tar.zst` archives of cold folders, and transparent APFS/btrfs compression.
- **Organise:** move, copy, rename and tag rules. Destinations come from templates or from the model picking one of your existing folders.

## 2. Principles

1. **Plans are the only way anything happens.** Every proposed action is an entry in a plan file and runs only once approved. Dry runs, manual review and autonomous mode all use this one mechanism.
2. **The model chooses; code writes.** Models return probabilities, choices and scores, never paths or names. Paths come from templates or from a fixed list of existing folders.
3. **Code guards are absolute.** L0 guards block system paths, credentials, bundles, cloud placeholders, git work in progress, open files, recent changes and mount points. Neither a rule, a model nor an agent can override them.
4. **Reversible by default.** `trash` and `quarantine` are preferred over `delete`. Intent is journalled before each act, and undo works per plan or per entry.
5. **Pay once.** Judge answers are cached by request digest, so rescanning an unchanged item under an unchanged policy costs nothing.

## 3. System overview

```
                         ┌────────────────────────────────────────────┐
  you / scripts ───────► │ tidy-cli  CLI · `tidy ui` · $EDITOR review  │ ◄──── agents (MCP, stdio)
                         │           watch · MCP server · output       │
                         └──────┬───────────────┬─────────────────────┘
                                │               │
                 ┌──────────────▼──────┐   ┌────▼─────────────────────┐
                 │ tidy-rules          │──►│ tidy-judge               │──► OpenAI Decisions
                 │ L0 guards           │   │ question IR, backends,   │──► TypeSafe Jev
                 │ L1 detectors        │   │ cache, cascade, HTTP     │──► Cloudflare Clef
                 │ L2 policy engine    │   └──────────────────────────┘
                 │ presets, checks,    │
                 │ media, calibration  │
                 └──┬──────────┬───────┘
                    │          │
          ┌─────────▼───┐  ┌───▼─────────────────────────────┐
          │ tidy-plan   │─►│ tidy-ops                        │
          │ model, store│  │ fingerprints, safe fs ops,      │
          │ journal,    │  │ trash/quarantine, probes, xattrs│
          │ apply/undo  │  │ dupes, archive, compress        │
          └─────────┬───┘  └───┬─────────────────────────────┘
                    │          │
                 ┌──▼──────────▼───────────────────────────────┐
                 │ tidy-scan  platform readers, parallel walker,│
                 │            dir-only index, snapshots         │
                 └──────────────────────────────────────────────┘
```

### Crates

| Crate | Role | Depends on | Lines (src / tests) |
|---|---|---|---|
| `tidy-scan` | Platform directory readers, the work-stealing walker, the directory-only index, snapshots, the index cache and patching, file visitor | — | 3,055 / 438 |
| `tidy-ops` | Fingerprints, safe filesystem primitives, trash and quarantine, probes (open files, git, cloud, mounts), xattr metadata, space actions | scan | 1,845 / — |
| `tidy-plan` | Plan model (JSONL), store, fsync'd journal, apply / undo / crash recovery, review helpers, keeping cached indexes in step | scan, ops | 1,579 / 412 |
| `tidy-judge` | Provider-neutral question IR, OpenAI and SystemOne backends, mock, persistent cache, cascade, HTTP with adaptive concurrency | — | 1,129 / — |
| `tidy-rules` | L0 guards, L1 detectors, policy file and presets, L2 engine, item evidence, rule checks, EXIF and thumbnails, calibration, planner | scan, ops, plan, judge | 3,712 / 463 |
| `tidy-cli` | The `tidy` binary: commands, the interactive UI (`tidy ui`, ratatui), `$EDITOR` review, watch mode, MCP server | all of the above | 6,189 / — |
| `tidy-bench` | `tidy-fixture`: deterministic trees for benchmarks; `tidy-cachebench`: cached starts and patches against rescans | scan | 256 / — |

That is about 17,800 lines of Rust (counted with `wc -l`) plus about 1,300 lines of integration tests. There are 65 tests in total; unit tests live next to the code.

## 4. Data flow

### Planning (`tidy plan PATHS`)

```
              ┌──────────── L1 (deterministic) ─────────────┐
 PATHS ──►  walk (rayon, one job per directory)              │
            → Index (DirNode arena, marker flags, top-K)     │
            → detectors (markers + known cache paths)        │
            → guards (L0) ───────────────────────────────────┴──► entries (regenerable)
                                                                        │
              ┌──────────── L2 (policy rules) ──────────────┐           │
 policy ──► presets expanded → rules compiled (scopes, τ)    │           │
            → items listed per rule scope (platform reader)  │           │
            → applicable rules per item (scope, filter,      │           │
              check, pruning of irrelevant keep rules)       │           │
            → guard pre-check (skip a paid call when every   │           │
              action would be blocked anyway)                │           │
            → Request: evidence text (+ thumbnail) + typed   │           │
              questions → Cascade(Cached(judge), …)          │           │
            → decide: keep veto → first exclusive match →    │           │
              tags accumulate → safety downgrade/uncertainty │           │
            → destination template / pick_from → guards ─────┴──► entries (policy)
                                                                        │
                                       L1 wins on overlapping paths ◄───┘
                                       → plan file (JSONL), entries pending
                                         (or approved_by = "policy" in autonomous mode,
                                          never when uncertain)
```

### Review, apply, undo

```
review   TUI | $EDITOR (rebase-style) | --approve/--reject + filters → status, approved_by = "human"
apply    for each approved entry:
           re-fingerprint (dev, ino, size, mtime; dir summary; BLAKE3 for dedupe) → skip if changed
           re-run guards + rule checks (e.g. extracted-sibling)
           journal "begin" (undo state) → act → journal "copied" (cross-volume) → "done" | "fail"
undo     journal "undo-begin" → reverse (restore from trash/quarantine, move back, restore tags;
         archives: move the quarantined original back, delete the .tar.zst) → "undo-done" | "undo-fail"
recover  on the next apply/undo: reconcile journal + filesystem; never delete a source unless the
         destination is intact; dry runs never recover
```

### Watch mode (`tidy watch`)

`notify` (FSEvents on macOS, inotify on Linux) watches the scopes of the action rules, recursively. Each changed path must stay unchanged for `--settle` seconds (default 2). Partial downloads (`.crdownload`, `.part`, `.download`, …), dotfiles and `node_modules` are ignored. Settled paths go through the same L2 engine and are appended to a watch plan. In autonomous mode, entries the policy approved are applied straight away, with the full minimum age still enforced for removals. Measured latency from arrival to plan entry is 2.1–2.6 s.

### Interactive UI (`tidy ui`)

`tidy ui [PATH]`, or plain `tidy` in a terminal, runs a ratatui app with three screens:

- **Explore** browses the directory index, sorted by size, name, age or item count. It shows the cached index (§5.1) at once and replaces it when a background scan finishes; after an apply or undo it reloads the patched index instead of rescanning. Folders and their totals come from the index; the files of the folder on screen are read on demand, so memory stays proportional to folders. Each folder found by the detectors carries a detector tag. The details panel shows guard verdicts, computed from the scan's data so there is no second walk. Marked items become a trash or quarantine plan through the same code as MCP proposals (`propose.rs`), but approved by the human.
- **Plans** reuses the review component of `tidy review` (`tui.rs`, saving after each change). Apply always runs a dry run first, shows the result, and waits for `y`. Undo also asks first.
- **Cleanup** runs `plan_detectors` and `policy_entries` on one folder and shows scan and judge progress as they happen.

Scans, explanations, rule evaluation, proposals, cleanups, applies and undos each run on a worker thread and report over a channel (`app/worker.rs`). The screen redraws every 100 ms while work runs, and every 500 ms otherwise. Scans can be cancelled through `ScanProgress`.

Dialogs queue: one never replaces another. Dialogs from finished jobs wait until no text is being typed, then ignore keys for 400 ms, so a stray keystroke can't confirm an apply. Edits to a plan are refused while a job runs on it. Child processes run with captured output, so they never draw over the screen.

### Agents (`tidy mcp`)

A JSON-RPC 2.0 MCP server over stdio. It accepts protocol versions 2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05.

- **Tools:** `tidy_scan`, `tidy_top`, `tidy_explain`, `tidy_plan`, `tidy_propose`, `tidy_plans`, `tidy_show_plan`, `tidy_reject`, `tidy_apply`, `tidy_undo`, `tidy_policy_test`, `tidy_dupes`.
- **Agents propose and reject; they never approve.** `tidy_apply` runs only entries with `approved_by` set to `human` or `policy`.
- **No flag injection.** Plans are addressed by id only. Tools that start a subprocess (`tidy_plan`, `tidy_dupes`) reject paths that start with `-` and pass the rest after `--`; `tidy_dupes` also validates `min_size` and binds it as `--min-size=…`.

## 5. Subsystems

### 5.1 Scanner (`tidy-scan`)

| Piece | Design |
|---|---|
| Linux reader | `open(O_DIRECTORY \| O_NOFOLLOW)` + `getdents64` into a reused buffer, then `statx(AT_STATX_DONT_SYNC)` relative to the directory fd. FIEMAP shared-extent flags give unshared bytes (`--private`) on btrfs and xfs. |
| macOS reader | `getattrlistbulk` returns name, type, device, file id, flags, mtime, link count, total and allocated size for many entries per syscall. `ATTR_CMNEXT_PRIVATESIZE` gives APFS unshared bytes in the same call. Handles the `ERANGE` retry quirk. |
| Portable reader | `std::fs` fallback (compiles on Windows; not tuned). |
| Walker | One rayon job per directory, work-stealing. The worker reading a directory owns its node, so per-file accounting needs no locks. Shared state is limited to an atomic id counter and a sharded hardlink set (touched only when `nlink > 1`); errors are counted per worker. The pool has 2× cores threads, clamped to 4–32, because walking is latency-bound. Never follows symlinks; stays on one device by default. On Linux, a directory with more than 4,096 entries has the rest stat-ed in chunks of 2,048 on other threads, with the worker's lock released meanwhile (rayon may run other directories on the waiting thread). Each chunk is totalled where it is read (a partial node, top-K and subfolder list) and the partials merge pairwise, so memory stays at the names of the remaining entries (about 20 MB per million). A single 1M-entry directory went from 2.95 s to 1.53 s on 2 vCPU. |
| Index | Directories only: a flat arena of 64-byte POD `DirNode`s (parent, name offset, flags, dirs, files, logical, allocated, private, own bytes, newest mtime) plus one name buffer. A child's id is always greater than its parent's, so one reverse pass rolls totals up. Files are aggregated into their directory; only per-thread top-K heaps keep the largest files. |
| Markers | While reading, flags record `CACHEDIR.TAG`, `Cargo.toml`, `package.json`, `pyvenv.cfg`, `.git` and similar. Detectors then need no second pass. |
| Snapshots | `TIDYIDX\x01` header, JSON header, POD nodes, names, then the top files. Loading is one read plus a cast, then a validation pass (every parent before its children, names in bounds). Writes go to a per-process temporary file and are renamed into place. `tidy scan --save` keeps one rolling "latest" plus history per root for `tidy diff`. |
| Index cache | Every full scan (`scan`, `top`, `diff`, `plan`, the UI, MCP) is also saved as `auto.idx` in its root's cache folder, with at least the 64 largest files: about 7 MB per 100k folders, saved in ~3 ms, loaded in 7–8 ms (`tidy scan --cached` takes 9 ms end to end, against 1.9 s for the scan). `cache::load` takes the newest suitable index: the folder's own, or one cut out of a cached ancestor's (`Index::extract`, ~10 ms for 93k files of a 1M-file index). Writes hold a `flock` on the index's lock file. Suitable means same mount handling, unshared bytes if asked for, and a long enough largest-files list (`ScanStats::top_k`). The newest 64 indexes are kept, up to 1 GiB; the least recently used go first. |
| Patching | `tidy_plan::sync` wraps every apply and undo (CLI, UI, MCP, watch). Before, `patch::Watch` lists the folder holding each entry and that folder's parent (only inside cached roots), plus the plan's own files and XDG `.trashinfo` files; a folder that compress rewrites in place is marked for a rescan. Afterwards it lists them again, plus where items landed and every folder above that up to the cached root (quarantine creates `<plan>/<entry>/`). `Index::patch` plans first and changes nothing unless every folder checks out. A folder the index still matched before the change gets the before→after difference; one the index had fallen behind on, or only seen afterwards, is set to what is there now. Subfolders that disappeared are dropped (compacting the arena, so ids stay parent-first); new ones, or ones replaced under the same name (a different inode), are scanned and grafted on. Hard links refuse the patch (where a linked file counts is a scan's call), as does a rewritten root. `cache::sync` then, under the lock: leaves alone an index scanned after the change, drops one scanned while it ran or one the patch refused, and patches the rest. Moving 100 folders (40k–98k files) out of a 1M-file index: 21–25 ms, against 1.9–2.0 s to rescan. |

### 5.2 Filesystem operations (`tidy-ops`)

- **Fingerprints:** files use (dev, ino, size, mtime). Directories use a fresh subtree summary (file count, apparent bytes, newest mtime). Dedupe entries add a BLAKE3 hash.
- **Safe primitives:**
  - `rename_noreplace` (no clobbering).
  - `remove_tree` and `copy_tree` never follow symlinks or cross mount points.
  - `copy_tree` carries Linux user xattrs.
  - `same_content` never follows symlinks or compares a file with itself.
  - `unique_in` handles name conflicts.
  - `free_bytes`.
- **Trash:** `~/.Trash` on macOS. On Linux, the XDG trash spec: `.trashinfo` files, home trash or a sticky `$topdir/.Trash`, chosen by checking the device of the nearest existing ancestor.
- **Quarantine:** Tidy's own holding area on the same volume (`<data>/quarantine`, or `<volume>/.tidy-quarantine/<uid>` on other volumes), bucketed per plan and entry. Roots on other volumes are recorded in `<data>/quarantine-roots` when first used, so `tidy purge` empties every root without probing mounts; volumes that aren't mounted are skipped.
- **Probes:**
  - open files (`lsof` on macOS, `/proc` on Linux)
  - git state (dirty, unpushed and stashed work; whether a path is ignored)
  - cloud roots (iCloud Drive, `~/Library/CloudStorage` providers, Dropbox, OneDrive, Google Drive, pCloud)
  - mount points
- **Metadata:** provenance (`kMDItemWhereFroms`, quarantine agent and last-used date on macOS; `user.xdg.origin.url` on Linux). Finder tags are read and written via a binary-plist codec; Linux uses `user.xdg.tags`.
- **Space actions:**
  - *Duplicates:* size, then a head + middle + tail sample hash, then full BLAKE3, each stage in parallel; hard links count once.
  - *Dedupe:* `clonefile` + atomic rename on macOS; `FIDEDUPERANGE` on Linux, where the kernel verifies the bytes.
  - *Archive:* `.tar.zst` via multithreaded zstd, verified by reading it back; the original is quarantined.
  - *Compress:* APFS decmpfs via `ditto --hfsCompression` with an atomic swap, or btrfs `defragment -czstd`. Compressibility is estimated by sampling with zstd-1.

### 5.3 Plans (`tidy-plan`)

- **Plan file:** JSONL with one header line (id, roots, mode, policy path and hash, judge id) and one line per entry. Saved atomically (temp file plus rename) under `<data>/plans/<id>.jsonl`.
- **Entry fields:**
  - what to do: `action` (delete, trash, quarantine, move, copy, rename, tag, dedupe, compress, archive), `path`, optional `dest` with `create_dest`, `on_conflict`, `tags`
  - status: `status` and `approved_by`
  - checks and context: `bytes`, `fingerprint`, `source` (layer and rule), `reason`, `alternatives`, raw judge `evidence`
  - undo: `undo` capability, then the `outcome` once applied
- **Journal:** append-only and fsync'd. It records `begin` (with undo state such as previous tags or the archive path), `copied`, `done` (the full outcome), `fail`, `undo-begin`, `undo-done`, `undo-fail` and `recovered`.
- **Executor:** same-volume moves are atomic renames. Cross-volume moves copy, verify, fsync, journal `copied`, and only then remove the source. Name conflicts follow the rule's `on_conflict`: `skip` (the default), `suffix`, or `dedupe-else-suffix`, which quarantines the source when the destination is identical.
- **Review:** shared filters (rule, action, status, minimum p, path prefix), bulk status changes, one-line rendering with `⚠ uncertain`, and the rebase-style editor format.

### 5.4 Rules (`tidy-rules`)

**L0 guards**, checked in this order. Rows marked ✱ apply only to actions that move, remove or rewrite the item (delete, trash, quarantine, move, rename, archive, dedupe, compress); tag and copy skip them.

| # | Guard | Blocks |
|---|---|---|
| 1 | root | the filesystem root |
| 2 | system | system locations (`/System`, `/usr`, `/etc`, `/Library`, `/private/var`, …) |
| 3 | root | home itself; ✱ the standard folders themselves (Desktop, Documents, …) |
| 4 | secrets | credentials (`~/.ssh`, `~/.aws`, `~/Library/Keychains`, `.netrc`, `.git-credentials`, …) |
| 5 | app-data ✱ | application data (`~/Library/Mail`, `Containers`, `Application Support`, …), unless a detector marks it regenerable |
| 6 | protect | your `protect = [...]` paths |
| 7 | destination ✱ | anything inside a move rule's destination |
| 8 | package | inside bundles (`.app`, `.photoslibrary`, …); media libraries |
| 9 | cloud | cloud-synced folders (unless allowed); dataless placeholders |
| 10 | immutable | locked or immutable files |
| 11 | mount ✱ | directories containing mount points |
| 12 | recent ✱ | removals and in-place rewrites (dedupe, compress) newer than `min_age` (7 days); moves and renames newer than a settle time (120 s) |
| 13 | open ✱ | files that are open |
| 14 | git ✱ | tracked or not-ignored files; repos with uncommitted, unpushed or stashed work, no upstream, or unreadable state |

Move destinations get their own check: never into system, credential, app-data or package locations.

**L1 detectors** are deterministic: build artefacts need a marker file next to them, and caches match known paths (`tidy detectors`). The 18 ids: `cargo-target`, `node-modules`, `python-venv`, `python-cache`, `swift-build`, `gradle-build`, `maven-target`, `js-build-cache`, `flutter-build`, `cmake-build`, `cachedir-tag`, `xcode-derived-data`, `xcode-device-support`, `simulator-caches`, `user-caches`, `model-caches`, `package-caches` and `logs` (children of `~/Library/Logs` older than 30 days).

**Policy** (`~/.config/tidy/policy.toml`): mode, guard knobs, detector settings, judge backends, subjects (reference photos), presets and `[[rule]]`s.
- A rule has an `id`, an `action`, a `scope`, a `filter` (kind, ext, name globs, size, age, depth), criteria (`when.any`, `when.all`, `when.none`) and, for moves, `to` (a path template or `pick_from`).
- Optional fields: `protects`, `check`, `always_review`, `content`, `threshold` and `mode`.

**Presets** are rule packs compiled into the binary, all off by default: `installers`, `screenshots`, `stale-downloads`, `archive-leftovers` and `protect.personal`. A user rule with the same id replaces the preset's. Every preset except `archive-leftovers` brings its own `policy test` cases.

**L2 engine** (`l2.rs`):
- **Applicable rules:** scope, filter and code check. A keep rule counts only when an action rule applies too. A removal-only keep rule counts only when a lossy removal applies.
- **Questions:** one predicate per criterion. A `choice` question is added for `pick_from`. Removals get two safety questions: *regenerable?* (predicate) and *loss cost* (a 0–3 score).
- **Combining answers:**
  - Keep rules veto at τ = 0.15. An unanswered keep rule also vetoes (fail-safe).
  - The first exclusive rule with p ≥ τ wins (default τ is 0.8).
  - Below that, the best rule with p ≥ max(0.5, τ − 0.25) is proposed as *uncertain*.
  - Tags accumulate.
  - A model-decided `delete` becomes `trash` unless regenerable ≥ 0.8 and loss ≤ 1.
  - A loss score ≥ 2.5, a missing loss answer, or `always_review` marks the entry uncertain.
- **Templates:** `{mtime:%Y}`, `{exif.date:%Y-%m}`, `{name}`, `{stem}`, `{ext}`, `{kind}`, `{choice}`.
- **Evidence** (what the model sees): name, home-relative path, kind, size, age, folder summary, and provenance (download URL, last opened, cloud state). With `content = true` it also gets a 2 KB text excerpt or a 512 px JPEG thumbnail (decoded with DCT scaling; HEIC and RAW via `sips` or `heif-convert`). When a criterion mentions a subject (`{me}`), the subject's reference photos and the item's thumbnail are attached even without `content = true`.

**Checks** are code predicates a rule can require. `extracted-sibling` reads a zip's central directory (ZIP64-aware) and compares size and CRC-32 against the folder next to it. Checks re-run at apply time, and lossless checks are exempt from removal-only keep rules.

**Calibration:** `tidy calibrate` collects labels (`labels.jsonl`) and fits a threshold per judge and rule (`calibration.json`). Action rules are fitted to a target precision (0.95); keep rules aim for full recall. Precedence is: your explicit threshold, then calibration, then a preset's threshold, then the default.

### 5.5 Judges (`tidy-judge`)

| Piece | Design |
|---|---|
| IR | A `Request` holds shared text, images and independent typed questions (predicate, choice, score). A `Response` holds an answer per question plus token usage and latency. `Caps` records image support, the maximum number of images and the maximum number of questions. |
| OpenAI | `POST /v1/decisions` with model `gpt-6-luna`. Input is text, or a user message with `input_text` and `input_image` data URLs. Maps the predicate, choice and score answer types. |
| SystemOne | `{model, state, questions: {name: {type: noul \| choice \| score, instructions, criteria}}}` for TypeSafe Jev (`/v1/systemone`) and Cloudflare Clef / Clef-flash (Workers AI `{result}` envelope, `images` up to 4). |
| Mock | Deterministic offline judge for tests and demos. |
| Cache | Persistent JSONL per judge id, keyed by a BLAKE3 digest of the request (evidence, image hashes, questions). Rewording one criterion re-asks only the affected items. |
| Cascade | Cheap model first. A later stage re-asks when any predicate lands in the uncertain band (0.2–0.8 by default) or is refused. |
| Concurrency | `decide_many` runs scoped threads (32 by default) over blocking `ureq`. A shared AIMD gate halves concurrency on HTTP 429 and grows it back on success. Auth and config errors stop the batch at once. Requests over the question limit are split and merged. |
| Keys | From the named environment variable, else `fnox get NAME`. Credentials are only ever sent over HTTPS (plain HTTP only to localhost). |

### 5.6 CLI (`tidy-cli`)

| Area | Commands |
|---|---|
| Interactive | `ui [PATH]`, or plain `tidy` in a terminal |
| Analyse | `scan`, `top`, `diff` |
| Plans | `plan`, `plans`, `show`, `review`, `apply`, `undo`, `explain`, `purge` |
| Space | `dupes [--plan]`, `archive [--transparent]` |
| Policy | `init`, `presets [NAME]`, `detectors`, `policy check`, `policy test`, `calibrate` |
| Judges | `judge ping`, `judge ask`, `bench judges` |
| Automation | `watch`, `mcp` |

Commands that take a folder default to the current folder (MCP's `tidy_scan` and `tidy_top`: the server's). `latest` is the default plan for `review`, `apply`, `undo` and `show`. Main commands emit JSON or NDJSON for scripts. `NO_COLOR` is respected. `SIGPIPE` keeps its default action, so piping into `head` works.

## 6. Concurrency and resources

| Work | Model |
|---|---|
| Directory walk, file visits | dedicated rayon pools, 2× cores (4–32 threads) |
| Duplicate hashing | rayon global pool (one thread per core) |
| Judge calls | scoped OS threads over blocking HTTP, with the AIMD gate |
| BLAKE3 | multithreaded (`rayon` feature) and memory-mapped for large files |
| zstd archives | multithreaded (`zstdmt`) |
| Watch | one `notify` watcher thread plus the main loop |
| `tidy ui` | the UI thread draws and handles keys; each job (scan, explain, rules, propose, cleanup, apply, undo) gets its own thread and reports over a channel |

There is no async runtime: network I/O is the only blocking wait that matters, and threads handle it well at this scale. Memory is bounded by the directory count (64 B per directory plus names) and the top-K heaps, not by the file count.

## 7. State on disk

| What | Where (macOS / Linux) | Override |
|---|---|---|
| Policy, tests | `~/.config/tidy/policy.toml`, `policy.tests.toml` | `TIDY_POLICY`, `TIDY_CONFIG_DIR`, `XDG_CONFIG_HOME` |
| Calibration, labels | `~/.config/tidy/calibration.json`, `labels.jsonl` | `TIDY_CONFIG_DIR`, `XDG_CONFIG_HOME` |
| Plans | `<data>/plans/<id>.jsonl` | `TIDY_HOME` |
| Journal | `<data>/journal/` | `TIDY_HOME` |
| Quarantine | `<data>/quarantine/<plan>/<entry>/`, or `<volume>/.tidy-quarantine/<uid>` | `TIDY_HOME` |
| Quarantine roots on other volumes | `<data>/quarantine-roots` | `TIDY_HOME` |
| Decision cache | `<data>/decisions/<judge-hash>.jsonl` | `TIDY_HOME` |
| Index cache, snapshots | `<cache>/snapshots/<root-hash>/` (`auto.idx`; `latest.idx` and history from `scan --save`) | `TIDY_CACHE_DIR`, `XDG_CACHE_HOME` |

`<data>` is `~/Library/Application Support/tidy` on macOS and `$XDG_DATA_HOME/tidy` (default `~/.local/share/tidy`) on Linux. `<cache>` is `~/Library/Caches/tidy` on macOS and `~/.cache/tidy` on Linux.

## 8. Platform matrix

| Capability | macOS (APFS) | Linux | Fallback |
|---|---|---|---|
| Bulk walk | `getattrlistbulk` | `getdents64` + `statx` | `readdir` + `lstat` |
| Unshared bytes | `ATTR_CMNEXT_PRIVATESIZE` | FIEMAP shared flag (btrfs, xfs) | allocated bytes |
| Dedupe | `clonefile` + rename | `FIDEDUPERANGE` | trash the copy |
| Transparent compression | decmpfs (`ditto`) | btrfs `defragment -czstd` | `.tar.zst` archive |
| Trash | `~/.Trash` | XDG trash | quarantine |
| Change feed | FSEvents | inotify | rescan |
| Provenance | `kMDItemWhereFroms`, quarantine agent, last used | `user.xdg.origin.url` | — |
| Tags | Finder tags (bplist xattr) | `user.xdg.tags` | — |
| Thumbnails | JPEG/PNG/WebP/TIFF in-process; HEIC/RAW via `sips` | in-process; HEIC via `heif-convert` | metadata only |
| Notifications (`watch --notify`) | `osascript` | `notify-send` | — |

## 9. Dependencies

### Toolchain

- **Rust 1.88+**, edition 2024 (let-chains), resolver 3.
- **A C compiler** for `blake3` (SIMD) and `zstd-sys`. `scripts/check-macos.sh` substitutes a stub compiler to type-check the macOS target from Linux.
- **Release profile:** `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`. A `profiling` profile keeps symbols.
- **Personal builds** add `RUSTFLAGS="-C target-cpu=native"`.

### Rust crates (direct)

| Crate | Version | Used by | Why |
|---|---|---|---|
| `libc` | 0.2 | scan, ops, plan, cli | Raw syscalls: `getdents64`, `statx`, `getattrlistbulk`, `clonefile`, ioctls, xattrs, `renameat2`/`renamex_np` |
| `rayon` | 1.12 | scan, ops, bench | Work-stealing parallelism for walks and hashing |
| `parking_lot` | 0.12 | scan, ops, rules, judge | Small, fast mutexes |
| `rustc-hash` | 2 | scan | Fast hashing for the hardlink set |
| `bytemuck` | 1.25 | scan | POD casts for `DirNode` and snapshots |
| `blake3` | 1.8 (`mmap`, `rayon`) | ops, plan, rules, judge, cli | Content hashes, cache keys, fingerprints |
| `zstd` | 0.13 (`zstdmt`) | ops | Archives and compressibility sampling |
| `tar` | 0.4 | ops | `.tar.zst` archives |
| `serde`, `serde_json` | 1 | all but bench | Plans, journal, cache, API bodies |
| `toml` | 0.9 | rules, cli | Policy, presets, tests |
| `thiserror` | 2 | scan, ops, plan, judge | Typed errors |
| `anyhow` | 1 | plan, rules, cli, bench | Error context |
| `ureq` | 3.4 (`rustls`, `platform-verifier`, `gzip`) | judge | Blocking HTTPS that trusts the OS certificate store |
| `base64` | 0.22 | judge | Image data URLs |
| `image` | 0.25 (jpeg, png, webp, gif, bmp, tiff) | rules | Thumbnails |
| `jpeg-decoder` | 0.3 | rules | DCT-scaled JPEG decode (fast thumbnails) |
| `crc32fast` | 1 | rules | Zip CRC check (`extracted-sibling`) |
| `clap` | 4.6 | cli, bench | Command line |
| `ratatui`, `crossterm` | 0.30, 0.29 | cli | `tidy ui` and `tidy review` |
| `notify` | 8 | cli | FSEvents and inotify for `watch` |
| `memmap2` | 0.9 | workspace | Declared for snapshot mapping but not used directly; linked through `blake3`'s `mmap` feature |
| `tempfile` | 3 | dev | Test sandboxes |

The dependency graph has 174 unique packages (including 13 proc-macros and Tidy's own 6 crates). The release binary is 7.7 MB stripped on Linux x86_64.

### External programs (optional, used when present)

| Program | Used for |
|---|---|
| `git` | Guard: work-tree state, ignored paths |
| `lsof` | Guard: open files (macOS; Linux reads `/proc`) |
| `fnox` | API key lookup when the environment variable is unset |
| `sips` (macOS), `heif-convert` (Linux) | HEIC and RAW thumbnails |
| `ditto` (macOS), `btrfs` (Linux) | Transparent compression |
| `osascript` / `notify-send` | Desktop notifications in `watch --notify` |
| `$VISUAL` / `$EDITOR` | `tidy review --edit` |

### External services (only for L2 rules)

| Service | Endpoint | Key |
|---|---|---|
| OpenAI Decisions API, `gpt-6-luna` | `https://api.openai.com/v1/decisions` | `OPENAI_API_KEY` |
| TypeSafe Jev (SystemOne) | `https://api.typesafe.ai/v1/systemone` | `TYPESAFE_API_KEY` |
| Cloudflare Clef / Clef-flash | `https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/run/@cf/cloudflare/clef` | `CLOUDFLARE_API_TOKEN` |

Scanning, detectors, guards, plans, space actions and deterministic rules (including the `screenshots` and `archive-leftovers` presets) work fully offline.

## 10. Testing and tooling

- `cargo test` runs 65 tests:
  - scanner (totals against a reference walk and `du`, lookups and top files, snapshot round-trip and diff, a 12,000-entry folder split across 1, 2 and 8 threads)
  - index cache and patching, each checked folder by folder against a fresh scan: cache round trip and choice of index, cut-outs, patches after removals, renames, new nested folders and restores, folders rewritten or replaced under the same name, an index that had fallen behind, landing in folders the change created, hard links (refused), and syncing cached indexes, including a scan that ran during the change
  - filesystem ops (trash and quarantine round-trips, symlink safety, mount listing, drift detection, dupes and archives, bplist and tags)
  - plans (11 tests: apply/undo round-trips, stale entries, crash recovery including an interrupted cross-volume move, dry runs, the editor format, the cached index after apply and undo)
  - judges (body/parse against the docs, cache, split)
  - L2 semantics with scripted judges
  - presets end to end
  - MCP approval enforcement
  - the interactive UI, headless (ratatui test backend): browse, mark, plan, dry run, apply and undo shown without a rescan, starting from the cached index, cleanup, dialog queueing and keystroke safety, non-UTF-8 names
- `flake.nix` packages Tidy for Nix (`x86_64-linux`, `aarch64-linux`, `aarch64-darwin`) and provides an overlay, a NixOS/nix-darwin module, a home-manager module (policy written and checked at build time, `tidy watch` as a systemd or launchd user service) and a dev shell. `nix flake check` builds the package, runs the tests in the build sandbox, and checks Rust and Nix formatting. Files are in `nix/`.
- `scripts/install.sh` runs `cargo install` (tuned for the CPU unless `--portable`) and checks that `tidy` on PATH is this binary, not macOS's `/usr/bin/tidy` (HTML Tidy).
- `scripts/check-macos.sh` type-checks the macOS code paths from Linux (`aarch64-apple-darwin`) with a stub C compiler.
- `bench/run.sh` compares `tidy scan` with du, dust, dua and gdu using hyperfine on `tidy-fixture` trees. Results are in `bench/results-linux-x86_64.md`.
- Live checks (`tidy policy test`, `tidy bench judges`) run against the real judge and are cached.
