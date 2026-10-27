# Behavior-to-test coverage

All tests are isolated unit tests in `src/lib.rs` and `src/main.rs`.
The `splits` helper checks whole input, every two-chunk division (including empty
chunks), one-byte chunks, and reproducible pseudorandom divisions, comparing
complete normalized snapshots. Byte and candidate accounting is checked after
each push.

| Behavior | Tests |
| --- | --- |
| Types, root paths, arrays, duplicate keys, occurrence vs record counts | `paths_types_presence_and_duplicate_keys` |
| UTF-8, escapes, surrogate pairs, empty keys, typed path distinctions | `decoded_keys_are_chunk_independent_and_typed` |
| Lone surrogate keys explicitly omitted, confirmed-only diagnostics | `unpaired_surrogate_keys_are_explicitly_omitted` |
| Pending snapshots, commit, rollback, finish idempotence, post-EOF pushes | `speculative_counts_never_leak_and_lifecycle_is_idempotent` |
| Recovery reuses offending opener without double counting | `recovery_byte_belongs_to_new_candidate_once` |
| Outside bytes, empty input, invalid/truncated strings, escapes, UTF-8, numbers, literals | `rejection_and_eof_corpus` |
| Parser depth/byte boundaries and recovery; EOF at limit | `candidate_and_parser_depth_limits_recover` |
| Key/path/depth/cardinality caps, subtree omission, rollback, continued updates | `profile_limits_are_explicit_and_rollback_is_clean` |
| Exact key/path/depth boundaries, zero-length limits | `exact_key_path_and_depth_boundaries` |
| Repeated live candidates, large string values in incremental input, pending snapshots | `long_stream_and_large_values_need_only_incremental_input` |
| Generated valid, invalid, truncated corpora across chunks | `bounded_volcano_corpus_matches_across_chunks` |
| Exact NDJSON fixture, partial writes, EOF rejection | `final_report_handles_partial_writes_and_eof` |
| Flat path/type occurrence maps, arrays, Unicode, empty/special keys, collision escaping, omitted roots | `flat_metrics_match_s3_sample_and_preserve_distinct_paths` |
| Flat exact sorted collection, partial writes, EOF/SIGINT metadata, rejected rollback, output errors, CLI format selection | `flat_reports_have_exact_sorted_counts_and_preserve_interrupt_state` |
| Read errors, writer errors, broken pipes, flush failures | `io_errors_propagate` |
| SIGINT request with pending candidate, confirmed-only totals, final-only mode | `interrupt_reports_pending_without_eof_and_ignores_queued_data` |
| Idle interrupt wakeup, exactly one report, interrupt write/flush failures | `idle_interrupt_reports_and_writer_failures_propagate` |
| Interval validation | `intervals_are_bounded` |
| Idle reporting and pending/final snapshots without sleeps | `periodic_snapshots_include_pending_input_and_idle_time` |

Gaps: actual offset overflow at u64::MAX (impractical through public input),
process-level signal and shell pipeline behavior, sustained performance and peak
memory measurements, independent property/fuzz campaigns. These have not been
run. Async input adapters, filters, embedded JSON, cross-sampler merging, and
human output are outside the initial implementation.

Manual verification proposed, not run: build the CLI, pipe a stream that emits
`{}[` and stays open into `json-sampler --final-only`, then send SIGINT to the
sampler PID. Expect one NDJSON snapshot with `stop_reason: "sigint"`,
`finished: false`, one confirmed candidate, one pending byte, no EOF rejection,
and exit status 130. Repeat while stdin is idle and with periodic reporting.
This checks actual OS signal wiring separately from isolated state/IO unit tests.
