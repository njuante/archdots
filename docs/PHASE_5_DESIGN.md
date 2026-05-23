# Phase 5 — `archdots export` (Design)

Status: **approved** — implementation target for Phase 5.

This document specifies the design of `archdots export <profile>`: a CLI that
turns a profile into a publishable directory ready to push to GitHub. It also
introduces the new `archdots-core::exporter` module that holds the load-bearing
logic (planning, secret scanning, README rendering, atomic write).

The seven load-bearing decisions are also recorded as ADR-005 in
[`ARCHITECTURE.md`](./ARCHITECTURE.md).

---

## 1. Executive summary

`archdots export <profile>` closes the loop of the tool: from "I manage my rice
with archdots" to "here is my shareable repo". The command is **read-only on
the system** (it never modifies the live profile, journal, or snapshot store,
and it takes no lock) and **fail-secure on output** (any heuristic hit for a
secret aborts with a non-zero exit; opt-in is explicit and verbose).

The pipeline has three phases:

1. **Plan** — load the profile, list every `FileEntry`, classify each as
   `Include`, `ExcludeSensitivePath`, `ExcludeBySize`, `ExcludeBinary`,
   `ExcludeBrokenSymlink`, `ExcludeDirectory`, `MissingSource`, or
   `OutsideHome`. Build the `ExportPlan` in memory. Read-only.
2. **Scan** — for every `Include` item that looks like text, run the embedded
   `SecretScanner` over its bytes. Hits become `SecretFinding`s attached to the
   plan and, by default, **promote the plan to "not safe"** if any are
   `High`-severity.
3. **Write** — only after explicit user confirmation (or `--yes`), materialise
   the output directory atomically: populate a sibling staging directory, fsync
   it, then `rename` over the destination. Same atomicity pattern as the
   journal sidecar in Phase 2.

`--check` runs phases 1–2 and exits without writing. `--include-secrets` is the
only global override; it requires a TTY and a non-skippable typed prompt
(`I UNDERSTAND`), is rejected on non-TTY stdin, and is **not** implied by
`--yes`.

---

## 2. Pipeline overview

```
                       ┌──────────────────────────────────────────────┐
                       │  archdots export <profile> [--output DIR]    │
                       └──────────────────────────────────────────────┘
                                              │
                                              ▼
                                  ┌─────────────────────┐
                                  │ LOAD PROFILE        │  Profile::load_from_file
                                  │ + RESOLVE ENTRIES   │  → exit 3 on ProfileError
                                  └─────────────────────┘
                                              │
                                              ▼
                                  ┌─────────────────────┐
                                  │ PLAN                │  for each FileEntry:
                                  │ (no I/O writes)     │    lstat source → classify
                                  │                     │    → ExportPlan
                                  └─────────────────────┘
                                              │
                       ┌──────────────────────┼──────────────────────┐
                  source-missing            ok                outside-home /
                  (required)            (continue)            sensitive-path
                       │                    │                    │
                       ▼                    │                    ▼
              exit ExportError              │            ExcludeBySensitivePath /
              (or skip if !required)        │            ExcludeOutsideHome (recorded
                                            │            in plan; user decides via flag)
                                            ▼
                                  ┌─────────────────────┐
                                  │ SCAN                │  for each Include item:
                                  │ (text bytes only)   │    SecretScanner::scan(bytes)
                                  │                     │  → findings attached to plan
                                  └─────────────────────┘
                                              │
                                              ▼
                                  ┌─────────────────────┐
                                  │ DECIDE              │  is_safe(plan, opts)?
                                  │                     │  — abort | confirm | proceed
                                  └─────────────────────┘
                            ┌─────────────┼────────────────┐
                      not-safe         --check         safe
                            │             │              │
                            ▼             ▼              ▼
                  exit ExportError   print report   confirm (unless --yes)
                  (exit 2)           exit 0/2             │
                                                          ▼
                                              ┌─────────────────────┐
                                              │ WRITE               │  staging dir under
                                              │ (atomic)            │  output_dir.parent():
                                              │                     │    .archdots-export.tmp.XYZ
                                              │                     │  populate, fsync,
                                              │                     │  rename → output_dir
                                              └─────────────────────┘
                                                          │
                                                          ▼
                                              ┌─────────────────────┐
                                              │ REPORT              │  text or --json
                                              │                     │
                                              └─────────────────────┘
                                                          │
                                                          ▼
                                                       exit 0
```

### Read-only contract

`export` mutates **nothing** in `$XDG_DATA_HOME/archdots/` or
`$XDG_STATE_HOME/archdots/`. It takes **no lock** (Phase 2's `ApplyLock` is for
FS mutations to user dotfiles, which export does not perform). It does **not**
invoke `pacman`, an AUR helper, or any network endpoint. The Validator from
Phase 3 is intentionally **not** used: the README's dependency sections are
rendered straight from `Profile.dependencies`, which keeps `export` runnable on
non-Arch hosts (CI, macOS, etc.) and decouples "publish" from "audit".

---

## 3. Output directory structure

```
<output>/
├── README.md                    # generated; embedded template
├── archdots-profile.toml        # the profile, re-rooted (sources point into ./dotfiles/)
├── install.sh                   # POSIX sh; standalone (no archdots dependency)
├── .gitignore                   # sane defaults (lockfiles, caches, OS junk)
└── dotfiles/                    # copy of the profile's files
    ├── .config/
    │   ├── hypr/hyprland.conf
    │   └── waybar/config
    └── .zshrc
```

### Naming

- `archdots-profile.toml`, not `<profile-name>.toml` — keeps the import command
  discoverable (`cp archdots-profile.toml ~/.local/share/archdots/profiles/my-rice.toml`).
- `dotfiles/` lowercase — matches mainstream `dotfiles` GitHub repos so
  contributors don't have to guess.

### Layout inside `dotfiles/`

Paths are preserved **relative to `$HOME`**. If a `FileEntry` resolves to
`/home/u/.config/hypr/hyprland.conf`, it lands at
`dotfiles/.config/hypr/hyprland.conf`. The mapping from system path to repo
path is:

```
target_in_repo = "dotfiles" / target_relative_to_home(entry.target.resolved)
```

