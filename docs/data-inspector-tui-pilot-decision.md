# MySQL Data-Inspector TUI Pilot Decision (Requirement 83)

- **Plugin / profile type**: `voidb-plugin-mysql` / MySQL and MariaDB TCP
  profiles (`mysql://`, username with password or brokered `credential_refs`,
  optional default database, `ssl_mode`).
- **Selected state**: `Accepted pilot`.
- **Command**: `voidb-plugin-mysql tui` (plugin-owned, CLI-launched), also
  routed as `mysql tui` through `voidb-cli`.
- **Scope**: schema-aware data triage. This is new plugin-owned UI work built
  on the existing `MySqlService` channel contract; it adds no backend
  operations and does not depend on removed router-hosted factories, shared
  legacy data widgets, or shell-owned plugin UI state.
- **Gate precedent**: this record follows the accepted Redis data-inspector
  TUI pilot (`voidb-plugin-redis/docs/data-inspector-tui-pilot-decision.md`),
  which established that a bounded plugin-owned pilot with fixture, target
  error, and real PTY evidence produced in the same requirement satisfies the
  Requirement 83 gate. MySQL previously sat at `CLI-only` in
  `voidb/docs/data-inspector-cli-first-decisions.md` because "the workflow is
  not plugin-unique enough to justify a standalone TUI before shared
  table-page and mutation-preview capabilities exist". The bounded read path
  now exists in the plugin service layer (schema browse plus paged data
  loading), so that precondition is met and the pilot is implemented,
  validated, and accepted here.

## Named human workflow

**Schema-aware data triage across tables.** An operator investigates a data
quality or incident report on a live server:

1. **Start**: the operator connects through an existing profile
   (`mysql tui --profile prod-app --purpose triage`). The TUI opens read-only
   on the databases view; nothing is loaded beyond the database list.
2. **Browse**: the operator presses `Enter` on a database (or the profile's
   default database is preselected) and lands in the tables browser with
   name/type/approximate row count/comment columns, so oversized tables are
   visible before any of them is opened.
