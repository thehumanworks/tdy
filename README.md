# tdy

Analyse, clean up, compress and organise the files on your disk — fast, frugal, and reversible.

- **Fast scanner.** Parallel, work-stealing walk over `getattrlistbulk` (macOS) or `getdents64` + `statx` (Linux). Memory scales with directories, not files (≈150 MB for 10M files).
- **Plans, not surprises.** Every action is an entry in a plan you review. Nothing touches disk until you approve it. Applies are journalled, crash-safe and undoable.
- **Rules in plain English.** A decision model (OpenAI `gpt-6-luna` via the Decisions API, or SystemOne-compatible Jev / Clef) judges your rules. Code-level guards always win. The model picks between options; it never writes paths.
- **Space without deleting.** Duplicates become copy-on-write clones (APFS `clonefile`, btrfs/xfs `FIDEDUPERANGE`). Cold folders get archived or transparently compressed.
- **Agent-ready.** NDJSON/JSON output on the main commands, plus an MCP server where agents can propose but never approve.

## Install

```sh
scripts/install.sh               # cargo install, tuned for this CPU; then checks which `tdy` your shell finds
scripts/install.sh --portable    # the same without -C target-cpu=native
```

**Renamed from `tidy`:** the command is `tdy`, so it no longer clashes with macOS's HTML Tidy at `/usr/bin/tidy`. On first run, `tdy` moves your policy, plans and caches from the old `tidy` folders to `tdy` ones and leaves links behind, so old plans can still be undone. `TIDY_*` environment variables are now `TDY_*`. Remove the old binary with `cargo uninstall tidy-cli`.

To build without installing: `cargo build --release` (binary at `target/release/tdy`). Requires Rust 1.88+. The release profile uses fat LTO, one codegen unit and `panic = "abort"`.

### Nix: NixOS, home-manager, nix-darwin

The repository is a flake (`x86_64-linux`, `aarch64-linux`, `aarch64-darwin`). To try it without installing:

```sh
nix run ~/projects/tidy -- --help
```

Add it to your own flake. Use `github:<owner>/tdy` instead of the local path once the repository is pushed:

```nix
inputs.tdy = {
  url = "git+file:///Users/tomas/projects/tidy";
  inputs.nixpkgs.follows = "nixpkgs";   # needs a nixpkgs with Rust 1.88+
};
```

**System-wide** (NixOS, or nix-darwin with `darwinModules.default`):

```nix
imports = [ inputs.tdy.nixosModules.default ];
programs.tdy.enable = true;
```

**Per user** (home-manager), with your policy and the optional watch service:

```nix
imports = [ inputs.tdy.homeModules.default ];
programs.tdy = {
  enable = true;
  # Written to ~/.config/tdy/policy.toml and checked with `tdy policy check`
  # at build time, so a broken policy fails the switch, not the next plan.
  settings = {
    presets = [ "screenshots" "archive-leftovers" ];
    judge.default = "luna";
    judge.backends.luna = { kind = "openai-decisions"; model = "gpt-6-luna"; api_key_env = "OPENAI_API_KEY"; };
    rule = [ { id = "protect.finance"; action = "keep"; scope = [ "~/Downloads" ]; when.any = [ "Tax or finance documents." ]; } ];
  };
  policyTests = [ { name = "tax return is protected"; item = { name = "2024-tax-return.pdf"; folder = "~/Downloads"; }; expect = "protect.finance"; } ];
  watch = {
    enable = true;                      # systemd user service on Linux, launchd agent on macOS
    paths = [ "~/Downloads" "~/Desktop" ];
    environmentFile = "/run/user/1000/secrets/tdy.env";   # OPENAI_API_KEY=…; a string path, so it stays out of the Nix store
  };
};
```

**Other outputs:**
- `overlays.default` adds `pkgs.tdy`.
- `packages.<system>.default.override { withHeif = true; }` adds `heif-convert` for HEIC thumbnails on Linux.
- `devShells.default` (`nix develop`) has the Rust toolchain, clippy, rust-analyzer and hyperfine.
- `nix flake check` builds the package, runs the test suite, and checks Rust and Nix formatting.

