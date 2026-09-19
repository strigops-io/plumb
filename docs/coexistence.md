# Plumb SQL identity and coexistence contract

Status: primary identity implemented; optional TIN-name compatibility **deferred**.
Plumb remains a proof of concept. Identity isolation does not establish production
readiness, persistent-index correctness, or complete hosted TIN compatibility.

## One database, two independent providers

| Object | Plumb (this fork) | Existing Lead/TIN provider |
| --- | --- | --- |
| Extension | plumb | tin |
| Control file / shared library | plumb.control / plumb | tin.control / provider library |
| Version | 0.1.0 | Provider-owned |
| Schema / functions | plumb, plumb.score, plumb.full_score, etc. | tin, tin.score, tin.full_score, etc. |
| Index access method | plumb | tin |
| Default text opclass | plumb.plumb_text_ops | Provider-owned (Lead: tin.tin_text_ops) |
| Canonical search operator | pg_catalog.~~>(text,text) | pg_catalog.==>(text,text) |
| Operator implementation | plumb.plumb_text_cmpfunc(text,text) | Provider-owned |

The operator is intentionally installed in `pg_catalog`, as a **distinct** signature,
so ordinary `body ~~> 'term'` works with the usual PostgreSQL search path without
adding `plumb` to it. Functions are explicitly schema-qualified. The fork ships no
`tin.control`, no `tin` schema/AM, no `==>` alias and no objects in the existing
provider's schema. It never adopts or replaces another extension's members.

Installation is superuser-only, non-relocatable, in schema `plumb`. Conflicting
operator/access-method/function identities make installation fail transactionally;
there is no `IF NOT EXISTS` shortcut that silently adopts an unrelated object.
The normal `CREATE EXTENSION` conflict checks also protect preexisting extensions.
Operators/AMs created by Plumb are Plumb extension members despite the operator's
catalog schema. Do not manually move or replace them.

## Explicit selection, not search-path switching

```sql
-- Install the independently packaged extensions, in either order.
CREATE EXTENSION plumb;
CREATE EXTENSION tin; -- existing public Lead/TIN provider, NOT a Plumb alias

CREATE TABLE documents (id bigint PRIMARY KEY, body text);
CREATE INDEX documents_plumb_idx ON documents USING plumb (body);
CREATE INDEX documents_tin_idx ON documents USING tin (body);

-- Plumb: its own index and scoring support.
SELECT id, plumb.full_score(ctid)
FROM documents WHERE body ~~> 'craft beer';

-- Existing provider: unchanged names and index.
SELECT id, tin.full_score(ctid)
FROM documents WHERE body ==> 'craft beer';
```

Both indexes may exist on the same column. Distinct operator families let the
planner route each predicate to the corresponding provider's index. A heap/seq
scan remains possible when PostgreSQL considers it cheaper; neither spelling
forces an index scan.

Unqualified operators follow PostgreSQL's normal lookup rules. If an application
explicitly puts an untrusted schema before `pg_catalog`, another `~~>` can shadow
the spelling. Use a safe search path or the fully qualified operator:

```sql
SELECT id, plumb.highlight(body)
FROM documents
WHERE body OPERATOR(pg_catalog.~~>) 'craft beer';
```

Do not solve coexistence by changing `search_path` to select which vendor owns
`==>`, replacing `tin.*`, or teaching Plumb's planner support to recognize operators
by spelling alone.

## Planner and lifecycle isolation

- Plumb score/highlight support resolves exactly `pg_catalog.~~>(text,text)` to an
  OID for each binding. It compares that OID, not `get_opname`, and does not cache
  OIDs across catalog changes. A same-spelling operator in another schema or with
  another argument signature is not a Plumb predicate.
- Index lookup and `plumb.score_inspect` require the `plumb` AM. Having a `tin`
  index on the same expression is not enough to bind Plumb's implicit scorer.
- Internal scorer resolution uses `plumb.score_bound`, never an unqualified
  function name. Support-function OIDs are provider-specific too.
- Implicit Plumb scoring/highlighting without a matching canonical predicate and
  eligible Plumb index fails rather than borrowing a foreign provider's context.
  Explicit-query highlighting is independent of implicit binding, as upstream.
- Dropping one extension must not drop the other's objects. PostgreSQL correctly
  refuses a plain drop if dependent user indexes exist; `CASCADE` can destroy that
  provider's dependent indexes and user objects. It is not a migration procedure.
- User-created views/functions depending on both providers legitimately depend on
  both. Extension isolation cannot prevent their removal under an explicit CASCADE.
- Do not mutate an existing tin index into a plumb index. Build a separate index,
  verify results/plans and change application SQL deliberately. Physical index
  formats and extension upgrade histories are independent.

## Optional TIN compatibility: later phase

The latest priority is native Plumb coexistence, not claiming the `tin` identity.
No Plumb implementation of `CREATE EXTENSION tin` is shipped now.

A future adapter may offer `tin.*`, `USING tin` and `==>` for applications that
cannot change their SQL. It must be an explicit opt-in, with its own tests and
compatibility evidence. The following constraints are non-negotiable:

1. **Mutually exclusive ownership.** Real Lead/TIN and a Plumb adapter cannot both
   own the database-wide extension/AM name `tin`, schema `tin` or identical `==>`
   signature. Native Plumb can coexist with either; the two tin providers cannot.
2. **Separate distribution.** Default Plumb packages never install or overwrite
   tin.control, tin SQL files or another provider's shared library. A distinct
   compatibility extension name such as `plumb_tin_compat` is preferable for package
   isolation, even if it exposes tin SQL names. If literal `CREATE EXTENSION tin`
   is later required, use a mutually exclusive compatibility package with package-
   manager conflicts and explicit host-level preflight. Database checks alone
   cannot prevent overwriting shared server files used by other databases.
3. **Fail closed.** Before any alias creation, refuse existing tin extension,
   schema, AM or operator conflicts. Never `CREATE OR REPLACE` over real provider
   objects, attach them to the adapter, or remove them automatically.
4. **Explicit dependency.** The adapter depends on native Plumb and owns only its
   aliases. Native Plumb never depends on it. Removing the adapter must leave
   native Plumb intact. Index dependencies still follow PostgreSQL's normal rules.
5. **Not just SQL wrappers.** Scoring/highlight planner support and AM/opclass
   identity need dedicated adapter design. Do not weaken exact-OID binding to
   match every `==>` by name. Only explicitly owned, registered aliases may bind.
6. **No hidden migration.** Provide documented export/rebuild/cutover steps and
   compatible-query tests. Installing aliases must not imply physical index
   compatibility, an in-place upgrade or complete TIN equivalence.

## Evidence and limits

On PostgreSQL 17.10, native Plumb's 41 tests pass, including five new identity and
operator-shadow regressions. [tests/coexistence.sql](../tests/coexistence.sql) passed
54 checks against the unchanged **public Lead** extension: extension membership,
unchanged Lead procedures, same-table index routing, mixed scores/highlights,
foreign-provider rejection, hostile search-path shadows, both installation orders
and both drop orders. All test DDL was rolled back in a fresh disposable database.

This does not establish coexistence with private/hosted TIN. Its actual available
extension privileges, SQL objects and provider behavior must be verified through
authorized public interfaces. PostgreSQL 18 and ARM remain separate CI validation
targets. See [tests/README.md](../tests/README.md) for guarded reproduction commands.
