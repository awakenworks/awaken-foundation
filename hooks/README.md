# Migration hooks

Portable commit-time checks for repositories that use `awaken-scoped-migration`.
Drop [`check_migrations.py`](check_migrations.py) **anywhere** in a consuming repo
and wire it into a git hook; it has no dependencies beyond Python 3 and `git`,
**auto-discovers** the repo's migration files (no configuration), and is
self-tested (`--self-test`).

See the [migration design doc](../docs/design/scoped-migration.md#best-practices-every-one-is-hook-verified)
for the practices these checks enforce and why.

## What it checks

Every migration best practice that is mechanically checkable from the source:

1. **Immutability (append-only)** — never edit/delete a shipped migration line.
2. **Destructive-op guard** — `DROP` / `TRUNCATE` / `DELETE`-without-`WHERE` needs a marker.
3. **Deterministic statements** — no `IF NOT EXISTS` / `IF EXISTS` / `CREATE OR REPLACE` (the ledger already guarantees exactly-once; conditional DDL masks drift).
4. **Portable SQL** — no raw `JSONB`/`TIMESTAMPTZ`/`SERIAL`/`now()`; use `{tokens}`.
5. **Readable, unique versions** — version labels are `V0001`-form, no duplicates.

(Version *ordering* across the persisted ledger and *cross-scope* reference need
the bundle types; those are enforced by the crate's `MigrationBundle::new` via
`cargo test` in pre-push — see the design doc's hook table.)

## Auto-discovery

A file is treated as a migration file if it **constructs** migrations
(`MigrationBundle::new`, `bundle_from_statements`, or `Migration::new(` in a `.rs`,
or a `.sql` on a migration path) and is **not** the crate's own implementation (a
file *defining* `MigrationBundle` / `plan` / a `MigrationRunner` is skipped). No
config is required; a root `.migration-paths` file (globs, one per line) can *add*
paths if your layout is unusual.

## Modes

- `--audit` — discover and check every migration file in the tree (CI / first adoption).
- `--staged` — check the migration files in the index against HEAD (pre-commit).
- `--base REF` — check the migration files in `REF...HEAD` (pre-push / CI).
- `--self-test` — validate the checker itself.

## Install

1. Copy `check_migrations.py` into the repo (e.g. `scripts/ci/`).
2. Wire it into the git-hook runner. With lefthook:

   ```yaml
   pre-commit:
     commands:
       migrations:
         run: python3 scripts/ci/check_migrations.py --self-test && python3 scripts/ci/check_migrations.py --staged
   pre-push:
     commands:
       migrations:
         run: python3 scripts/ci/check_migrations.py --audit
   ```

## Override markers

Put a marker in the diff (a comment on an added line) to allow an intentional,
reviewable exception:

- `migration-allow-edit` — modify/delete an existing migration line.
- `migration-allow-destructive` — an added destructive statement.
- `migration-allow-raw-sql` — an added raw dialect type (escape hatch).
- `migration-allow-conditional` — an added conditional/non-deterministic statement.
