# Changelog

## 1.0.0

Initial release:

- Event-driven JSON profiling over json-skimmer, with tentative candidate
  aggregation, confirmed commit, and rejection rollback without document buffers.
- Typed path/type occurrence and record-presence counts, explicit limits,
  stream-byte accounting, confirmed/pending depths, and truncation reporting.
- Readable colored CLI reports plus flat/expanded JSON, periodic idle snapshots,
  Unix SIGINT snapshots, and broken-pipe handling.
- Mixed valid/invalid example stream and deterministic unit-test coverage.
- Preserve literal `paths:` prefixes in human path labels.
- Optional always-expanded JSON summary-file reports with stream-only terminal output, shared
  periodic/final-only scheduling, and continued file reporting after stdout closes.

See the release checklist for validation evidence. Publication remains developer-managed.
