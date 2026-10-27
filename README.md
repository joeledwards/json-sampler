# json-sampler

A streaming JSON structure profiler: a library on top of json-skimmer and a CLI
that reads stdin. It profiles every discovered object/array candidate; it does
not randomly select records. Use it alongside FLON to explore data before
choosing how to format or query it.

```sh
json-sampler --final-only < application.log
s3 scan s3://bucket/prefix | json-sampler --interval 5
```

Input can contain adjacent JSON candidates, unrelated bytes, and malformed or
truncated JSON. Newlines and complete in-memory documents are unnecessary.
S3 listing, decompression, and object framing belong to the upstream tool.
Direct concatenation can join fragments from separate objects; separators do
not guarantee isolation. This program profiles the received byte stream.

## Library

```rust
use json_sampler::Sampler;

let mut sampler = Sampler::default();
sampler.push(b"log: {\"items\":[1,");
assert!(sampler.snapshot().metrics.is_empty()); // still speculative
sampler.push(b"2]} trailing text");
let live = sampler.snapshot(); // confirmed metrics available before EOF
assert_eq!(live.stream.candidates_confirmed, 1);
sampler.finish(); // call exactly once at actual EOF; repeated calls are harmless
```

`Sampler::new(Config)` configures limits. `push` accepts arbitrary byte chunks;
`finish` rejects any remaining candidate and closes the sampler. Later pushes
are ignored. `snapshot` returns owned, serializable cumulative statistics without
changing parser state. The library has no clock, threads, transport, or output.

## Profiling contract

Every value has a type: object, array, string, number, boolean, or null. The root
object/array has an empty path. Array elements share the typed `item` segment.
Object keys are decoded JSON strings. Paths are arrays of typed segments, so a
key containing dots, slashes, tildes, or the word `item` cannot collide with a
nested path or array item.

For `{"items":[1,2]}`, one metric is:

```json
{"path":[{"key":"items"},"item"],"value_type":"number","counts":{"occurrences":2,"records":1}}
```

`occurrences` counts each value, including duplicate object keys and repeated
array elements. `records` counts confirmed candidates containing the path/type
at least once. json-skimmer accepts syntactically valid lone surrogate escapes;
such keys cannot be decoded into Rust strings. Their subtrees are omitted, and
confirmed candidates containing them increment `candidates_key_decode_errors`.
Rejected candidates do not increment it. Duplicate keys are visited individually; they are not collapsed
with last-key-wins semantics. Root metrics count all confirmed candidates of
that root type, subject to profiling limits. Top-level scalars are outside text,
following json-skimmer's object/array discovery contract.

Candidate metrics remain tentative until `CandidateEnd`, then merge into totals.
Rejection discards all tentative metrics; its stream/error statistics remain.
Snapshots contain only confirmed path metrics, sorted by typed path and type.
A rejected candidate can contain complete nested values, but none are committed
unless parser recovery discovers and confirms a new candidate.

No objects, arrays, string values, or number spellings are buffered. Storage
contains the parser/container stack, retained keys and paths, and tentative and
confirmed aggregates. String and number fragment sizes do not affect counts.

## Stream accounting

Every snapshot satisfies:

```text
consumed_bytes = outside_bytes + confirmed_bytes + rejected_bytes + pending_bytes
candidates_started = candidates_confirmed + candidates_rejected + candidates_pending
```

- Outside bytes include whitespace outside candidates and arbitrary text/binary
  bytes. They are not interpreted as intended JSON fragments.
- Confirmed bytes include root delimiters and all internal whitespace.
- Rejected bytes include the byte exposing the error, except when json-skimmer
  reuses that byte to open a new candidate: it then belongs to the new candidate.
- Pending bytes belong to the currently open, unconfirmed candidate. At EOF
  they move to rejected bytes and increment `candidates_incomplete_at_eof`, even
  when the final error is `InvalidUtf8` rather than `UnexpectedEnd`.
- Parser limit errors count as rejections. A depth/candidate-limit offending
  byte is consumed without reuse. Offset overflow consumes no additional byte.

`errors` is a map keyed by json-skimmer error names (such as `UnexpectedByte`,
`UnexpectedEnd`, `InvalidUtf8`, and `CandidateLimitExceeded`). Counts describe
parser detections, not an inferred count of all intended JSON documents.

## Limits

| Option / Config field | Default | Effect |
| --- | --- | --- |
| `--max-depth` / `max_depth` | 1000 | Parser container limit; root is one; rejects excessive candidates |
| `--max-candidate-bytes` / `max_candidate_bytes` | unlimited | Optional positive raw candidate size limit; first excess byte rejects |
| `--profile-depth` / `profile_depth` | 32 | Root path is zero; deeper paths are omitted while validation continues |
| `--max-paths` / `max_paths` | 10000 | Path/type entry cap in each tentative and confirmed map |
| `--max-key-bytes` / `max_key_bytes` | 4096 | Raw JSON key bytes, excluding quotes; oversized keys and their subtrees are omitted |
| `--max-path-bytes` / `max_path_bytes` | 16384 | Decoded key bytes plus one per array segment; oversized paths/subtrees are omitted |

