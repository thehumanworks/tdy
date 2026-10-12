# tidy

Analyse, clean up, compress and organise the files on your disk — fast, frugal, and reversible.

- **Fast scanner.** Parallel, work-stealing walk over `getattrlistbulk` (macOS) or `getdents64` + `statx` (Linux). Memory scales with directories, not files (≈150 MB for 10M files).
- **Plans, not surprises.** Every action is an entry in a plan you review. Nothing touches disk until you approve it. Applies are journalled, crash-safe and undoable.
- **Rules in plain English.** A decision model (OpenAI `gpt-6-luna` via the Decisions API, or SystemOne-compatible Jev / Clef) judges your rules. Code-level guards always win. The model picks between options; it never writes paths.
- **Space without deleting.** Duplicates become copy-on-write clones (APFS `clonefile`, btrfs/xfs `FIDEDUPERANGE`). Cold folders get archived or transparently compressed.
- **Agent-ready.** NDJSON/JSON output on the main commands, plus an MCP server where agents can propose but never approve.

## Install

```sh
scripts/install.sh               # cargo install, tuned for this CPU; then checks which `tidy` your shell finds
scripts/install.sh --portable    # the same without -C target-cpu=native
```

**macOS:** the system ships an unrelated HTML Tidy at `/usr/bin/tidy`. If `tidy --help` prints `Config: unknown option: help`, that one comes first on your PATH. Put `~/.cargo/bin` before `/usr/bin` (the installer prints the line to add) and check with `which -a tidy`.

To build without installing: `cargo build --release` (binary at `target/release/tidy`). Requires Rust 1.88+. The release profile uses fat LTO, one codegen unit and `panic = "abort"`.

### Nix: NixOS, home-manager, nix-darwin

The repository is a flake (`x86_64-linux`, `aarch64-linux`, `aarch64-darwin`). To try it without installing:

```sh
nix run ~/projects/tidy -- --help
```

Add it to your own flake. Use `github:<owner>/tidy` instead of the local path once the repository is pushed:

```nix
inputs.tidy = {
  url = "git+file:///Users/tomas/projects/tidy";
  inputs.nixpkgs.follows = "nixpkgs";   # needs a nixpkgs with Rust 1.88+
};
```

**System-wide** (NixOS, or nix-darwin with `darwinModules.default`):

```nix
imports = [ inputs.tidy.nixosModules.default ];
programs.tidy.enable = true;
```

**Per user** (home-manager), with your policy and the optional watch service:

```nix
imports = [ inputs.tidy.homeModules.default ];
programs.tidy = {
  enable = true;
  # Written to ~/.config/tidy/policy.toml and checked with `tidy policy check`
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
    environmentFile = "/run/user/1000/secrets/tidy.env";   # OPENAI_API_KEY=…; a string path, so it stays out of the Nix store
  };
};
```

**Other outputs:**
- `overlays.default` adds `pkgs.tidy`.
- `packages.<system>.default.override { withHeif = true; }` adds `heif-convert` for HEIC thumbnails on Linux.
- `devShells.default` (`nix develop`) has the Rust toolchain, clippy, rust-analyzer and hyperfine.
- `nix flake check` builds the package, runs the test suite, and checks Rust and Nix formatting.

**Notes:**
- The package appends a minimal `git` to `tidy`'s PATH, so the git guard works even where git isn't installed If git is already on your PATH, `withGit = false` drops it and shrinks the closure from about 170 MiB to 55 MiB. Changing these runtime tools only rebuilds a small wrapper, not Tidy itself.
- Nix profiles come before `/usr/bin` on PATH, so the macOS HTML Tidy clash goes away. The watch service runs Tidy by its full store path.
- nixpkgs' `html-tidy` also provides `bin/tidy`, so don't install both into the same profile.
- On macOS, privacy protection can stop the watch agent reading `~/Desktop`, `~/Documents` or `~/Downloads`. If `~/Library/Logs/tidy-watch.log` shows `Operation not permitted`, give the `tidy` binary Full Disk Access in System Settings → Privacy & Security. Its store path changes with each update, so you'll need to grant it again.

