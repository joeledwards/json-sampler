# json-sampler

A streaming JSON structure profiler: a library on top of json-skimmer and a CLI
that reads stdin. It profiles every discovered object/array candidate; it does
not randomly select records. Use it alongside FLON to explore data before
choosing how to format or query it.

## Installation

Rust 1.87 or newer is required by this package. Install the CLI from crates.io
with Cargo:

```sh
cargo install json-sampler --locked
```

For library use, disable CLI dependencies:

```toml
[dependencies]
json-sampler = { version = "1", default-features = false }
```

## Quick start

```sh
json-sampler --final-only < application.log
s3 scan s3://bucket/prefix | json-sampler --interval 5
```

Try `json-sampler --final-only < example.ndjson` with the included
[mixed example stream](example.ndjson). Its ten rows intentionally include invalid
JSON, so this is a parser demonstration rather than a strictly valid NDJSON file:

| Rows | Contents |
| --- | --- |
| 1–2 | Service snapshots with repeated paths, type variation, Unicode, escapes, and special keys |
| 3 | A top-level mixed array with nested and empty containers |
| 4 | Nested objects, reaching value depth 41 |
| 5 | Nested arrays, reaching value depth 49 |
| 6–7 | Missing-comma rejection followed by a valid recovery document |
| 8–9 | Invalid-escape rejection followed by a valid recovery document |
| 10 | Unfinished document at EOF (deliberately no final newline) |

With default limits, expect seven confirmed and three rejected candidates,
including one incomplete at EOF. Confirmed maximum depth is 49; two confirmed
candidates exceed the default profiling depth of 32. Compare with
`--profile-depth 64` to see the deeper paths and no depth truncation. The
`rejected_only` keys must never enter confirmed path counts. Embedded JSON text
in row 1 is profiled as a string. The entire example is only a few KiB.

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
Rejected candidates do not increment it. Duplicate keys are visited individually;
they are not collapsed
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

Any confirmed candidate with resource-limit omissions increments `candidates_profile_limited`.
Rejected candidates never increment this counter. Treat path counts as lower
bounds when it or `candidates_key_decode_errors` is nonzero. Existing entries
continue updating within a full map, while new entries are omitted. A path in
the confirmed map can still be missed if the tentative map filled before that
path was encountered in a later candidate. Tentative entries are admitted in event order; new
confirmed entries are admitted in sorted path/type order during commit. Entries
are never evicted. Two maps can each hold up to `max_paths` entries, and taking a
snapshot temporarily copies the confirmed map. Memory depends on configured
limits, not document or stream length. Limits do not stop the stream.

## CLI output and lifecycle

