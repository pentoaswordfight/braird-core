# Snapshot export and merge-import contract

Braird snapshots are portable, PWA-compatible JSON backups of the eight synced stores. The Rust
API surface is:

```text
SyncEngine::export_snapshot() -> Result<String, SyncError>
SyncEngine::import_merge(json: String) -> Result<ImportSummary, SyncError>
```

There is deliberately no Replace API. Import is a protective merge against both local and current
server state; it never clears the account before applying a backup.

## Plaintext security boundary

`export_snapshot` returns plaintext note text, and `import_merge` accepts that plaintext again.
The snapshot string is outside the Vault's ciphertext-at-rest boundary. Hosts must treat it as
highly sensitive user data:

- Never write snapshot JSON to logs, analytics or telemetry, breadcrumbs, crash reports, console
  output, pasteboards, or unprotected shared storage.
- Create the temporary file on the destination filesystem, in the destination directory when
  possible. A temporary file on another volume cannot be atomically renamed into place.
- Apply restrictive permissions and platform file protection before writing any bytes. On Apple
  platforms use an app-private file with complete file protection; on Android use app-private
  internal storage with private permissions and the strongest available device-backed protection.
- Write, flush, and close the file, then reopen it from the destination filesystem and verify the
  complete byte count plus the `_syntopicon: true` / `schemaVersion: 19` envelope before atomically
  renaming it to the final path. Never expose a partially written backup at the final path.
- Remove the temporary file on success, failure, cancellation, or process recovery. Do not retain
  plaintext backup copies after import unless the user explicitly chose durable backup storage.
- Keep plaintext buffers only as long as needed to call the API, then release them. Do not include
  archive content in error UI or diagnostics.

The core fails a whole export if any note cannot be decrypted. It never returns a partial archive
or substitutes ciphertext for plaintext.

## Schema 19 export wire shape

The exporter emits the top-level keys in this exact order. All eight store names are camelCase:

```json
{
  "_syntopicon": true,
  "schemaVersion": 19,
  "exportedAt": "2026-07-15T12:34:56.789Z",
  "books": [],
  "notes": [],
  "customIdeas": [],
  "noteLinks": [],
  "lenses": [],
  "collections": [],
  "collectionMemberships": [],
  "noteSignals": []
}
```

`exportedAt` is an ISO-8601 UTC timestamp. Every array contains live rows only; soft-deleted rows
are excluded, and exported live rows carry the PWA-compatible `deleted: 0` shape.

Note `text` is plaintext. For handwritten-annotation edges, the exporter also reconstructs
`user_metadata.user_annotation` on the parent note from child-note plaintext, ordered by edge
`createdAt` and then edge id. The authoritative synced `noteLinks` rows remain in `noteLinks`.

Synced `imagePath` and `inkCropPath` values are retained, but device-local preview payloads
`imageDataUrl` and `inkCropDataUrl` are never exported. No local-only table is exported: `meta`,
`outbox`, `embeddings`, and `discovery_jobs` are excluded.

## Accepted imports

Imports accept snapshot schema versions 1 through 19. A missing or null `schemaVersion` is the
legacy schema-1 form; when present it must be an integer in the inclusive range 1–19.

Validation happens before token checks, network access, crypto work, or store writes:

- the root must be a JSON object and `_syntopicon` must be the literal boolean `true`;
- each known store must be an array when present (`null` or absent is treated as empty);
- each row must be an object with its required non-empty string primary key (`noteId` for
  `noteSignals`, `id` for the other stores);
- primary keys must be unique within each archive store;
- known fields must have their documented JSON types, including integer timestamps/counts and
  finite signal numbers; and
- snapshot rows must identify live data: `deleted` may be absent, null, `false`, or numeric zero,
  but tombstones and other truthy or malformed values are rejected.

Unknown and device-local row fields are ignored. Imported `contentTag`, `imageDataUrl`,
`inkCropDataUrl`, and `user_metadata` values are not trusted or persisted. Invalid input returns
`SyncError::InvalidImport` with a sanitized reason and no archive record content.

## Protective merge algorithm

A syntactically valid archive requires an access token and connectivity. Import proceeds as one
protective operation:

1. Pull incremental changes for all eight synced tables. Every table must complete cleanly. A
   partial or total pull failure aborts snapshot staging; remote rows and outbox rebases already
   applied by the pull may remain.
2. Directly fetch every archive candidate key from its server table, including server tombstones.
   This closes the incremental-cursor race and supplies the current server side of the comparison.
   Any failed fetch or malformed, duplicate, or unrequested returned row aborts before snapshot
   writes.
3. Compare each normalized archive `updatedAt` with both the local row and the directly fetched
   server row. The archive wins only when its timestamp is strictly greater than both. An equal
   timestamp keeps the existing row. A newer or equal tombstone therefore blocks resurrection; an
   older tombstone may be superseded.
4. Choose one checked timestamp for the accepted batch:
   `max(import_now, newest timestamp compared for accepted rows + 1)`. Every accepted row receives
   that same `updated_at` and `deleted: false`. Arithmetic overflow fails before import writes.
5. For each accepted note with string text, derive a fresh `content_tag` with the active Vault and
   effective nullable book id, then encrypt the plaintext as fresh `enc:v2` bound to the note id.
   An imported tag is discarded. A supported legacy note with omitted text remains null and is not
   given an invented tag or plaintext value.
6. Complete each accepted row to its known table descriptor, then stage the entire dependency-
   ordered batch in one SQLite transaction: remove a matching pending tombstone, replace the local
   full row, and enqueue the identical full ciphertext-safe row. Any store or outbox error rolls
   back all snapshot rows and snapshot outbox entries.

Import does not auto-flush. A later normal `sync()` uploads the staged outbox batch.

## Import result

`ImportSummary` reports the normalized archive `schemaVersion` plus per-store counts in
`imported` and `skippedStale`. On a successful call every archive candidate contributes to exactly
one of those counts. Counts use the same eight stores and dependency order as the archive:
`books`, `notes`, `customIdeas`, `noteLinks`, `lenses`, `collections`,
`collectionMemberships`, and `noteSignals`.