## Quick start

```sh
tidy                           # interactive UI in a terminal (same as `tidy ui .`)
tidy scan ~                    # where the space goes (dust-style tree)
tidy top ~ -n 20               # largest files   (--dirs: by bytes held directly)
tidy plan ~                    # detectors + your policy rules → a plan
tidy review                    # TUI (or --edit for $EDITOR, or --approve --rule X)
tidy apply --dry-run           # rehearse: verifies fingerprints, guards, destinations
tidy apply
tidy undo                      # reverse what can be reversed
```

`latest` is the default plan for `review`, `apply`, `undo` and `show`. You can also pass a plan id or a unique prefix.

## Interactive UI

`tidy ui [PATH]` opens a full-screen app; so does plain `tidy` in a terminal. It has three screens: switch with `1` `2` `3` or Tab, and `?` lists the keys on each.

| Screen | What you do there |
|---|---|
| **Explore** | Browse where the space goes, sorted by size, name, age or item count. Folders come from the scan; files are read when you open their folder. Detectors tag build output and caches. `i` shows sizes and guard verdicts; `E` asks your policy rules. Mark items with space; `t` or `z` turns them into a trash or quarantine plan, approved by you. |
| **Plans** | Review any plan: approve, reject, filter, details. `X` applies it after a dry run and your confirmation; `U` undoes it. A log shows each step. |
| **Cleanup** | Pick a folder and run detectors and policy rules, with live progress (files scanned, items judged, cost). The result opens as a plan. |

Nothing changes on disk until you confirm an apply, which uses the same guards, fingerprint checks and journal as `tidy apply`. Scans, judging and applies run in the background, so the screen stays responsive, and a dialog from a finished job never replaces one you have open.

## Commands

| Command | What it does |
|---|---|
| `ui [PATH]` | The interactive UI (also plain `tidy` in a terminal) |
| `scan`, `top`, `diff` | Size tree, largest files/dirs, changes since the last `scan --save` |
| `plan [PATHS]` | L1 detectors + L2 policy rules → plan (`--no-rules`, `--no-detectors`, `--show-blocked`) |
| `plans`, `show` | List plans; print entries (filters: `--rule`, `--action`, `--status`, `--min-p`, `--under`) |
| `review` | Approve or reject entries: TUI, `--edit`, `--approve-all`, `--approve/--reject` + filters |
| `apply`, `undo` | Execute approved entries (`--dry-run`, `--entry`); reverse applied ones |
| `explain PATH` | Which detector, guard or rule decides this path, with probabilities |
| `dupes [--plan]` | Duplicates (size → sample hash → BLAKE3); the plan clone-dedupes or trashes copies |
| `archive [--transparent]` | Cold, compressible folders → `.tar.zst` (original quarantined) or APFS/btrfs compression |
| `watch` | Watch rule scopes; judge new files once they settle; autonomous mode applies right away |
| `init` | Write an example policy to `~/.config/tidy/policy.toml` |
| `presets [NAME]` | List built-in rule presets (● = enabled), or print one to copy and adapt |
| `policy check`, `policy test` | Validate the policy; run `policy.tests.toml` cases, plus enabled presets' own cases, through the judge |
| `judge ping`, `judge ask` | Talk to the configured models directly |
| `calibrate` | Label sample items; fit per-rule thresholds for the current judge |
| `bench judges` | Agreement, p50/p95 latency and cost of each backend on your labels |
| `purge [--older 30d] --yes` | Permanently empty old quarantine buckets |
| `mcp` | MCP server on stdio |

## The policy file

`~/.config/tidy/policy.toml`. Run `tidy init` for a commented example.

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
me = { refs = ["~/.config/tidy/subjects/me-1.jpg"] }

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