Any confirmed candidate with omissions increments `candidates_profile_limited`.
Rejected candidates never increment this counter. Treat path counts as lower
bounds when it or `candidates_key_decode_errors` is nonzero. Existing entries continue updating after a map fills;
new entries are omitted. Tentative entries are admitted in event order; new
confirmed entries are admitted in sorted path/type order during commit. Entries
are never evicted. Two maps can each hold up to `max_paths` entries, and taking a
snapshot temporarily copies the confirmed map. Memory depends on configured
limits, not document or stream length. Limits do not stop the stream.

## CLI output and lifecycle

Stdout contains newline-delimited JSON snapshots: `finished`, `stream`, and
`metrics`. `--format structured` (the default) uses the typed path/type entries
described above. `--format flat` uses an S3-sample-style sorted occurrence-count
map inside the same envelope:

```sh
s3 scan s3://bucket/prefix | json-sampler --format flat
```

For `{"items":[1,2]}`, the flat `metrics` collection is:

```json
{"paths:items.$_ARRAY_ITEM_$=NUMBER":2,"paths:items=ARRAY":1}
```

Names use dot-separated keys, `$_ARRAY_ITEM_$` for array elements, and uppercase
terminal types. Root entries are omitted, matching S3 sample; root candidate
counts remain in `stream`. Flat counts are occurrences, not record-presence
counts. Use structured output to retain both. Literal backslashes, dots, equals
signs, and dollar signs in keys are backslash escaped to prevent path collisions;
for example, a literal `a.b` key is `paths:a\.b=NUMBER` (the backslash is itself
escaped in JSON). This is the S3 sample naming style with explicit escaping,
not byte-for-byte compatibility for every key. Library callers can obtain this
map with `Snapshot::flat_metrics()`.

Both formats use the same confirmation, resource-limit, periodic, EOF, and
SIGINT behavior. Defaults are cumulative reports every five seconds plus a final
report at EOF. `--final-only` suppresses periodic reports. Periodic snapshots also
run while stdin is idle, using a reader thread and bounded read-ahead. Deadlines
are checked between input chunks; parsing a chunk or writing a large report can
postpone a report. Slow output applies backpressure.

Diagnostics go to stderr. Malformed candidates are data, not fatal process
errors. Broken stdout pipes exit successfully. Other read/write/flush errors exit
unsuccessfully; a read error is not treated as clean EOF and produces no final
report.

On Unix (including Linux and macOS), Ctrl-C / SIGINT requests one final snapshot,
flushes stdout, and exits with status 130. This works with `--final-only` and
idle stdin. The interrupt snapshot adds `stop_reason: "sigint"` and retains
`finished: false`: interruption is not EOF. An open candidate remains pending;
its path metrics are uncommitted, and no EOF rejection is invented. Only bytes
already processed by the sampler are counted; queued input is not drained.
The signal handler only sets an atomic flag; parsing and output happen in the
normal processing loop. Idle waits are capped at 100 ms. Parsing a chunk or
blocked output can delay shutdown, and additional SIGINTs do not force exit.
Broken-pipe and other output-error exit policies still apply. Other signals
retain their default behavior. Non-Unix targets currently retain ordinary
Ctrl-C termination without a guaranteed report.

The CLI does not yet provide human tables, filters, embedded-JSON
parsing, snapshot merging, or periodic deltas.

## Development and performance

From this directory, using the workspace-local toolchain:

```sh
../../scripts/with-rust cargo test --offline --lib --bins
../../scripts/with-rust cargo fmt --check
../../scripts/with-rust cargo clippy --offline --all-targets --all-features -- -D warnings
```

See [the coverage matrix](docs/test-coverage.md). Unit tests use deterministic,
bounded json-volcano corpora and controlled chunk divisions. This is correctness
validation, not a performance measurement.

For developer-run performance experiments, build release binaries first. Generate
finite json-volcano fixtures before timing json-sampler so generation speed is
excluded; measure peak resident memory as well as elapsed time. Compare many
small candidates, large candidates, deep nesting, and high path cardinality.
Measure periodic reporting separately from `--final-only`. A live pipeline tests
combined generator/consumer performance rather than isolated sampler throughput.
No benchmark results have been established yet.

The workspace provides a developer-run benchmark script (Python 3 on Linux/macOS).
From the workspace root:

```sh
scripts/measure-json-sampler --bytes 64MiB --candidates 1024 --repeats 3
```

It builds both release binaries offline with locked dependencies, generates each
fixture before timing, then checks final reports for valid records, a large
string, invalid candidates, mixed outcomes, and EOF truncation. It reports
wall-clock throughput and sampler peak RSS, excluding builds and generation.
Timing includes process startup and final report output; repeated reads may use
the filesystem cache. It measures `--final-only`, not periodic reporting.
Fixtures, generator logs, reports, and `results.json` are retained in a temporary
directory printed at startup. Five fixtures use approximately five times the
selected byte budget. Use `--output-dir PATH` to select a new artifact directory.
This script is a proposed manual benchmark; it has not been run end to end.

The baseline deliberately uses `--shape records`: arrays produce stable grouped
paths even when their values vary. The `object` shape instead generates random
`record_<number>` keys and can exhaust the sampler's path budget. Treat that as a
separate cardinality/limit workload. The script fails validation if profiling
limits cause omissions; it does not silently benchmark incomplete statistics.
A future generator option for a bounded key vocabulary would allow controlled
cardinality experiments across object shapes.