**Notes:**
- The package appends a minimal `git` to `tdy`'s PATH, so the git guard works even where git isn't installed If git is already on your PATH, `withGit = false` drops it and shrinks the closure from about 170 MiB to 55 MiB. Changing these runtime tools only rebuilds a small wrapper, not Tidy itself.
- The watch service runs `tdy` by its full store path.
- nixpkgs' `html-tdy` also provides `bin/tdy`, so don't install both into the same profile.
- On macOS, privacy protection can stop the watch agent reading `~/Desktop`, `~/Documents` or `~/Downloads`. If `~/Library/Logs/tdy-watch.log` shows `Operation not permitted`, give the `tdy` binary Full Disk Access in System Settings → Privacy & Security. Its store path changes with each update, so you'll need to grant it again.

## Quick start

```sh
tdy                           # interactive UI in a terminal (same as `tdy ui .`)
tdy scan ~                    # where the space goes (dust-style tree)
tdy top ~ -n 20               # largest files   (--dirs: by bytes held directly)
tdy plan ~                    # detectors + your policy rules → a plan
tdy ask trash installers in Downloads older than 30 days   # a plan from plain English
tdy review                    # TUI (or --edit for $EDITOR, or --approve --rule X)
tdy apply --dry-run           # rehearse: verifies fingerprints, guards, destinations
tdy apply
tdy undo                      # reverse what can be reversed
```

`latest` is the default plan for `review`, `apply`, `undo` and `show`. You can also pass a plan id or a unique prefix.

## Interactive UI

`tdy ui [PATH]` opens a full-screen app; so does plain `tdy` in a terminal. It has three screens: switch with `1` `2` `3` or Tab, and `?` lists the keys on each.

| Screen | What you do there |
|---|---|
| **Explore** | Browse where the space goes, sorted by size, name, age or item count. It opens on the index of the last scan at once and refreshes it in the background. Folders come from the index; files are read when you open their folder. Detectors tag build output and caches. `i` shows sizes and guard verdicts; `E` asks your policy rules. Mark items with space; `t` or `z` turns them into a trash or quarantine plan, approved by you. `p` makes a plan for the folder on screen; `a` asks about it in plain English. |
| **Plans** | Review any plan: approve, reject, filter, details. `X` applies it after a dry run and your confirmation; `U` undoes it. A log shows each step. `n` makes a new plan. |
| **New plan** | Make a plan for a folder, four ways (←→ to switch): **Cleanup** runs the detectors and your policy rules; **Ask** takes a request in plain English and shows how it was understood as you type; **Duplicates** and **Archive** find identical files and large cold folders. Progress is live (files scanned, items judged), and the finished plan opens on Plans. |