**Thresholds:** the defaults are 0.8, and 0.15 for keep rules. `tidy calibrate` fits them per rule and per judge to a target precision; the result is stored in `~/.config/tidy/calibration.json`.

**Tests for your policy** live in `~/.config/tidy/policy.tests.toml`. Run them with `tidy policy test` (answers are cached, so re-runs are free). `expect` takes a rule id, `none`, `keep` (any keep rule) or a list of these:

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

- **Overriding:** a rule of yours with the same `id` replaces the preset's rule. `tidy presets <name>` prints a preset's TOML to start from.
- **Tests:** each enabled preset adds its own cases to `tidy policy test`, unless you replaced its rule.
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
2. **L1 detectors** propose regenerable artifacts only when the marker file is present (`target/` next to `Cargo.toml`, `node_modules` next to `package.json`, a valid `CACHEDIR.TAG`, …). `tidy detectors` lists all of them.
3. **L2 rules** go through the judge as described above.
4. **You approve.** `apply` re-fingerprints every entry (device, inode, size, newest mtime, file count, hash for dedupes) and re-runs the guards. Changed items are skipped.

**Journal and recovery:**
- `apply` journals intent before every change and the outcome after, fsync'd.
- After a crash, the next `apply` or `undo` reconciles from the journal plus the filesystem.
- An interrupted cross-volume move never deletes the source unless the destination is intact.

**Reversibility:**
- `trash` (OS trash), `quarantine` (Tidy's same-volume holding area, purged with `tidy purge`), moves, renames, tags and archives can all be undone.
- `delete`, `dedupe` and `compress` are marked `undo ✗`. Dedupe and compress keep the content identical; delete is for regenerable artifacts.

## Agents (MCP)

```sh
claude mcp add tidy -- ~/.cargo/bin/tidy mcp   # Claude Code; any MCP client works
```

Use the absolute path: an MCP client may start the server with a PATH where macOS's `/usr/bin/tidy` comes first.

**Tools:** `tidy_scan`, `tidy_top`, `tidy_explain`, `tidy_plan`, `tidy_propose`, `tidy_plans`, `tidy_show_plan`, `tidy_reject`, `tidy_apply`, `tidy_undo`, `tidy_policy_test`, `tidy_dupes`.

**What agents can do:** propose and reject entries. `tidy_apply` runs only entries a human approved (TUI, editor or CLI), or entries the policy approved in autonomous mode.

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

`bench/run.sh` compares `tidy scan` with du, dust, dua and gdu using hyperfine, on a generated fixture tree (`tidy-fixture`). On a 2-vCPU Linux VM with a warm cache and 1M files in 100k directories:

| tool | time | peak RSS |
|---|---|---|
| tidy | 1.55 s | 18 MB |
| du | 3.11 s | 2 MB |
| dust | 3.86 s | 448 MB |
| gdu | 3.92 s | 26 MB |
| dua | 6.23 s | 11 MB |

## Development

```sh
cargo test                    # unit + integration tests (Linux or macOS)
scripts/check-macos.sh        # type-check the macOS paths from Linux
bench/run.sh                  # needs hyperfine (+ du/dust/dua/gdu if you want them compared)
```

Code map: `tidy-scan` (readers, walker, index, snapshots), `tidy-ops` (fingerprints, trash/quarantine, safe fs ops, dupes/archive/compress, probes), `tidy-plan` (plan model, journal, apply/undo/recover, review), `tidy-rules` (guards, detectors, policy, L2 engine, media), `tidy-judge` (OpenAI Decisions, SystemOne, cache, cascade), `tidy-cli` (CLI, interactive UI, watch, MCP), `tidy-bench` (fixtures).

Docs: [architecture](docs/architecture.md) (goal, design, dependencies), [assessment](docs/assessment.md) (strengths, weaknesses, future ideas), [design spec](docs/design.md) (original spec and build decisions).
