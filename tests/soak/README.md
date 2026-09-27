# Soak test foundation

`soak.py` runs the existing public-contract load generator for a bounded test
window and records periodic Prometheus and **server** process-resource snapshots.
The runner prefers the server's `process_resident_memory_bytes` metric and
falls back to an explicitly supplied `--server-pid`; without either, RSS is
unavailable. The selected source is recorded in every sample and in
`rss_source`.
Missing
metrics are recorded as `unavailable`; they are never converted to zero.

The default remains the specification window of 24 hours. For the acceptance
exercise, run one hour explicitly:

```sh
python tests/soak/soak.py --duration 1h --interval 60s --users 200 \
  --metrics-url http://127.0.0.1:8080/metrics \
  --server-pid "$SERVER_PID" \
  --password-file .admin-pass.txt --instance-id "$INSTANCE"
```

The report is written to `artifacts/soak-report.json`. This one-hour run is
smoke/soak infrastructure validation only. The report computes E-1 as
`met`/`failed`/`not_evaluated` only when the actual duration, completion status,
and server RSS samples permit it; a one-hour run remains `not_evaluated`.
The RSS representative is the first and last successfully sampled server
values (explicitly reported as `server_rss_growth_ratio`).