Nothing changes on disk until you confirm an apply, which uses the same guards, fingerprint checks and journal as `tdy apply`. Scans, judging and applies run in the background, so the screen stays responsive, and a dialog from a finished job never replaces one you have open. After an apply or undo, Explore shows the change without rescanning (see [Speed](#speed)).

## Commands

Commands that take a folder (`ui`, `scan`, `top`, `diff`, `plan`, `dupes`, `archive`, `calibrate`) default to the current folder.

| Command | What it does |
|---|---|
| `ui [PATH]` | The interactive UI (also plain `tdy` in a terminal) |
| `scan`, `top`, `diff` | Size tree, largest files/dirs, changes since the last `scan --save`. `--cached` answers from the last scan instead of scanning |
| `plan [PATHS]` | L1 detectors + L2 policy rules → plan (`--no-rules`, `--no-detectors`, `--show-blocked`) |
| `ask REQUEST` | A plan from plain English, e.g. `tdy ask move videos over 1GB to ~/Archive` (`--explain` shows how it was understood, `--from` sets the folder) |
| `plans`, `show` | List plans; print entries (filters: `--rule`, `--action`, `--status`, `--min-p`, `--under`) |
| `review` | Approve or reject entries: TUI, `--edit`, `--approve-all`, `--approve/--reject` + filters |
| `apply`, `undo` | Execute approved entries (`--dry-run`, `--entry`); reverse applied ones |
| `explain PATH` | Which detector, guard or rule decides this path, with probabilities |
| `dupes [--plan]` | Duplicates (size → sample hash → BLAKE3); the plan clone-dedupes or trashes copies |
| `archive [--transparent]` | Cold, compressible folders → `.tar.zst` (original quarantined) or APFS/btrfs compression |
| `watch` | Watch rule scopes; judge new files once they settle; autonomous mode applies right away |
| `init` | Write an example policy to `~/.config/tdy/policy.toml` |
| `presets [NAME]` | List built-in rule presets (● = enabled), or print one to copy and adapt |
| `policy check`, `policy test` | Validate the policy; run `policy.tests.toml` cases, plus enabled presets' own cases, through the judge |
| `judge ping`, `judge ask` | Talk to the configured models directly |
| `calibrate` | Label sample items; fit per-rule thresholds for the current judge |
| `bench judges` | Agreement, p50/p95 latency and cost of each backend on your labels |
| `purge [--older 30d] --yes` | Permanently empty old quarantine buckets |
| `mcp` | MCP server on stdio |

## Asking in plain English

`tdy ask` (or **Ask** on the UI's New plan screen) turns a request into a one-off rule and makes a plan from it. The parts code can check become filters; whatever is left describes the items, and the judge decides which match.

| You write | It becomes |
|---|---|
| `trash`, `remove`, `get rid of` · `permanently delete` · `quarantine` · `move … to X` · `copy … to X` · `tag … as X` · `archive` · `compress` | the action (`remove` and `delete` mean the trash unless you say `permanently`) |
| `in Downloads`, `in ~/Desktop`, `in "My Stuff"` | the folder, searched with everything inside (default: the current folder) |
| `older than 30 days`, `not opened in the last 6 months`, `more than a year old` · `changed in the last 2 weeks` | age filters |
| `over 1GB`, `bigger than 500 MB` · `under 1KB` | size filters |
| `installers`, `videos`, `folders`, `pdfs`, `dmg files` … | kind or extension filters |
| anything else (`screenshots`, `old tax receipts`, `photos of my dog`) | what the judge checks each item against |

Entries always wait for your review, and your keep rules (presets included) still protect what they protect. A request that the filters fully describe (`trash dmg files older than a year`) needs no judge.

## The policy file

`~/.config/tdy/policy.toml`. Run `tdy init` for a commented example.

```toml
mode = "manual"            # "autonomous": entries the policy is sure about are pre-approved
min_age = "7d"             # removals never touch anything changed more recently
protect = ["~/Documents/Taxes"]
presets = ["screenshots", "archive-leftovers"]   # built-in rule packs, see below

[detectors]                # deterministic artifact/cache cleanup (L1)
min_size = "10MB"
action = "delete"          # or "trash"

[judge]
default = "luna"           # or: cascade = ["clef-flash", "luna"]
[judge.backends.luna]
kind = "openai-decisions"
model = "gpt-6-luna"
api_key_env = "OPENAI_API_KEY"     # read from env, else `fnox get OPENAI_API_KEY`

[subjects]
me = { refs = ["~/.config/tdy/subjects/me-1.jpg"] }

[[rule]]
id = "protect.finance"
action = "keep"            # keep rules veto at a low bar (0.15) — and when unanswered
scope = ["~/Downloads", "~/Desktop"]
when.any = ["Tax returns, invoices, contracts, payslips or bank statements."]

[[rule]]
id = "photos.dogs"
action = "move"
scope = ["~/Downloads", "~/Desktop"]
filter = { kind = "image", min_size = "50KB" }
content = true             # send a 512px thumbnail (opt-in; metadata only by default)
when.any = ["a picture of a dog", "a picture of {me} next to a dog"]
to = { path = "~/Pictures/Dogs/{exif.date:%Y}", create_if_missing = true }
on_conflict = "dedupe-else-suffix"

[[rule]]
id = "file.docs"
action = "move"
scope = ["~/Downloads"]
filter = { ext = ["pdf"], older_than = "3d" }
when.any = ["a business document"]
to = { pick_from = "~/Documents/*" }    # the judge chooses an existing folder (or none)
```

- **Actions:** `keep` (`protects = "removal"` vetoes only removals, so organising rules still apply), `delete`, `trash`, `quarantine`, `move`, `copy`, `rename` (`name = "{exif.date:%Y-%m-%d} {stem}.{ext}"`), `tag` (`tags = [...]`; Finder tags on macOS, `user.xdg.tags` on Linux), `archive`, `compress`.
- **Filters:** `kind` (file, dir, any, image, video, audio, document, archive, installer, code), `ext`, `name` (a glob or a list of globs), `min_size`, `max_size`, `older_than`, `newer_than`, `depth`.
- **Criteria:** `when.any`, `when.all` and `when.none`. A rule with no `when` is deterministic and needs no model.
- **Checks:** `check = "extracted-sibling"` runs a code check before a rule applies, and again at apply time. It holds when the folder next to a `.zip` has every file in it with identical content (size and CRC-32).
- **Review:** `always_review = true` marks a rule's entries uncertain, so they are never pre-approved.
- **Templates:** `{mtime:%Y}`, `{exif.date:%Y-%m}`, `{name}`, `{stem}`, `{ext}`, `{kind}`, `{choice}`.

**How rules combine:**
- Keep rules veto first.
- For moves and removals, the first rule that clears its threshold wins; the runner-up is recorded as an alternative.
- Tags accumulate.
- A permanent `delete` decided by the model is downgraded to `trash` unless the judge also rates the item regenerable (≥ 0.8) and cheap to lose (≤ Minor).
- Items the judge rates costly to lose, or that land just below a threshold, are proposed but marked **⚠ uncertain**. They stay pending even in autonomous mode.

**Thresholds:** the defaults are 0.8, and 0.15 for keep rules. `tdy calibrate` fits them per rule and per judge to a target precision; the result is stored in `~/.config/tdy/calibration.json`.

**Tests for your policy** live in `~/.config/tdy/policy.tests.toml`. Run them with `tdy policy test` (answers are cached, so re-runs are free). `expect` takes a rule id, `none`, `keep` (any keep rule) or a list of these:

```toml
[[case]]
name = "tax return is protected"
item = { name = "2024-tax-return.pdf", folder = "~/Downloads", size = "2MB", age = "400d" }
expect = "protect.finance"
```

### Presets

Built-in rule packs. All are off until you list them in `presets = [...]`.

| Preset | Action | Model | What it does |
|---|---|---|---|
| `installers` | trash | yes | Installers and disk images in `~/Downloads`, older than 30 days and ≥ 5 MB, that can be downloaded again |
| `screenshots` | move | no | Screenshots on the Desktop or in Downloads → `~/Pictures/Screenshots/<yyyy-mm>` |
| `stale-downloads` | quarantine | yes | Top-level `~/Downloads` items untouched for a year that look like one-off downloads; always marked uncertain |
| `archive-leftovers` | trash | no | A `.zip` whose extracted folder next to it holds identical copies of every file |
| `protect.personal` | keep (removals) | yes | Finance, legal and identity papers, medical records, personal photos and videos, and your own work are never removed. Moves and tags still apply. |

- **Overriding:** a rule of yours with the same `id` replaces the preset's rule. `tdy presets <name>` prints a preset's TOML to start from.
- **Tests:** each enabled preset adds its own cases to `tdy policy test`, unless you replaced its rule.
- **`protect.personal`** is off by default. It only asks the model about items a lossy removal rule would touch, so it adds no calls for moves, and none for `archive-leftovers` (whose check proves nothing is lost).
- **Without a judge:** model-decided rules are skipped with a warning. Deterministic presets (`screenshots`, `archive-leftovers`) still work, and keep rules that can't be answered protect the item.

## Safety model

1. **L0 guards** are code and absolute:
   - system locations, credentials (`~/.ssh`, keychains, …), app data (unless regenerable)
   - inside packages and bundles, and media libraries
   - cloud-synced folders and dataless placeholders
   - locked files, open files, and directories containing mount points
   - git work trees with uncommitted, unpushed or stashed work; tracked files
   - recent changes (removals: `min_age`; moves: a short settle time)
   - move destinations: never into system, credential, app-data or package locations
2. **L1 detectors** propose regenerable artifacts only when the marker file is present (`target/` next to `Cargo.toml`, `node_modules` next to `package.json`, a valid `CACHEDIR.TAG`, …). `tdy detectors` lists all of them.
3. **L2 rules** go through the judge as described above.
4. **You approve.** `apply` re-fingerprints every entry (device, inode, size, newest mtime, file count, hash for dedupes) and re-runs the guards. Changed items are skipped.

**Journal and recovery:**
- `apply` journals intent before every change and the outcome after, fsync'd.
- After a crash, the next `apply` or `undo` reconciles from the journal plus the filesystem.
- An interrupted cross-volume move never deletes the source unless the destination is intact.

**Reversibility:**
- `trash` (OS trash), `quarantine` (Tidy's same-volume holding area, purged with `tdy purge`), moves, renames, tags and archives can all be undone.
- `delete`, `dedupe` and `compress` are marked `undo ✗`. Dedupe and compress keep the content identical; delete is for regenerable artifacts.

## Agents (MCP)

```sh
claude mcp add tdy -- ~/.cargo/bin/tdy mcp   # Claude Code; any MCP client works
```

Use the absolute path: an MCP client may start the server with a minimal PATH.

**Tools:** `tdy_scan`, `tdy_top`, `tdy_explain`, `tdy_plan`, `tdy_propose`, `tdy_plans`, `tdy_show_plan`, `tdy_reject`, `tdy_apply`, `tdy_undo`, `tdy_policy_test`, `tdy_dupes`. `tdy_scan` and `tdy_top` take `cached: true` to answer from the last scan.

**What agents can do:** propose and reject entries. `tdy_apply` runs only entries a human approved (TUI, editor or CLI), or entries the policy approved in autonomous mode.

## Platforms

| | macOS (APFS) | Linux |
|---|---|---|
| Bulk walk | `getattrlistbulk` | `getdents64` + `statx` |
| Unshared bytes (`--private`) | `ATTR_CMNEXT_PRIVATESIZE` (same syscall) | FIEMAP shared flag (btrfs, xfs) |
| Dedupe | `clonefile` + atomic rename | `FIDEDUPERANGE` (kernel-verified) |
| Transparent compression | decmpfs via `ditto --hfsCompression` | btrfs `defragment -czstd` |
| Trash | `~/.Trash` | XDG trash (`.trashinfo`) |
| Watch | FSEvents | inotify |
| Thumbnails | JPEG (DCT-scaled), PNG/WebP/TIFF, HEIC/RAW via `sips` | same, HEIC via `heif-convert` if installed |

Windows is not supported yet. The `std::fs` fallback reader compiles there but nothing is tuned for it.

## Benchmarks

`bench/run.sh` compares `tdy scan` with du, dust, dua and gdu using hyperfine, on a generated fixture tree (`tdy-fixture`). On a 2-vCPU Linux VM with a warm cache and 1M files in 100k directories:

| tool | time | peak RSS |
|---|---|---|
| tdy | 1.55 s | 18 MB |
| du | 3.11 s | 2 MB |
| dust | 3.86 s | 448 MB |
| gdu | 3.92 s | 26 MB |
| dua | 6.23 s | 11 MB |

## Speed

Tidy keeps the index of every scan it makes (`~/Library/Caches/tdy` or `~/.cache/tdy`; the newest 64, up to 1 GiB) and keeps it current through its own changes:

- **Instant start.** `tdy ui` shows the last index at once and refreshes it in the background. `tdy scan --cached` and `tdy top --cached` answer from it and say how old it is. A folder inside a cached one is cut out of the parent's index.
- **No rescan after an apply or undo.** Tidy lists the folders an apply or undo touches, before and after, and patches every cached index that covers them. New folders (an undo restoring one, say) are scanned on their own.
- **Huge folders use every core.** A folder with more than 4,096 entries is read on several threads.

Changes made outside Tidy show up at the next scan; the cached index's age is always shown.

Measured with `tdy-cachebench` and hyperfine on the same 2-vCPU VM, 1M files in 100k folders, warm cache:

| | time |
|---|---|
| full scan | 1.8–2.1 s |
| `tdy scan --cached`, end to end | 9 ms |
| a 93k-file subfolder: scan / cut out of the cached index | 180–210 ms / 10 ms |
| after moving 100 folders (40k–98k files) out: rescan / patch | 1.9–2.0 s / 21–25 ms |
| moving them back: patch, scanning only those folders | 120–340 ms |
| one folder holding 1M files: before / after splitting big folders | 2.95 s / 1.53 s (peak memory 5 → 26 MB) |

## Development

```sh
cargo test                    # unit + integration tests (Linux or macOS)
scripts/check-macos.sh        # type-check the macOS paths from Linux
bench/run.sh                  # needs hyperfine (+ du/dust/dua/gdu if you want them compared)
```

Code map: `tdy-scan` (readers, walker, index, snapshots, index cache and patching), `tdy-ops` (fingerprints, trash/quarantine, safe fs ops, dupes/archive/compress, probes), `tdy-plan` (plan model, journal, apply/undo/recover, review), `tdy-rules` (guards, detectors, policy, L2 engine, media), `tdy-judge` (OpenAI Decisions, SystemOne, cache, cascade), `tdy-cli` (CLI, interactive UI, watch, MCP), `tdy-bench` (fixtures, `tdy-cachebench`).

Docs: [architecture](docs/architecture.md) (goal, design, dependencies), [assessment](docs/assessment.md) (strengths, weaknesses, future ideas), [design spec](docs/design.md) (original spec and build decisions).
