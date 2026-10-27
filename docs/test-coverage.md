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
| Flat exact sorted collection, partial writes, EOF/SIGINT metadata, rejected rollback, output errors, explicit flat/expanded and default human CLI format selection | `flat_reports_have_exact_sorted_counts_and_preserve_interrupt_state` |
| Read errors, writer errors, broken pipes, flush failures | `io_errors_propagate` |
| SIGINT request with pending candidate, confirmed-only totals, final-only mode | `interrupt_reports_pending_without_eof_and_ignores_queued_data` |
| Idle interrupt wakeup, exactly one report, interrupt write/flush failures | `idle_interrupt_reports_and_writer_failures_propagate` |
| Readable exact row fixture, numeric/colon alignment, safe controls, independent color/destination policy | `readable_rows_align_counts_and_escape_controls` |
| Human sections/order, partial writes, live/EOF/SIGINT status, plain/styled output, writer/flush failures | `readable_reports_put_paths_before_separated_stream_metrics` |
| FLON type palette, keys/array markers, default-color separators, escaped literal keys and controls | `path_components_and_types_use_consistent_colors_with_plain_separators` |
| Bundled ten-row example: seven confirmed/three rejected, recovery, EOF, deep object/array maxima, cutoff comparison, one-byte chunks | `example_stream_covers_recovery_deep_documents_and_eof` |
| Literal keys starting with `paths:` preserved in human output (regression failed before fix) | `human_report_preserves_keys_starting_with_metric_namespace` |
| Compiled public-library usage | Crate-level doctest |
| Interval validation | `intervals_are_bounded` |
| Always-expanded JSON files regardless of stdout format, preserved occurrence/record counts, colored stream-only stdout, SIGINT pending state, partial file writes, CLI flags | `summary_file_gets_full_reports_and_terminal_only_stream_metrics` |
| Periodic file reports and final-only suppression in both destinations | `summary_file_obeys_periodic_and_final_only_modes` |
| File write/flush failures, terminal failure, continued file reporting after stdout broken pipe | `summary_file_survives_terminal_broken_pipe_and_propagates_failures` |
| Observed depth beyond profiling cutoff, scalar depths, exact boundaries, commit/rollback, independent key/parser limits, chunk divisions | `encountered_depth_and_truncation_obey_confirmation_and_limits` |
| Human confirmed/pending depth and explicit truncation status | `depth_reporting_distinguishes_confirmed_and_pending_truncation` |
| Idle reporting and pending/final snapshots without sleeps | `periodic_snapshots_include_pending_input_and_idle_time` |

Process-level release checks live in `tools/check-release.py`. All ten were run
against the 1.0.0 release binary on Linux: example/version, summary-file routing,
idle SIGINT, final-only SIGINT with pending data, broken pipes with/without file
output, file-open and file-write failures, redirected color/JSON, and PTY colors.
The harness uses temporary files, bounded timeouts, and local processes only.

Remaining gaps: actual offset overflow at u64::MAX (impractical through public
input), cross-platform signal behavior, sustained performance/peak memory,
and independent property/fuzz campaigns. Async adapters, filters, embedded JSON,
and cross-sampler merging are outside the initial implementation.

See [release validation](release-checklist.md) for reproducible commands.