If a target resolves outside `$HOME`, the item is classified `OutsideHome`
and skipped with a warning (see decision A.4 and edge case #22). Targets
outside `$HOME` happen in unusual setups (e.g. `/etc/X11/xorg.conf.d/`) and
exporting them implies escalation we won't model.

### Re-rooted `archdots-profile.toml`

The exported `archdots-profile.toml` has every `entry.source` rewritten so the
profile is importable from the cloned repo:

- `source` is rewritten to be **relative to the repo root** and to point into
  `./dotfiles/...`.
- `target` is left unchanged (still references `~/`, `$VAR`, etc.).
- Excluded entries (sensitive paths, oversized files, broken symlinks) are
  **omitted** from the exported profile. A recipient `archdots apply` would
  otherwise fail on missing sources.

### Permissions

Copies preserve the **executable bit** (`entry.exec` or actual `mode & 0o111`).
They do **not** preserve uid/gid/mtime/xattrs/ACLs — same scope as snapshots
(PHASE_2 §3, "uid/gid and xattrs/ACLs are not captured in v1"). Same principle
here.

### Symlinks never land in the output

The export is always **self-contained**: when `FileEntry.source` is itself a
symlink (the typical archdots layout, where `$HOME` contains symlinks into a
separate dotfiles directory), both the scan and the copy operate on
`canonicalize(source)` — the real bytes pointed at. The output never contains a
symlink that might dangle on the recipient's machine. See decision A.4 and
edge cases #2, #3, #4.

---

## 4. Anatomy of the generated README

The template is embedded via `include_str!` (same pattern as
`data/known_dotfiles.toml`, `data/binary_providers.toml`). The renderer is a
minimal substitution layer over a fixed set of section markers — no
third-party template engine.

````markdown
# {profile.name}

{profile.description OR "A dotfiles profile generated with archdots."}

| | |
|---|---|
| Author    | {profile.author OR "—"} |
| Tags      | {profile.tags joined as `` `tag` ``} |
| WM        | {profile.wm.kind OR "—"} |
| Generated | {YYYY-MM-DD} with [archdots](https://github.com/njuante/archdots) |

## Screenshots

> _Add screenshots to `screenshots/` and reference them here. See **Adding screenshots** below._

## Dependencies

### Official repositories
{IF profile.dependencies.pacman is non-empty}
```sh
sudo pacman -S {pacman deps joined by spaces}
```
{ELSE}
_None declared._
{ENDIF}

### AUR
{IF profile.dependencies.aur is non-empty}
> Install with an AUR helper (e.g. [`paru`](https://github.com/Morganamilo/paru) or [`yay`](https://github.com/Jguer/yay)):
```sh
paru -S {aur deps joined by spaces}
```
{ELSE}
_None declared._
{ENDIF}

### Optional
{IF profile.dependencies.optional_pacman is non-empty}
```sh
sudo pacman -S {optional_pacman deps joined by spaces}
```
{ELSE}
_None declared._
{ENDIF}

## Files included

| Path in repo | Installed to | Mode |
|---|---|---|
{FOR each included FileEntry, sorted}
| `dotfiles/{rel}` | `{target_unexpanded}` | {symlink \| copy \| template} |
{ENDFOR}

{IF excluded items exist}
## Files excluded

These files were referenced by the profile but excluded from this export:

| Path | Reason |
|---|---|
{FOR each excluded item}
| `{target_unexpanded}` | {reason} |
{ENDFOR}
{ENDIF}

## Installation

### Standalone (no archdots required)

```sh
git clone <this-repo>.git
cd <this-repo>
./install.sh
```

`install.sh` will:
1. Install the listed pacman dependencies (you'll be asked for sudo).
2. Symlink every file under `dotfiles/` to the matching path in `$HOME`, backing up any existing target to `<path>.bak.YYYYMMDD-HHMMSS`.

AUR dependencies are listed but **not** auto-installed; pick your helper.

### Recommended: install with archdots

[archdots](https://github.com/njuante/archdots) is an atomic dotfile manager for Arch Linux. It provides apply / rollback / snapshots / dependency validation.

```sh
# Install archdots
paru -S archdots-bin   # or build from source

# Import this profile
mkdir -p ~/.local/share/archdots/profiles
cp archdots-profile.toml ~/.local/share/archdots/profiles/{profile.name}.toml

# Apply (atomic; will snapshot existing targets first)
archdots apply {profile.name}

# Inspect what changed / what would change
archdots diff {profile.name}
archdots check {profile.name}
```

## How this rice was assembled

This repository was generated by `archdots export {profile.name}`. archdots is an Arch-only Rust CLI/TUI that:

- Tracks which files belong to which rice (named profiles).
- Applies them atomically: every apply creates a snapshot first, so `archdots rollback` always works.
- Validates pacman/AUR dependencies before publishing — the dependency lists in this README come from the profile metadata, not from manual upkeep.

The contents of `dotfiles/` are an exact copy of the files on the author's system at export time. The `archdots-profile.toml` reproduces the same layout on any Arch box.

## Adding screenshots

1. Capture: `grim ~/screenshots/main.png` (Wayland) or `maim ~/screenshots/main.png` (X11).
2. Move the file into `screenshots/` in this repo.
3. Reference from the **Screenshots** section above with `![alt](screenshots/main.png)`.

---

_Generated by [archdots](https://github.com/njuante/archdots) v{archdots_version} on {YYYY-MM-DD}._
````

**Notes:**

- Strict separators between substitutions (`{...}`) and conditionals
  (`{IF ...}{ENDIF}` / `{FOR ...}{ENDFOR}`) mean we can render with a ~60-line
  parser — no `tera`, no `handlebars`.
- The "Files excluded" section is omitted when no exclusions exist; we don't
  leak the fact that we ran a scan unless it produced exclusions the user
  opted to skip.
- The "How this rice was assembled" section is the marketing sub-bullet. It is
  deliberately short and factual; no logo, no badges.
- The template is **not** user-overridable in v0.5. See decision C.

---

## 5. Decisions

### A. Privacy and safety — the load-bearing decision

**Premise:** an `export` that publishes a private SSH key to a public repo is
the worst bug we can ship. Every other ergonomics call in this design is
subordinate to this.

#### A.1 Defence layers

We use **three independent filters**, each of which can abort independently:

| Layer | Operates on | Default action on hit |
|---|---|---|
| **Sensitive-path filter** | The resolved target path AND `canonicalize(source)` | Exclude + warn (recorded in plan) |
| **Size / binary filter** | File size, sniff of first 8 KiB | Exclude + warn (size, magic bytes) |
| **Content scan (`SecretScanner`)** | File contents (text only), read via `canonicalize(source)` | **Abort the whole export** unless overridden, for `High`-severity hits |

Layer 1 is a known-pattern denylist. Layer 2 is a non-secret-specific sanity
check that also catches obvious leaks (`.kdbx`, `.gpg`, `.p12` binaries).
Layer 3 is the high-value detector for the common case: a credential pasted
into `.zshrc` or a token in `.config/foo/config.toml`.

#### A.2 Sensitive-path filter — embedded denylist

A `data/sensitive_paths.toml` table, embedded via `include_str!`. Entries are
matched against both the resolved target path (relative to `$HOME`) **and**
the canonicalized source path (relative to `$HOME`, when it resolves inside
`$HOME`). Either match excludes the item. Three match kinds:

| Kind | Example | Action |
|---|---|---|
| `prefix` | `.ssh/`, `.gnupg/`, `.aws/`, `.azure/`, `.gcloud/`, `.kube/`, `.docker/` | Exclude any file whose relative path starts with this. |
| `exact` | `.netrc`, `.git-credentials`, `.pgpass`, `.npmrc`, `.pypirc`, `.config/gh/hosts.yml`, `.config/git/credentials`, `.config/rclone/rclone.conf` | Exclude this exact path. |
| `suffix` | `.pem`, `.key`, `.p12`, `.pfx`, `.jks`, `.kdbx`, `.gpg`, `.wallet` | Exclude any file with this extension. |

The catalog also covers shell history files (`.bash_history`, `.zsh_history`,
`.psql_history`, `.mysql_history`, `.lesshst`) because they routinely contain
pasted secrets.

**Why the dual check (target AND canonicalized source).** The path filter only
inspecting `target` would miss a profile entry whose declared target path is
innocent (`~/foo`) but whose source is a symlink into `~/.ssh/`. By matching
the canonicalized source too, we catch that case at the path layer before the
content scan even runs.

The denylist ships with archdots and is reviewable in the repo. Updates are
normal PRs.

**Per-item override:** `--allow-path <glob>` whitelists a specific path against
the denylist. Multiple flags allowed; the glob matches the same relative-to-
`$HOME` form the filter uses. Each whitelist appears in the final report so
the user re-confirms.

#### A.3 Size / binary filter

- Files larger than **1 MiB** (configurable via `--max-bytes`) are excluded by
  default. Dotfiles are text; a 100 MiB binary in a profile is either a
  wallpaper (use a wallpaper repo) or an accident.
- Files whose first 8 KiB fail a UTF-8 / mostly-printable sniff are flagged as
  **binary**. Binary files **skip content scanning** (the scanner is
  text-aware) and are excluded by default. Override: `--allow-binary <glob>`
  per-path (repeatable), or `--allow-path <glob>`.

This filter catches `.kdbx`, compiled wallpaper packs, font files, and
similar — none of which belong in a dotfiles repo.

#### A.4 Content scan — `SecretScanner`

A small, embedded set of regex rules in `data/secret_patterns.toml`, each
with:

```toml
[[rule]]
id          = "aws-access-key-id"
description = "AWS Access Key ID"
pattern     = "AKIA[0-9A-Z]{16}"
severity    = "high"            # "high" | "medium"
```

Curated rules cover the high-confidence, low-false-positive cases:

- `aws-access-key-id` — `AKIA[0-9A-Z]{16}` — *high*
- `github-token` — `gh[ps]_[A-Za-z0-9]{36,}` — *high*
- `gitlab-token` — `glpat-[A-Za-z0-9_-]{20}` — *high*
- `slack-token` — `xox[baprs]-[A-Za-z0-9-]+` — *high*
- `stripe-secret` — `sk_(live|test)_[A-Za-z0-9]{24,}` — *high*
- `google-api-key` — `AIza[A-Za-z0-9_-]{35}` — *high*
- `private-key-header` — `-----BEGIN (RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----` — *high*
- `npm-auth-token` — `//.+/:_authToken=` — *high*
- `jwt` — `eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+` — *medium*
- `generic-assignment` — `(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*['"][A-Za-z0-9_/+=-]{16,}['"]` — *medium*

**Severity policy:**

- `high` hits → **abort** the export (even with `--yes`). Only
  `--allow-secret <rule_id>` per-rule or `--include-secrets` (global override)
  bypasses this.
- `medium` hits → **warn**, surface in the final report, but **proceed** if
  the user confirmed (or passed `--yes`). The CLI report shows the file:line
  + a redacted preview so the user can recheck.

Per-rule allowlisting uses `--allow-secret <rule_id>[:<glob>]`. Example:
`--allow-secret jwt:.config/foo/example.json` — allow only that path to keep
matching JWTs.

**Symlink handling.** All content reads — both for the scanner and for the
eventual copy in WRITE — operate on `canonicalize(source)`, not on the
symlink path itself. The export is always self-contained: no symlinks ever
land in `dotfiles/`. A `~/.zshrc` that is a symlink to
`~/dotfiles/.zshrc` is scanned and copied as its target's bytes. A `~/foo`
that symlinks into `~/.ssh/` is excluded by the sensitive-path filter's
canonicalized-source check (see A.2); if it weren't, the scanner would still
catch the private-key header on read.

**TOCTOU.** PLAN canonicalizes and stat's; WRITE re-canonicalizes and re-reads.
We do not re-scan during WRITE. A determined attacker with write access to
the user's `$HOME` between PLAN and WRITE can win this race; in that scenario
the user has already lost. Same posture as PHASE_2 §6 #12 ("source disappears
between plan and apply").

#### A.5 The `--include-secrets` nuclear option

This flag exists because we cannot reasonably enumerate every false positive,
and refusing to ever ship a workaround would push users to `cp -r ~/dotfiles
repo/` instead, which has zero safeguards.

Constraints when `--include-secrets` is used:

1. The flag is **never** implied by `--yes`. They are orthogonal.
2. Even with `--include-secrets`, the user gets an interactive
   `[type 'I UNDERSTAND' to continue]` prompt on stdin. Refusing this prompt
   is `exit 1`, not `exit 0`. On a non-TTY stdin the flag is rejected outright
   (`exit 3` with `refusing: --include-secrets requires a TTY`).
3. The flag prints a multi-line red banner listing **every finding by
   file:line** before the prompt, so the user sees what they're publishing.
4. The flag is logged via `tracing::warn!`. The final printed report
   enumerates every finding that was overridden; the `--json` output records
   the override under `summary.findings_overridden`.

`--include-secrets` does **not** override the sensitive-path filter. Paths
matched there must be opt-in with `--allow-path`. We separate "this is the
kind of file we don't ship" (path, policy) from "the contents tripped a
regex" (heuristic).

#### A.6 Default flow — `archdots export <profile>`

```
PLAN → SCAN → if any 'high' findings or sensitive paths → ABORT (exit 2)
            → else: print report (counts + first few findings)
            → confirm (unless --yes)
            → WRITE
```

**Defaults are strict.** A profile that mentions `~/.zshrc` containing an AWS
key cannot be exported with `export laptop --yes`. The user has to either fix
the source file, pass `--allow-secret aws-access-key-id` (after looking at
the finding), or `--include-secrets`. Strictness is the point.

#### A.7 What we explicitly do NOT do

- We don't try to redact secrets in-place. Writing a "sanitised" version of
  `.zshrc` into the export with the secret stripped is a great way to ship a
  config that no longer works on the recipient's machine. Out of scope.
- We don't scan binaries with entropy heuristics. False positives on font
  files etc. tank UX. Binaries are excluded by default; users who really want
  them use `--allow-binary <glob>`.
- We don't fetch `git diff` of the profile against last-applied to detect
  "recent additions look like keys." Out of scope; a Phase 5+ idea.
- We don't gate on a `.gitignore` or `.git/info/exclude` of the user's home
  directory. Source of truth is the profile contents.

#### A.8 Threat model — what we protect against, what we don't

| Threat | Protected? |
|---|---|
| User has an SSH key in `~/.ssh/id_ed25519` and the profile includes it | **Yes** — sensitive-path prefix filter (target path). |
| User has an innocent target name whose source symlinks into `~/.ssh/` | **Yes** — sensitive-path filter on `canonicalize(source)` (A.2). |
| User pasted an AWS key into `~/.zshrc` two months ago and forgot | **Yes** — content scan. |
| User has a custom-format token in a config and we have no regex for it | **No** — heuristic limit. `--check` lets the user eyeball the contents before publishing. README clearly says "review before publishing." |
| User passes `--include-secrets` to bypass the check on purpose | **Out of scope** — that's an informed decision, not a bug. We make sure they saw what they're doing. |
| User runs `archdots export` in a non-TTY CI script with `--include-secrets` | **Protected** — flag rejected when stdin is not a TTY. |
| Filesystem race: a secret appears in a file between PLAN and WRITE | **Mostly protected** — we re-`canonicalize` and re-read during WRITE, but do not re-scan. A determined attacker with write access to `$HOME` can win this race; in that scenario the user has already lost. |
| Encrypted password manager databases (`.kdbx`, `*.wallet`) | **Yes** — suffix filter excludes them. |

---

### B. Snapshot export vs profile export

**Decision:** support **both via `--format`**, with `full` as the default. No
hybrid mode.

| Format | Includes | When to use |
|---|---|---|
| `--format full` *(default)* | `README.md` + `dotfiles/` + `archdots-profile.toml` + `install.sh` + `.gitignore` | Sharing publicly. The recipient does not need archdots. |
| `--format profile-only` | `README.md` + `archdots-profile.toml` only | Pure versioning of the profile metadata; the dotfiles themselves live elsewhere (e.g. a separately maintained dotfiles repo). Sources in the exported `archdots-profile.toml` are left **as-is** (relative to `$HOME`) since there is no `dotfiles/` to point into. |

**Why not a "hybrid"?**

- A hybrid that copies dotfiles but also links to a master repo confuses
  recipients ("which is the source of truth, your repo or mine?"). Picking a
  model is clarifying.
- "Snapshot" / "Profile" both fit naturally into the existing vocabulary
  (Phase 2 has snapshots; profile-only export is just the metadata). No new
  mental concept introduced.

`profile-only` skips the SCAN phase (there are no copied dotfiles to scan).
The README is still generated. `--include-secrets` is meaningless with
`profile-only`; passing both → `exit 3` (`InvalidOptions`).

---

### C. README generation — fixed embedded template

**Decision:** one embedded template, no user override in v0.5.

| Option | Pros | Cons |
|---|---|---|
| **Embedded** *(chosen)* | Predictable output; greppable; no template ecosystem; one less attack surface (no remote template fetch) | Less customisable |
| External path (`--readme template.md`) | Power users can theme | Doubles the surface (path resolution, includes, undefined-var policy); too much work for v0.5 |
| Tera / handlebars | "Real" templating | New dep, new error class, gargantuan for a 200-line README |

A ~60-line substitutor handles the three constructs we need: `{var}`,
`{IF cond}…{ENDIF}`, `{FOR item IN list}…{ENDFOR}`. Errors during render are
`ExportError::TemplateRender` (`thiserror`) and indicate a programming bug in
the embedded template — they should not occur in a release build, same posture
as `DetectorError::ParseCatalog`.

**Screenshots:** placeholder section that documents `grim` / `maim` usage. We
do not generate or detect them. Custom override (a flag pointing to a
screenshots directory to embed) is a Phase 5+ candidate.

A future `--readme <path>` override is left as a non-breaking addition; the
template renderer is private behind `Exporter::render_readme`.

---

### D. Git init

**Decision:** **do not** init a git repo. The output is a plain directory.

| Option | Pros | Cons |
|---|---|---|
| **No git** *(chosen)* | No surprise commits; `gh repo create` is one user command away; no `git config` reading | Output directory is just files |
| `git init` + first commit | "One-shot" UX | Inherits the user's git config (potentially their work email), creates a default-branch name that doesn't match repo conventions |
| `git init` only (no commit) | Less invasive | Adds a `.git/` the user has to nuke if they wanted no git |

We **do** emit a final hint:

```
✓ Export complete: ./hyprland-rice-export/

Next steps:
  cd hyprland-rice-export
  git init
  git add .
  git commit -m "Initial commit (generated by archdots)"
  gh repo create --public --source=. --push
```

Users who routinely export with git wired up can wrap that in a shell
function. archdots stays out of git config / network territory.

---

### E. CLI flags — `archdots export <profile>`

```
archdots export <profile> [flags...]

Positional:
  <profile>                  Profile name (under $XDG_DATA_HOME/archdots/profiles/).

Output:
  -o, --output <DIR>         Destination directory. Default: ./<profile>-export/
      --force                Allow writing into an existing non-empty directory.
                             Existing files in the way are overwritten.

Format:
      --format <full|profile-only>
                             Output format. Default: full.

Safety overrides (use with care, repeatable, glob-aware):
      --allow-path <GLOB>    Whitelist a specific path that would otherwise be
                             excluded by the sensitive-path filter.
      --allow-binary <GLOB>  Whitelist a specific path that would otherwise be
                             excluded by the binary-content filter.
      --allow-secret <ID[:GLOB]>
                             Whitelist a secret-scanner rule (optionally only
                             when the match is in <GLOB>).
      --max-bytes <N>        Maximum per-file size in bytes (default: 1048576).
      --include-secrets      DISABLE the content-scan abort. Requires a TTY and
                             an interactive 'I UNDERSTAND' confirmation. Cannot
                             be skipped via --yes.

Workflow:
      --check                Run plan + scan, print report, exit. Writes nothing.
                             Exit 0 if clean, 2 if findings, 3 on error.
      --no-install-script    Do not generate install.sh.
      --no-readme            Do not generate README.md. (Power users / regenerators.)
  -y, --yes                  Skip the final "ready to write?" confirmation.
                             Does NOT bypass --include-secrets' typed prompt.
      --json                 Emit the export report as JSON to stdout.
```

`--allow-path`, `--allow-binary`, and `--allow-secret` all share the same
glob matcher (see G.6). All three are repeatable and operate on the same
relative-to-`$HOME` path form the filters use.

**Excluded from v0.5** (rejected during design, listed so we don't relitigate):

- `--git`: see decision D.
- `--readme <PATH>` for custom templates: see decision C.
- `--bundle <tar.gz|zip>`: archive output. Convenient but not part of the
  "publishable repo" mission. Phase 5+.
- `--include-snapshot <ID>`: export a specific snapshot's contents instead of
  the live filesystem. Interesting; needs design work on which manifest
  fields to surface in the README. Phase 5+.
- `--remote-create <gh|gl>`: out of scope, see decision D.

**Exit codes:**

| Code | Meaning |
|---|---|
| 0 | Export completed successfully (or `--check` produced a clean report). |
| 1 | User declined the confirmation prompt. |
| 2 | Secret-scan or sensitive-path findings blocked the export. `--check` exits 2 when findings exist. |
| 3 | Indeterminate: profile broken, source path unreadable, output dir unwritable, invalid flag combination, `--include-secrets` on non-TTY stdin. |

Exit-code precedence is `3 > 2 > 1 > 0`, matching Phase 3 (`check`).

---

### F. TUI integration in v0.5

**Decision:** **no TUI surface for export in v0.5.**

Reasons:

1. `export` is a "publish" workflow, not an "interact with my dotfiles"
   workflow. It's a one-shot terminal action, like `archdots init`.
2. The confirmation flow for `--include-secrets` (`type I UNDERSTAND`) is
   poorly served by a modal — a typed-confirmation modal is a new widget. CLI
   handles it naturally.
3. The TUI's existing "spawn task → modal on completion" pattern
   (PHASE_4 §5.5) would work, but it duplicates UI for a feature whose target
   user is the one publishing, not the one interacting.

In v0.6, a `[E]xport profile` action on `ProfilesView` could shell out to
`archdots export <name> --check`, render the report inline, and on accept fall
through to the CLI (with terminal restored). This is straightforward to add
later without rework.

---

### G. New core APIs

The exporter is a new core module. Existing modules are not modified beyond
adding the `Export` variant to `CoreError` (`thiserror` `#[non_exhaustive]`
makes this additive).

#### G.1 New module: `archdots_core::exporter`

```rust
//! Export a profile into a publishable directory.
//!
//! `Exporter` is read-only on the user's `$XDG_DATA_HOME` / `$XDG_STATE_HOME`
//! and never invokes pacman / aur / network. The pipeline is `plan` → `scan` →
//! `write`. The caller (binary) decides whether to gate `write` on
//! confirmation.

pub struct Exporter<'a> {
    profile: &'a Profile,
    profile_dir: &'a Path,
    home: &'a Path,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportPlan {
    pub profile_name: String,
    pub items: Vec<PlannedExportItem>,
    pub options: ExportOptions,           // record of the flags that built it
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedExportItem {
    pub entry_id: String,
    pub source: PathBuf,                  // absolute, resolved (pre-canonicalize)
    pub source_canonical: Option<PathBuf>,// absolute, post-canonicalize (None if broken)
    pub target: PathBuf,                  // absolute, resolved
    pub rel_in_repo: Option<PathBuf>,     // None when not included
    pub classification: ItemClassification,
    pub findings: Vec<SecretFinding>,     // empty until scan() has run
    pub size_bytes: Option<u64>,
    pub is_text: Option<bool>,
}

/// All variants are struct-form (including empty `{}` variants), so serde
/// external tagging emits a consistent object shape for every JSON
/// classification value — never a bare string. See §8.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ItemClassification {
    /// File is eligible to be copied into the export.
    Include {},
    /// Source did not exist at plan time.
    MissingSource {},
    /// Target resolved outside $HOME; the export refuses to ship it.
    OutsideHome {},
    /// Path (target or canonicalized source) matches the sensitive-path
    /// denylist.
    ExcludeSensitivePath { rule_id: String, kind: SensitivePathKind },
    /// File is larger than `max_bytes`.
    ExcludeBySize { size_bytes: u64, limit_bytes: u64 },
    /// File's first 8 KiB do not look like text.
    ExcludeBinary {},
    /// Source is a symlink whose target is missing / not readable.
    ExcludeBrokenSymlink {},
    /// Source resolved to a directory (recursive include is out of scope for
    /// v0.5; see edge case #16).
    ExcludeDirectory {},
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitivePathKind { Prefix, Exact, Suffix }

#[derive(Debug, Clone, Serialize)]
pub struct SecretFinding {
    pub rule_id: String,
    pub severity: SecretSeverity,
    pub line: u32,
    pub column: u32,
    pub preview: String,                  // redacted: first/last 3 chars + length
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretSeverity { High, Medium }

#[derive(Debug, Clone, Serialize)]
pub struct ExportOptions {
    pub format: ExportFormat,
    pub allow_paths: Vec<String>,         // raw globs from CLI
    pub allow_binary: Vec<String>,        // raw globs from CLI (per-path)
    pub allow_secret_rules: Vec<SecretAllowance>,
    pub max_bytes: u64,
    pub include_secrets: bool,
    pub include_install_script: bool,
    pub include_readme: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SecretAllowance {
    pub rule_id: String,
    pub path_glob: Option<String>,        // None ⇒ rule allowed everywhere
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat { Full, ProfileOnly }

#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub output_dir: PathBuf,
    pub items_included: usize,
    pub items_excluded_by_path: usize,
    pub items_excluded_by_size: usize,
    pub items_excluded_binary: usize,
    pub items_missing: usize,
    pub findings_high: usize,
    pub findings_medium: usize,
    pub findings_overridden: usize,       // hits cleared by --allow-secret / --include-secrets
    pub bytes_written: u64,
}

impl<'a> Exporter<'a> {
    pub fn new(profile: &'a Profile, profile_dir: &'a Path, home: &'a Path) -> Self;

    /// Build a plan without reading file contents. Stats each source (lstat +
    /// canonicalize), classifies it against the embedded sensitive-path and
    /// size filters, and records the final repo-relative path. Never writes
    /// anything.
    ///
    /// # Errors
    /// - `ExportError::Profile` when path resolution fails.
    /// - `ExportError::Io` when stat-ing a source fails for an *unexpected*
    ///   reason (broken symlinks / missing sources are recorded in the plan,
    ///   not returned as errors).
    pub fn plan(&self, opts: &ExportOptions) -> Result<ExportPlan, ExportError>;

    /// Read the contents of every `Include` item via `canonicalize(source)`
    /// and populate its `findings`. Idempotent: re-running clears and
    /// re-fills findings.
    ///
    /// # Errors
    /// - `ExportError::Io` if a file marked `is_text == true` becomes
    ///   unreadable between plan and scan.
    pub fn scan(&self, plan: &mut ExportPlan) -> Result<(), ExportError>;

    /// Returns `true` iff the plan can be safely written without further
    /// interactive confirmation. False if any `High` finding exists (not
    /// overridden by `--allow-secret`) or any `ExcludeSensitivePath` item
    /// without a matching `--allow-path` override. `opts.include_secrets`
    /// overrides the `High` finding gate (but not the path filter).
    #[must_use]
    pub fn is_safe_to_write(&self, plan: &ExportPlan, opts: &ExportOptions) -> bool;

    /// Materialise the plan to `output_dir`. Implementation:
    /// 1. Create a sibling staging dir `output_dir + ".archdots-export.tmp.<rand>"`.
    /// 2. Populate it (README, profile.toml, dotfiles/, install.sh, .gitignore).
    /// 3. fsync each written file and the staging dir.
    /// 4. Rename staging dir → output_dir (atomic within one FS).
    /// 5. On any error: remove the staging dir, propagate.
    ///
    /// Pre-existing `output_dir`: with `force`, we merge into it (files
    /// overwritten atomically via the same rename pattern). Without `force`,
    /// a non-empty `output_dir` returns `ExportError::OutputNotEmpty`.
    ///
    /// # Errors
    /// - `ExportError::Unsafe` when `is_safe_to_write` would return false.
    /// - `ExportError::OutputNotEmpty` if the directory exists and is non-empty
    ///   without `force`.
    /// - `ExportError::Io` on FS errors.
    pub fn write(
        &self,
        plan: &ExportPlan,
        output_dir: &Path,
        opts: &ExportOptions,
        force: bool,
    ) -> Result<ExportReport, ExportError>;

    /// Render the README from profile + plan. Pure; used internally by `write`
    /// and exposed so `--check` and `--json` callers can emit the text
    /// without touching disk.
    ///
    /// # Errors
    /// - `ExportError::TemplateRender` when the embedded template has an
    ///   unresolved placeholder (should not happen in a release build).
    pub fn render_readme(
        &self,
        plan: &ExportPlan,
        archdots_version: &str,
    ) -> Result<String, ExportError>;
}
```

#### G.2 New module: `archdots_core::exporter::scanner`

Public so future tooling (e.g. an `archdots audit <path>` command in a later
phase) can reuse it.

```rust
/// Stateless, embedded-rule secret scanner.
pub struct SecretScanner { /* compiled regexes from data/secret_patterns.toml */ }

impl SecretScanner {
    /// Construct the scanner with the embedded rule set.
    ///
    /// # Errors
    /// - `ExportError::ScannerInit` when the embedded patterns fail to compile
    ///   (programming error; should not happen in a release build).
    pub fn new() -> Result<Self, ExportError>;

    /// Scan `bytes` and return all findings. Operates on raw bytes via
    /// `regex::bytes::Regex` so non-UTF-8 inputs that are mostly printable
    /// (e.g. ISO-8859-1) still produce findings. Inputs that fail the binary
    /// sniff should not reach this function — that classification happens at
    /// plan time.
    #[must_use]
    pub fn scan(&self, bytes: &[u8]) -> Vec<SecretFinding>;
}
```

#### G.3 New error variant

```rust
// in archdots-core::error
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExportError {
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error("I/O error at {path}: {source}")]
    Io { path: PathBuf, #[source] source: std::io::Error },
    #[error("output directory exists and is non-empty: {0}")]
    OutputNotEmpty(PathBuf),
    #[error("export is not safe to write (findings or sensitive paths present); see plan")]
    Unsafe,
    #[error("template render error: {0}")]
    TemplateRender(String),
    #[error("invalid configuration: {0}")]
    InvalidOptions(String),               // e.g. ProfileOnly + IncludeSecrets
    #[error("scanner initialisation failed: {0}")]
    ScannerInit(String),
    #[error("invalid path: {0} (must be valid UTF-8)")]
    NonUtf8Path(PathBuf),
}

// CoreError gets:
#[error(transparent)]
Export(#[from] ExportError),
```

`ExportError` does NOT include a "secret found" variant: findings are data on
the plan, not errors. The caller (CLI) reads them and decides.

#### G.4 What we do not add

- No changes to `Profile`, `Validator`, `Linker`, `Snapshot`, `Journal`,
  `Lock`.
- No new traits.
- No new XDG paths.
- No on-disk state. `Exporter` holds borrows; lifetime ends with the call.

#### G.5 New embedded data files (in `archdots-core/data/`)

| File | Purpose |
|---|---|
| `sensitive_paths.toml` | Prefix / exact / suffix patterns (decision A.2). |
| `secret_patterns.toml` | Regex rules with severity (decision A.4). |
| `readme_template.md` | Rendered template (decision C, section 4). |
| `install_template.sh` | The `install.sh` body (POSIX sh, see §6). |
| `gitignore_template` | Default `.gitignore` body. |

All embedded via `include_str!`. Each parses at `Exporter::new` (cheap, µs).

#### G.6 New dependencies

| Crate | Reason |
|---|---|
| `regex = "1"` | Required by `SecretScanner`. New workspace dep; pin it explicitly. |

`walkdir = "2"` already exists in the workspace but is **not** used by
`exporter` — `FileEntry`s are explicit, no directory recursion in v0.5.
`globset` rejected to keep deps minimal: a ~50 LOC hand-rolled glob matcher
under `exporter::glob` covers `*` / `?` / `**` against unix paths, which is
all `--allow-path` / `--allow-binary` / `--allow-secret:<glob>` need.

---

### H. Edge cases

| # | Case | Handling |
|---|---|---|
| 1 | Source path is missing (regular file path that doesn't exist) | `ItemClassification::MissingSource`. If `entry.required == true`, abort with `ExportError::Io`; if `false`, exclude with warning. |
| 2 | Source is a **dangling symlink** | `ExcludeBrokenSymlink` — `canonicalize` errors. `required = true` → abort; `required = false` → exclude with warning. The path filter does **not** see a canonicalized source in this case (there is none), so only the target path is consulted. |
| 3 | Source is a **live symlink** (typical archdots layout) | **Resolve and copy bytes.** Both the SecretScanner and the file copy operate on `canonicalize(source)`. The export is always self-contained — no symlinks ever land in `dotfiles/`. |
| 4 | Source is a symlink that points at a sensitive target (e.g. `~/foo → ~/.ssh/id_ed25519`) | The sensitive-path filter matches against **both** the resolved target path AND `canonicalize(source)`. The latter catches the symlink-to-sensitive case. As defense-in-depth, the content scanner would also catch the private-key header on read. |
| 5 | Source is larger than `--max-bytes` (default 1 MiB) | `ExcludeBySize`. Always excluded by default; `--allow-path <glob>` includes. |
| 6 | Output dir exists and is non-empty | Without `--force`: `ExportError::OutputNotEmpty`. With `--force`: merge — staging dir built first, then renamed over; existing files outside our writeset are left alone; ours are atomically replaced. |
| 7 | Output dir overlaps with profile source paths | Compare `canonicalize(output_dir)` against every resolved source. Any source under `output_dir` → `ExportError::InvalidOptions("output directory overlaps with profile source paths")`. Prevents recursive-copy footgun. |
| 8 | Profile has no dependencies | "Dependencies" sub-sections render `_None declared._`. No empty `sudo pacman -S` commands generated. |
| 9 | Source has non-default permissions (e.g. `0755` script) | Preserve the executable bit (`mode & 0o111`). uid/gid/xattrs/ACLs are NOT preserved — same posture as snapshots (PHASE_2 §3). |
| 10 | File matches a `high` secret rule | `findings` populated; `is_safe_to_write == false`. CLI prints findings (file:line + redacted preview), exits 2. Override: `--allow-secret <rule_id>` per-rule, or `--include-secrets` global. |
| 11 | File matches a `medium` rule (e.g. JWT in a public-ish config) | `findings` populated with `severity: Medium`. Does **not** block. Shown in the report; CLI still prompts for confirmation as usual. `--yes` skips the prompt but findings still appear in printed text and `--json`. |
| 12 | `--include-secrets` on non-TTY stdin (CI) | Reject at flag-parse time: `exit 3` with "refusing: --include-secrets requires a TTY". |
| 13 | `--include-secrets` + `--yes` | Allowed for everything except the `I UNDERSTAND` prompt, which is non-skippable. `--yes` only skips the final pre-write confirmation. |
| 14 | Source bytes are not valid UTF-8 but first 8 KiB are mostly printable (rare: ISO-8859-1 config) | Treat as text; scan with `regex::bytes::Regex` over the byte slice. Curated rules' strict prefixes (`AKIA`, `gh[ps]_`, etc.) keep false positives low. |
| 15 | Two `FileEntry` items resolve to the same target | Already an error at `Profile::validate`. Out of scope here. |
| 16 | `FileEntry.source` resolves to a directory | `ItemClassification::ExcludeDirectory`. Per-item warning. Future: `--include-dir` for recursive include. The linker has the same restriction (PHASE_2 §6 #4). |
| 17 | Filesystem fills up during write | We're in the staging dir. Propagate `ExportError::Io`; remove the staging dir; `output_dir` untouched (atomicity preserved). |
| 18 | Output dir parent does not exist | `ExportError::Io` from staging-dir creation. We don't `mkdir -p` for the user; explicit is better. |
| 19 | Profile has `[hooks].pre_apply` / `post_apply` scripts | Script files are copied into `dotfiles/` under their relative paths; the exported `archdots-profile.toml` keeps `[hooks]`. The README adds a note: "this profile runs hook scripts; review them before applying." |
| 20 | `--format profile-only` + `--include-secrets` | `ExportError::InvalidOptions("--include-secrets is meaningless with --format profile-only")`. Exit 3. |
| 21 | User cancels at the final confirmation prompt | Print "Aborted." Exit 1. Staging dir (if any was created) is removed. Output dir untouched. |
| 22 | Target's relative-to-`$HOME` form contains `..` after normalisation (e.g. target was `$HOME/../shared`) | `OutsideHome`. Same as PHASE_2 §6 case 5 (`OutsideHome` conflict). |
| 23 | Embedded `secret_patterns.toml` is malformed (programmer bug) | `ExportError::ScannerInit`; CLI prints "internal error: please file an issue." Exit 3. Should not occur in release builds. |
| 24 | Two consecutive exports into the same `--output` without `--force` | First succeeds; second hits case 6. |
| 25 | `--output` path is a regular file (not a directory) | Pre-flight check (`is_file()`); `ExportError::Io` with a clear message. |
| 26 | TOCTOU: symlink target or file contents change between PLAN and WRITE | WRITE re-`canonicalize`s and re-reads (but does **not** re-scan). A determined attacker with write access to the user's `$HOME` can win this race; in that scenario the user has already lost. |

---

## 6. The `install.sh` template

```sh
#!/bin/sh
# Auto-generated by archdots. Do not edit by hand — re-run `archdots export`.
#
# Standalone installer: works on any Arch box; does NOT require archdots.

set -eu

REPO="$(cd "$(dirname "$0")" && pwd)"
DOTFILES="$REPO/dotfiles"
STAMP="$(date +%Y%m%d-%H%M%S)"

echo "==> Installing dependencies"
{IF pacman_deps non-empty}
sudo pacman -S --needed --noconfirm {pacman_deps}
{ENDIF}
{IF aur_deps non-empty}
echo "==> AUR dependencies — install manually with your AUR helper:"
echo "    {aur_deps}"
{ENDIF}

echo "==> Linking files (existing targets backed up with .bak.$STAMP suffix)"
find "$DOTFILES" -type f | while read -r src; do
    rel="${src#$DOTFILES/}"
    tgt="$HOME/$rel"
    mkdir -p "$(dirname "$tgt")"
    if [ -e "$tgt" ] || [ -L "$tgt" ]; then
        mv "$tgt" "$tgt.bak.$STAMP"
    fi
    ln -s "$src" "$tgt"
done

echo "==> Done. For atomic apply / rollback / dependency checks, install archdots:"
echo "    https://github.com/njuante/archdots"
```

The script is POSIX `sh` (not `bash`) so it runs on minimal Arch installs.
Optional deps and AUR are listed but not auto-installed — we don't ship an
auto-AUR-helper-picker (decision Q3). Existing targets are backed up with a
timestamp suffix; the user can `rm -rf ~/.config/old` after verifying.

---

## 7. Testing plan

Coverage target: ≥ 70 % on `crates/archdots-core/src/exporter/` (project-wide
bar).

### 7.1 Pure unit tests (no FS)

| File | What it covers |
|---|---|
| `exporter::scanner::tests` | Each rule has one positive test and one negative test. Crucially: a positive test for `generic-assignment` with a 16-char alphanumeric value, a negative test where the value is shorter than 16 chars, and a negative test where the line is commented out. |
| `exporter::template::tests` | Substitution, `IF` truthiness with empty vs non-empty vectors, `FOR` over items, escape behaviour (no Markdown injection in user-controlled fields like `profile.description`). |
| `exporter::glob::tests` | `*` / `?` / `**` matching against unix paths. Cases: `*.toml`, `.config/**/*.conf`, `.ssh/`, exact matches. |

### 7.2 Plan-level integration (tempdir, no scan)

| Scenario | Assertion |
|---|---|
| Profile with one file under `~/.config/foo/bar` | `plan.items[0].rel_in_repo == Some("dotfiles/.config/foo/bar")`. |
| Profile with `~/.ssh/id_ed25519` (direct target) | `ItemClassification::ExcludeSensitivePath { kind: Prefix, .. }`. |
| Profile with `~/foo` whose source is a symlink to `~/.ssh/id_ed25519` | `ExcludeSensitivePath` matched against the canonicalized source. |
| Profile with a 2 MiB source, default `max_bytes` | `ExcludeBySize`. |
| Profile with a binary source | `ExcludeBinary` after sniff. |
| Profile with a target outside `$HOME` (set up via temp `$HOME`) | `OutsideHome`. |
| Profile with a broken symlink, `required = true` | `Err(ExportError::Io)` from `plan` (the contract: required-missing surfaces). |
| Profile with a broken symlink, `required = false` | `ExcludeBrokenSymlink`. |
| Profile entry whose source is a live symlink | `source_canonical` populated; classification is `Include` (assuming nothing else trips). |

### 7.3 Scan-level integration

| Scenario | Assertion |
|---|---|
| Source contains an AWS key on line 3, column 12 | `findings[0]` matches `aws-access-key-id` with `line == 3, column == 12`, preview `"AKI...XYZ (20 chars)"`. |
| Source contains `-----BEGIN OPENSSH PRIVATE KEY-----` | `private-key-header` hit, severity `High`. |
| Source contains a JWT in a comment | `jwt` hit, severity `Medium`. `is_safe_to_write` returns true. |
| Source is a symlink whose target contains an AWS key | `findings` populated by reading the canonicalized target. |
| `--allow-secret aws-access-key-id` on a profile with an AWS key | After applying the allowance, `is_safe_to_write` returns true. |
| `--allow-secret aws-access-key-id:.config/aws-mock.toml` on a file at a different path | Allowance does not match; `is_safe_to_write` returns false. |

### 7.4 Write-level integration (tempdir destination)

| Scenario | Assertion |
|---|---|
| Happy-path full export | `output_dir/README.md` exists; `dotfiles/` has the expected file tree; `archdots-profile.toml` parses back into a `Profile` whose `files[i].source == PathBuf::from("dotfiles/<rel>")`. |
| `--format profile-only` | Only `README.md` and `archdots-profile.toml` exist; `dotfiles/` not created; `install.sh` not created. |
| Existing non-empty output dir without `--force` | `ExportError::OutputNotEmpty`. |
| Existing non-empty output dir with `--force` | Files overwritten; unrelated existing files preserved; transitions are atomic (we assert by checking no `.archdots-export.tmp.*` leftover). |
| Filesystem error mid-write (simulated via permission-denied on staging) | Staging dir removed; output dir unchanged. |
| Symlink source: copy in `dotfiles/<rel>` is a regular file containing the canonicalized target's bytes (not a symlink). | `is_symlink == false` after copy. |

### 7.5 CLI integration (`crates/archdots/tests/cli_phase5.rs`)

- `archdots export --help` matches a stable snapshot of the help text.
- `archdots export <missing-profile>` exits 3 with non-empty stderr.
- `archdots export <profile> --check` on a clean profile prints a report and
  exits 0.
- `archdots export <profile> --check` on a profile with an AWS key prints
  findings and exits 2.
- `archdots export <profile> --include-secrets` with non-TTY stdin exits 3.
- `archdots export <profile> --json --check` parses as JSON with
  `schema_version: 1` and `classification` values that are objects only (see
  §8).
- `archdots export <profile> --format profile-only --include-secrets` exits 3
  (invalid combo).

### 7.6 Snapshot test for the rendered README

One golden-file test that renders a fixed profile + plan and diffs the
output against `tests/fixtures/expected_readme.md`. Hand-rolled `assert_eq!`
against a `const EXPECTED: &str = ...` to keep dev-deps minimal (no `insta`).

### 7.7 What we do not test

- Network behaviour: we make no network calls.
- Real `git init` / `gh repo create`: out of scope.
- Real `pacman` interactions in export: export does not use pacman.
- Visual rendering of the Markdown.

---

## 8. JSON output contract

`archdots export --json` emits a stable shape (`schema_version: 1`, same
versioning policy as `check --json`). **Every enum variant in the output
serialises as an object — never as a bare string.** This is a contract from
v0.5.0: downstream tooling can match on a single shape for every variant of
`classification`.

```jsonc
{
  "schema_version": 1,
  "profile": "hyprland-rice",
  "output_dir": "/home/u/hyprland-rice-export",
  "format": "full",
  "wrote": true,                                 // false in --check mode
  "items": [
    {
      "entry_id": "hypr-conf",
      "source": "/home/u/.config/hypr/hyprland.conf",
      "source_canonical": "/home/u/.config/hypr/hyprland.conf",
      "target": "/home/u/.config/hypr/hyprland.conf",
      "rel_in_repo": "dotfiles/.config/hypr/hyprland.conf",
      "classification": { "include": {} },
      "size_bytes": 1842,
      "is_text": true,
      "findings": []
    },
    {
      "entry_id": "zshrc",
      "source": "/home/u/.zshrc",
      "source_canonical": "/home/u/dotfiles/.zshrc",
      "target": "/home/u/.zshrc",
      "rel_in_repo": null,
      "classification": {
        "exclude_sensitive_path": { "rule_id": "shell-history", "kind": "exact" }
      },
      "size_bytes": 4096,
      "is_text": true,
      "findings": [
        {
          "rule_id": "aws-access-key-id",
          "severity": "high",
          "line": 42,
          "column": 15,
          "preview": "AKI...XYZ (20 chars)"
        }
      ]
    },
    {
      "entry_id": "big-blob",
      "source": "/home/u/.local/share/foo/cache.bin",
      "source_canonical": "/home/u/.local/share/foo/cache.bin",
      "target": "/home/u/.local/share/foo/cache.bin",
      "rel_in_repo": null,
      "classification": {
        "exclude_by_size": { "size_bytes": 2097152, "limit_bytes": 1048576 }
      },
      "size_bytes": 2097152,
      "is_text": null,
      "findings": []
    },
    {
      "entry_id": "font",
      "source": "/home/u/.local/share/fonts/foo.ttf",
      "source_canonical": "/home/u/.local/share/fonts/foo.ttf",
      "target": "/home/u/.local/share/fonts/foo.ttf",
      "rel_in_repo": null,
      "classification": { "exclude_binary": {} },
      "size_bytes": 153600,
      "is_text": false,
      "findings": []
    }
  ],
  "summary": {
    "items_included": 14,
    "items_excluded_by_path": 1,
    "items_excluded_by_size": 1,
    "items_excluded_binary": 1,
    "items_missing": 0,
    "findings_high": 1,
    "findings_medium": 0,
    "findings_overridden": 0,
    "bytes_written": 0
  },
  "exit_code": 2
}
```

### Stability rules

1. **Versioning is per-output, not per-crate.** `schema_version: 1` versions
   the JSON shape independently of the crate's semver.
2. **Additive changes are non-breaking.** Adding a new optional field, a new
   variant under an `#[non_exhaustive]` enum (tagged appropriately), or
   extending a vec stays on `schema_version: 1`. Downstream consumers MUST
   ignore unknown keys and unknown variants.
3. **Breaking changes bump the version.** Renaming a field, removing a field,
   changing a type, changing the tagging strategy, or changing the semantics
   of an existing variant all bump `schema_version` (to `2`) and require a
   dedicated CHANGELOG entry.
4. **All-object tagging is part of the contract.** `classification` values
   are always objects with a single key. Other simple enums in this output
   (`format`, `severity`, `kind`) are scalar (`"full"`, `"high"`, `"exact"`)
   because they have no parameterised variants and their shape will not need
   to change. If a future variant of `severity` or `kind` needs parameters,
   the addition becomes a breaking change (bump `schema_version`) precisely
   because the shape would have to change.

---

## 9. Out of scope for v0.5

- Initialising a git repo / pushing to GitHub / creating a release.
- Tar / zip bundle output (`--bundle`).
- Snapshot-id-based export (`--include-snapshot <ID>`).
- Embedding screenshots automatically.
- A user-overridable README template (`--readme <PATH>`).
- A user-overridable `install.sh` template.
- Multi-profile export (`archdots export profile1 profile2 ...` into one
  repo).
- An "import" subcommand. The `cp` + `archdots apply` recipe in the README
  is sufficient.
- Recursive directory inclusion (`FileEntry.source` being a directory).
- Network calls of any kind.
- TUI surface for export (see decision F).
- Differential export ("only files that changed since last export").
- Entropy-based secret detection / binary scanning.
- Sanitising secrets in-place (writing redacted copies).
- Customisable secret-pattern rules outside the embedded set.
- Customisable sensitive-path rules outside the embedded set.
- Multi-distro `install.sh`. The script is Arch-only by design.

---

## 10. Confirmed design decisions (from review)

These items were called out during the review pass and are recorded here so
the rationale is co-located with the design.

- **Q1 — `--output` default**: `./<profile>-export/`. The `-export` suffix
  makes the staging nature obvious; the user is expected to rename if they
  want a different repo name. *Confirmed.*
- **Q2 — `--readme` configurable**: not in v0.5. A future `--readme <path>`
  override is left as a non-breaking addition; the template renderer is
  private behind `Exporter::render_readme`. *Confirmed.*
- **Q3 — `install.sh` and AUR**: `install.sh` never auto-installs AUR; it
  lists the AUR packages and tells the user to use their helper. *Confirmed.*
- **Q4 — JSON tagging**: `classification` values are **always objects**
  (`{"include": {}}`, `{"exclude_sensitive_path": {...}}`). All
  `ItemClassification` variants are declared as struct-form (including the
  empty `{}` variants) so serde external tagging emits a consistent shape.
  No bare strings. *Adjusted from draft.*
- **Q5 — `generic-assignment` rule**: ships **enabled**, severity
  `medium`. It is the highest-FP rule, but disabled-by-default would be
  surprising and would mean shipping a scanner that misses the most common
  paste-into-zshrc pattern. Severity `medium` means it does not block; it
  warns in the report. *Confirmed.*
- **Q6 — `--allow-binary` granularity**: per-path glob, repeatable
  (`--allow-binary <GLOB>`), consistent with `--allow-path` and
  `--allow-secret`. The global boolean is gone. All three flags share the
  same hand-rolled glob matcher under `exporter::glob`. *Adjusted from
  draft.*
