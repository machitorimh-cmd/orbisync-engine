# Explicit checkpoint reconciliation

Generation writes remain disabled by default. This command does not start a
listener, run migrations, infer history, or discard either legacy source.
Supported PostgreSQL, writer exclusion, resource and release acceptance are
separate prerequisites; the nonDB fixture tests do not certify them.

Run only on the designated deployment host after the supervisor has confirmed
the old process exited and old writers were excluded. Configure the existing
`world.checkpoint_writer_lock` and `world.checkpoint_deployment`. The tool takes
the same host and database ownership locks as the server; it cannot coexist with
a live writer. Keep `world.checkpoint_generation_enabled = false`.

The runtime database URL comes from the configured `database.url_env`. Apply also
requires `ORBISYNC_OPERATOR_DATABASE_URL` with the separately provisioned operator
credential. Supply credentials through environment or the deployment's secret-file
loader, never command arguments or review/report files. Errors redact driver data.

1. Run `orbisync-server checkpoint-operator inspect --instance <UUID> --output inspection.json --old-writer-exit-confirmed`.
   The new file preserves both sources, observations and their fixed selection
   identity. Existing output files are refused.
2. Copy the inspection to `review.json`. Preserve `selection` and `instance`.
   Supply `chosen_checkpoint` as a complete legacy-format checkpoint object,
   including the explicitly chosen entities and original receipts. Supply a
   nonempty `evidence` report and an explicit `decision`:
   - `trusted_history` requires `history_complete: true` and reviewed evidence
     for the chosen values, deletes and original receipt history.
   - `new_empty` requires `never_admitted: true` and reviewed creation proof.
     Both sources, chosen entities and receipts must be empty at initial revision.
   - `baseline` requires `last_old_commit`, `cutoff` (Unix milliseconds),
     `sessions_invalidated: true`, `historical_loss_accepted: true`, and no chosen
     receipts. The cutoff must follow the full 24-hour retry horizon and cannot
     be in the future. This is an explicit operator decision, never a default.
   Missing evidence and unresolved classifications remain blocked. The tool
   validates the attestations' structure; it cannot establish their truth.
3. Review the complete file, then run
   `orbisync-server checkpoint-operator review-digest --review review.json`.
   This command is local-only and does not acquire database ownership. It prints
   the digest of the parsed artifact's canonical JSON serialization.
4. Run `orbisync-server checkpoint-operator apply --review review.json --approve <DIGEST> --report conversion.jsonl --old-writer-exit-confirmed --retries 2`.
   The tool rechecks the current fixed checkpoint and row contents before
   approval and again inside conversion. Changed sources or reviewed input refuse.
   The report file is created exclusively and flushed after each record.

`--retries` explicitly requests up to 16 retries within this invocation, using
the retained attempt, snapshot, writer boot and receipt times. It does not repeat
approval or construct another attempt. An uncertain result is not success.
Preserve its report: after process exit, another invocation cannot resume that
in-memory boot. Resolve/review durable authority under the deployment recovery
procedure before further conversion; never treat a failed CLI response as proof
of absence. No automatic baseline, timestamp renewal or old-head fallback exists.

Review files are capped at 64 MiB including the two preserved source views and
chosen legacy input. Each chosen checkpoint still obeys the existing 8 MiB legacy
and configured read limits, domain/receipt validation and canonical stream limits.
Artifact JSON, retained inventories and chosen state are separately owned operator
memory, not codec scratch or a constant-RAM claim. Decode/manifest work uses the
existing global job and single conversion gates. Database driver cleanup, native
allocations and end-to-end shutdown deadlines remain separately unverified.

An admitted apply attempt has one five-second execution/observation budget across
preparation, inventory, approval and conversion. Global admission separately waits
at most two seconds. Each explicit same-process retry receives its own budget.
Setup, file reads/report sync and retained cleanup are separately owned; the entire
CLI invocation is not promised to finish in five seconds. Blocking worker results
cannot initiate approval or conversion after the execution deadline.

Reports distinguish `approval_pending`, `approved_conversion_not_started`, and
`conversion_pending`. Unknown approval never authorizes conversion or automatic
reapproval. A failed report write/sync leaves the outcome unknown; preserve the
reviewed-input record and both sources. A timeout never proves absence.

Transaction terminal operations and connection retirement remain owned after
observation expiry. Generation transaction connections are retired rather than
returned to the reusable pool; the pool slot stays charged through close. This
conservative policy requires subsequent PG/deployment throughput measurement.
Cleanup can outlive execution and keep the two job slots unavailable.

Server shutdown starts its configured drain/force clocks at the first signal,
including final persistence, connection drain, writer close and pool close. The
existing defaults remain 30 seconds plus 10 seconds. A second signal or force
expiry terminates nonzero with uncertainty; neither process exit nor a timer is
primary commit/absence evidence. The supervisor must prove exit before replacement,
and recovery must query authoritative state. Default enablement remains off.