Depth reporting uses **value/path depth**, with root depth zero (different from
the parser's container limit, which counts the root as one):

- `max_depth_encountered`: deepest value in confirmed candidates, including
  scalar leaves and values beyond the profiling cutoff. Zero before any deeper
  confirmed value is encountered.
- `candidates_depth_truncated`: confirmed candidates with values deeper than
  `profile_depth`. A nonzero count explicitly indicates depth-based omissions;
  those candidates also count toward `candidates_profile_limited`.
- `pending_max_depth` and `pending_depth_truncated`: depth and cutoff status of
  the currently open candidate, shown separately in live/interrupt snapshots.
  Both reset when that candidate is confirmed or rejected.

Depth is tracked even under subtrees omitted because of key/path/cardinality
limits. Those other limits alone do not imply depth truncation. Rejected and
EOF-truncated candidates never change confirmed depth statistics. A parser
depth-limit error rejects the candidate; its refused container is not a value
event and is not included in the observed pending maximum.

The default (`--format human`) is a readable report with path occurrence counts
first, followed by a blank line and stream metrics. Counts are right aligned
before aligned colons within each section. For example:

```text
JSON sampler (live)
Path counts
  20 : items.$_ARRAY_ITEM_$=NUMBER
   3 : items=ARRAY

Stream metrics
   3 : candidates confirmed
   1 : candidates pending
  ...
```

The full stream section includes every numeric stream statistic and parser error
counts. Empty path collections display `(none)`. Report status is `live`,
`complete` at EOF, or `interrupted` on SIGINT. Control characters in labels are
escaped so keys cannot inject terminal controls or split report rows.

Path styling follows FLON: keys and `OBJECT` are blue; array-item markers,
`ARRAY`, `NUMBER`, and numeric counts are orange; `STRING` is green, `BOOLEAN`
is magenta, and `NULL` is yellow. Separator periods and equals signs use the
default text color. Escaped literal periods remain part of their key component.

Human output uses colored headings, path components, types, and numbers when
stdout is a terminal. Nonempty `NO_COLOR` disables automatic colors. `--color always` forces
colors, including redirected output; `--color never` disables them. Colors do
not affect padding or numeric alignment and are never added to JSON output.

For machine-readable output, `--format flat` emits compact newline-delimited JSON
with `finished`, `stream`, and a flat occurrence-count `metrics` map.
`--format expanded` emits compact newline-delimited JSON with the typed path/type
entries described above, including record-presence counts:

```sh
json-sampler --format expanded --final-only < example.ndjson
```

Add `--format flat` to select the flat JSON collection.
For `{"items":[1,2]}`, its `metrics` collection is:

```json
{"paths:items.$_ARRAY_ITEM_$=NUMBER":2,"paths:items=ARRAY":1}
```

Names use dot-separated keys, `$_ARRAY_ITEM_$` for array elements, and uppercase
terminal types. Root entries are omitted, matching S3 sample; root candidate
counts remain in `stream`. Flat counts are occurrences, not record-presence
counts. Use `--format expanded` to retain both. Literal backslashes, dots, equals
signs, and dollar signs in keys are backslash escaped to prevent path collisions;
for example, a literal `a.b` key is `paths:a\.b=NUMBER` (the backslash is itself
escaped in JSON). This is the S3 sample naming style with explicit escaping,
not byte-for-byte compatibility for every key. Library callers can obtain this
map with `Snapshot::flat_metrics()`.

All formats use the same confirmation, resource-limit, periodic, EOF, and
SIGINT behavior. Defaults are cumulative reports every five seconds plus a final
report at EOF. `--final-only` suppresses periodic reports. Periodic snapshots also
run while stdin is idle, using a reader thread and bounded read-ahead. Deadlines
are checked between input chunks; parsing a chunk or writing a large report can
postpone a report. Slow output applies backpressure.

### End-only and file reports

```sh
# One full report at EOF (or on Unix SIGINT).
json-sampler --final-only < example.ndjson

# Save the full report; show only readable stream metrics on stdout.
json-sampler --final-only --summary-file summary.json < example.ndjson

# Save complete machine-readable snapshots while monitoring stream metrics.
s3 scan s3://bucket/prefix | json-sampler --summary-file summary.ndjson
```

`--summary-file PATH` creates or truncates the file at startup. Each reporting
event appends and flushes a complete expanded JSON report. Without
`--final-only`, the file therefore contains cumulative periodic reports plus the
EOF or SIGINT report; it is not replaced at each interval. `--final-only` writes
only the EOF/SIGINT report to the file and only the corresponding stream report
to stdout. Input errors do not produce a final report.

Summary files always contain compact expanded JSON, regardless of filename
extension or `--format`: typed paths, occurrence counts, record-presence counts,
stream statistics, and lifecycle metadata, subject to configured profiling limits.
Periodic reports form NDJSON; `--final-only` produces one JSON record. File output
never contains ANSI colors, even with `--color always`.

`--format` controls stdout only when no summary file is selected. With a summary
file, stdout always shows readable stream-only reports using the usual terminal
color policy. No path counts are printed to stdout in this mode.

The file is written and flushed before the stdout report. File open/write/flush
failures exit unsuccessfully. A broken stdout pipe disables further terminal
reports but continues collection and file reporting through EOF/SIGINT. Other
stdout failures remain fatal; reports already written to the file remain there.

Diagnostics go to stderr. Malformed candidates are data, not fatal process
errors. Broken stdout pipes exit successfully. Other read/write/flush errors exit
unsuccessfully; a read error is not treated as clean EOF and produces no final
report.

On Unix (including Linux and macOS), Ctrl-C / SIGINT requests one final snapshot,
flushes stdout, and exits with status 130. This works with `--final-only` and
idle stdin. JSON interrupt snapshots add `stop_reason: "sigint"` and retain
`finished: false`; human reports display `interrupted`. Interruption is not EOF.
An open candidate remains pending;
its path metrics are uncommitted, and no EOF rejection is invented. Only bytes
already processed by the sampler are counted; queued input is not drained.
The signal handler only sets an atomic flag; parsing and output happen in the
normal processing loop. Idle waits are capped at 100 ms. Parsing a chunk or
blocked output can delay shutdown, and additional SIGINTs do not force exit.
Broken-pipe and other output-error exit policies still apply. Other signals
retain their default behavior. Non-Unix targets currently retain ordinary
Ctrl-C termination without a guaranteed report.

The CLI does not yet provide filters, embedded-JSON parsing, snapshot merging,
or periodic deltas.

## Compatibility

Use JSON formats for automation. Both emit one compact JSON object per line;
human layout, labels, spacing, and colors are presentation rather than a parsing
interface. Consumers should tolerate additional JSON fields. Path escaping,
value-type names, occurrence/record definitions, candidate confirmation, and
byte/depth accounting are documented contracts. Snapshot/library types currently
support serialization, not deserialization or resuming a saved sampler.

Malformed candidates do not cause a failing process exit. A clean EOF exits 0,
including when every candidate was rejected. Unix SIGINT exits 130 after its
report; broken stdout pipes exit 0; other I/O failures exit nonzero. Reports are
cumulative, so summing successive reports would double-count data.

For release validation and remaining platform/performance gaps, see the
[release checklist](docs/release-checklist.md). Changes are recorded in
[CHANGELOG.md](CHANGELOG.md).

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
the filesystem cache. It measures `--final-only --format expanded`,
not periodic reporting.
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