3. **Page**: the operator presses `i` on a table and the full-screen table
   data viewer opens on the first bounded page (`LIMIT 5 OFFSET 0` through
   the service's paged loader). `n` fetches the next page, `b` walks back;
   pages are **replaced**, not appended, so the viewport stays bounded. `/`
   filters the current page client-side with a live match count.
4. **Inspect**: the operator presses `Enter` on a row and a full-screen row
   viewer renders every column with its declared type, NULL/NOT NULL and PK
   flags, and the value (NULL rendered literally, long text scrollable inside
   a bounded line buffer).
5. **Act**: for a row that must go (a poisoned session, a test artifact), the
   operator presses `d`, **types the exact table name**, and presses `y`; the
   delete is a single PK-anchored row delete through the service's existing
   save-changes path, and the current page is refetched so the row
   disappears. A failed delete (lock timeout, permissions) shows a redacted
   target error in a non-modal banner and the operator keeps working.
6. **Success state**: without leaving the TUI, the operator has walked
   database → table → page → row and removed one identified row, with the
   refreshed page as confirmation. Row edits and inserts are **not** part of
   this slice (explicitly `later candidate` below).

**Why the CLI is materially worse here**: the same loop requires
`mysql databases`, then `mysql tables <db>`, then one
`mysql query -d <db> "SELECT * FROM <table> LIMIT n OFFSET m"` invocation per
page, plus a separate `mysql query` with a hand-written `DELETE ... WHERE pk`
per candidate row. Every step pays a fresh process startup and none share
database, table, page, or cursor context; the operator retypes the table name
and re-derives the pagination offsets in shell history. The TUI keeps the
database → table → page state, per-view selection, per-page filter, and row
inspection in one viewport, so an operator walks a suspect table page by page
in one sitting. Generic "browse data" is not the claim; the claim is
cursor-preserving triage over repeated browse-page-inspect cycles across
tables of one server.

## Gate-by-gate evidence

| Gate | Evidence in this implementation |
|---|---|
| Named human workflow | Above; exercised end to end in fixture mode and by the PTY transcript (open `appdb` → open `users` → page → inspect row → typed delete of row `id=1`, refetch shows 11 rows and the row gone). |
| Capability-backed bounded reads | Reads are the existing service-layer operations only: `SchemaCommand::ListDatabases` (SHOW DATABASES), `SchemaCommand::ListTables { database }` (INFORMATION_SCHEMA.TABLES with row estimates), and `LoadCommand::LoadTableData { database, table, page_size, offset }` — `LIMIT page_size OFFSET offset` bound in `service/data_loader.rs`, with `LoadRowCount`/`LoadPrimaryKeys` piggybacked by the same command. The TUI adds no new backend operations; there is no free-form SQL surface in the pilot. |
| Pagination and large-result safety | One page (`PAGE_SIZE = 5` rows) is rendered at a time; `n` fetches `offset + PAGE_SIZE` only and pages are replaced, never appended, so the viewport stays bounded. The viewer title reports `rows X-Y of T page P`, so empty state (`empty_archive`: "No rows on this page…"), partial last page (`users`: 12 rows over 3 pages), and end-of-data state are all visible. Table row counts come from INFORMATION_SCHEMA estimates, never a table scan. Row-viewer bodies are bounded by `BoundedLines` at 2,000 lines / 2 MiB with dropped-line counters in the status panel. |
| Redaction and error shape | Preflight (`--format json`) reports auth as `"auth": "password"|"none"` plus `credential_ref_count` with `"secret_material": "redacted"` and no connection URL; `secret_leak_markers` scan covers `mysql_password_value`, `mysql_secret_data_value`, `plaintext_credential_value`, `raw_connection_url` and passes (`passed: true`, 0 markers) in `build_mysql_tui_evidence` output; errors render through `safe_error_summary` in the non-modal banner and keep the service-layer category text (`validation`/`auth`/`permission`/`transport`/`timeout`/`plugin`/`target` categories come from `mysql_error_to_capability_error` / `capability_error_to_legacy_message`). |
| Mutation safety | First viewport is read-only. `--readonly` launch stages plans but blocks at confirm with `readonly launch blocked delete row on <table>`. Row `DELETE` requires **typed confirmation of the exact table name** plus explicit `y` (two confirmations, `destructive` risk), is refused up front when the table has no primary key (`build_pk_where` cannot anchor a bounded delete), and sends exactly one PK-anchored `DELETE FROM <table> WHERE …` through the existing `CrudCommand::SaveChanges` path with an empty pending-edit set and a single deleted row index — there is no bulk, table-level, or unbounded delete anywhere in the pilot. Row `UPDATE` (SaveChanges cell edits), `InsertRow`, and all DDL (`BrowserCommand`) are **`later candidate`** and unreachable from the TUI. Mutation policy is recorded in `evidence["mutation_rescan_policy"]`. |
| Fixture and target-error evidence | `fixtures/mysql_tui_operations.json` covers 3 databases, typed columns with PK flags, 12-row paging over 5-row pages (full/partial/empty), NULL and long-text values, a no-primary-key table, an empty table, and a deliberately erroring table (`locked_table` → `target error: lock wait timeout exceeded …`). Startup carries a `target error: 1205 lock wait timeout …` banner; the PTY proves the TUI stays fully responsive across it (all assertions pass with the banner present on the first frame). |
| PTY lifecycle evidence | Real PTY run (`pty.fork()` + `TIOCSWINSZ` `HHHH` at 36x130, winsize set pre-exec): all assertions across consecutive runs. First frame markers (`MySQL Data TUI`, `1:databases`, `2:tables`, `fixture-mysql-ops`, fixture rows, hint footer, target-error banner); navigation into tables/data viewer with paging; typed-delete confirmation and refetch; resize to 24x100 keeps content visible and reports `resized mysql view to 100x24`; `q` exits 0 with terminal restore; raw-mode cleanup is delegated to `ratatui::restore()` after the loop (raw input is not used: `raw_input = false`). |
| No legacy dependency | The command lives in the plugin (`src/tui.rs`), is launched from the plugin CLI (`mysql tui`), consumes only `MySqlService` channel events, and contains no reference to removed router-hosted data-inspector adapters or shared legacy widgets. The agent-session/agent_live machinery is untouched. |

## Interaction surface

- Views and hotkeys: `1` databases browser, `2` tables browser (Tab cycles);
  pending schema/data fetches show the family `⟳` marker in the header tabs.
  The profile's default database, when set, is preselected on connect.
- Databases browser: database list with a details panel; `Enter` opens the
  tables browser for the selected database.
- Tables browser: name/type/rows/comment columns; `i`/`Enter` opens the
  full-screen table data viewer (`LIMIT/OFFSET` pages from the service data
  loader) with `j/k`/PgUp/PgDn row cursor, `n` next page, `b` back, and `/`
  client-side filter on the current page with a live match count.
- Row inspect: `Enter` on a row opens the row detail viewer (typed column
  rendering: name, type, NULL/PK flags, value) with scroll and a bounded line
  budget; long text and literal NULLs are visible.
- Mutation: `d` on a row (data viewer or row viewer) stages a typed delete
  (type the exact table name, then `y`); readonly launches stage but block at
  confirm; the current page is refetched after execution.
- Confirm-quit fires while a schema/data fetch or a staged plan is active;
  the error banner is non-modal (any key acknowledges while the key keeps its
  meaning).

## Deferred inside the accepted pilot

- **Server view**: the MySQL service channel contract exposes no bounded
  server-info/variables command (unlike Redis `FetchServerInfo`), and the
  pilot must not add backend operations or wire arbitrary SQL through
  `ExecuteQuery`; deferred as `later candidate` until the service gains a
  bounded diagnostics command.
- **Row edits and inserts** (`CrudCommand::SaveChanges` cell updates,
  `CrudCommand::InsertRow`): `later candidate` — value-editing UX needs its
  own fixture, confirmation, and redaction evidence before it can ship.
- **Table/database management** (`BrowserCommand` DDL: drop/rename/duplicate,
  database create/drop): `later candidate`; blast radius is beyond any slice 1
  mutation gate.
- **Server-side pattern narrowing** for database/table lists: `later
  candidate`; `ListDatabases`/`ListTables` expose no filter parameter.
- Until these ship, docs and release copy must keep pointing to the CLI
  (`mysql query/databases/tables/describe`, and
  `voidb-cli invoke run mysql.<capability> …`) as the supported general path
  for writes, arbitrary queries, and diagnostics; `plugin.toml` declares
  `[ui] tui = true, raw_input = false` for this bounded pilot only.

## Validation commands and evidence

All run in `voidb-plugin-mysql` with `VOIDB_DISABLE_KEYRING=1` (no OS keyring
touch):

```bash
VOIDB_DISABLE_KEYRING=1 cargo test            # pre-existing tests + TUI interaction tests
cargo clippy --all-targets -- -D warnings     # 0 warnings
cargo fmt --all && cargo fmt --all -- --check # clean (CI enforces this)
cargo run --example export_contract -- --check # manifest/contract drift green
```

TUI interaction tests (in `src/tui.rs#[cfg(test)]`): fixture evidence +
secret-leak scan, preflight redaction, non-modal startup banner, view hotkeys
and tab cycling, database selection, table paging with offset/total state,
filter match count, row viewer typed columns with NULL and long text,
typed-delete exact-table-name gate, delete-execute + page refetch, delete
refusal without a primary key, readonly block, erroring-action
responsiveness, empty table state, confirm-quit, page-key movement.

Preflight and evidence:

```bash
target/debug/voidb-plugin-mysql tui --fixture fixtures/mysql_tui_operations.json --format json
target/debug/voidb-plugin-mysql tui --fixture fixtures/mysql_tui_operations.json --evidence evidence.json
```

Evidence JSON: `kind: mysql_tui_fixture_evidence`, quality gate
`retained_tui_quality_gate("mysql", ["fixture-mysql-ops", "fixture-mysql-local"], ["target error"], 80, 24, 10_000)`,
coverage entries including the new interactions
(`database_selection`, `table_data_paging`, `row_viewer_typed_columns`,
`typed_delete_confirmation`, `delete_requires_primary_key`, `mutation_refetch`,
…), `secret_leak_scan.passed = true`.

PTY lifecycle record (macOS, Python 3.9 stdlib only, `struct "HHHH"`, 36 rows
x 130 cols): assertions across consecutive clean runs, exit code 0 each run;
script retained out-of-repo at `/tmp/mysql_tui_pty_smoke.py` (run
`python3 -I /tmp/mysql_tui_pty_smoke.py`). Covered: first frame
(non-blank, marker-checked), banner acknowledgement, database → tables →
data viewer navigation, paging forward/back, filter, row viewer, typed-delete
confirmation and refetch, live resize with content retention, normal `q` quit
with clean exit 0 and terminal restore. Local Python is 3.9 (no tomllib), so
the python provenance tests cannot run on this machine (CI runs 3.11+); this
does not affect the PTY evidence, which uses only stdlib.

## Migration note

This pilot is new plugin-owned UI work. It does not restore, wrap, or depend
on the router-hosted MySQL data-inspector adapters removed in Requirement 77,
and it does not reintroduce shared legacy data widgets or shell-owned plugin
UI state. The default TUI guidance for MySQL profiles remains the
CLI/capability path; `mysql tui` is a bounded, profile-scoped triage pilot
whose only supported mutation is a single PK-anchored row delete. Release
docs must not advertise the deferred row-edit, insert, DDL, or server-view
workflows as available in the TUI.
