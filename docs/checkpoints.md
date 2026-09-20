# Local Git checkpoints

GitHub publishing is currently blocked by integration permissions. Development
continues locally on `work/plumb-local`; checkpoint ZIPs are the deliverable until
publishing is available. A checkpoint is a milestone snapshot, not a production
release or evidence that hosted TIN has been tested.

Each checkpoint contains:

- `plumb/`: the committed source tree, LICENSE, NOTICE, docs and tests;
- `plumb/.git/`: the complete local repository, including all cloned upstream
  history, local commits, branches and checkpoint tags;
- `checkpoint.json`: the revision, tag, history count and validation summary.

Build outputs, PostgreSQL data directories, toolchains and dependency caches are
not part of the ZIP. Dependencies are resolved from the included Cargo.lock when
building. This is not an offline build kit. Local implementation commits are
attributed to Hyperagent, not impersonated as a maintainer's commits.

## Open the checkpoint as a repository

Extract into a **new directory**, preserving hidden files and permissions. Do not
extract over an existing checkout or replace its `.git` directory.

```sh
unzip plumb-checkpoint-001-foundation.zip -d checkpoint-001
cd checkpoint-001/plumb
# .git is included even if your file browser hides dotfiles.
git status
git log --oneline --decorate -8
git fsck --full
```

The Git history is non-shallow. Checkpoint archives are validated after extraction
for Git integrity and a clean working tree. The `origin` remote remains the public
fork URL; no authentication credentials are included or needed to inspect history.

## When extraction appears to create uncommitted files

Some ZIP extractors preserve bytes but drop Unix executable bits. An audit of
checkpoints 001–004 found every tracked file byte-identical to its committed blob;
only these executable files appeared modified after permission-dropping extraction:

- script/materialize-private-regress
- script/run-private-regress
- tests/merge_concurrency.py (checkpoint 004)

First inspect, without changing anything:

```sh
git diff --summary
git diff --numstat
git -c core.filemode=false status --short
```

`100755 => 100644` with `0 0` in numstat is a mode-only change. On Unix, restore only
those known executable flags (this does not change content):

```sh
chmod +x script/materialize-private-regress script/run-private-regress
# Checkpoint 004 and later, if present:
chmod +x tests/merge_concurrency.py
```

New checkpoints include a conservative helper:

```sh
python3 script/check-checkpoint-permissions.py        # report only
python3 script/check-checkpoint-permissions.py --fix  # only byte-identical files
```

The helper never resets contents, stages files, follows symlinks or changes Git
configuration. Modified/missing files are reported and left untouched. A filesystem
that cannot preserve Unix executable bits may still report mode differences; the
command-scoped core.filemode diagnostic above checks this without changing global
or repository settings. Do not use `git reset --hard` to hide unexplained changes.
If actual content differs, inspect and retain it before any restoration.

## Bring a checkpoint into an existing fork

Start with a clean working tree and inspect the differences before merging. Use
a unique local remote name; do not overwrite an existing remote configuration.
Replace the path below with the **extracted repository** on your machine:

```sh
git remote add plumb-checkpoint-001 /absolute/path/to/checkpoint-001/plumb
git fetch plumb-checkpoint-001 work/plumb-local
git log --oneline HEAD..FETCH_HEAD
git diff HEAD...FETCH_HEAD
# If the reviewed changes are the ones you want:
git merge --no-ff FETCH_HEAD
```

This retains the existing repository's history and configuration. Resolve any
conflicts deliberately; do not force-reset over your own changes. There is no need
to copy `.git` into an existing repository. Later checkpoints contain earlier
checkpoint commits, so Git can recognize which changes were already applied.

Checkpoint tags use `checkpoint/001-foundation`, `checkpoint/002-postings-codec`,
and subsequent numbered names. A new ZIP is created at each completed, validated
milestone; prior downloads are retained rather than replaced.
