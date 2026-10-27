//! Incremental structure profiling. Only confirmed candidates enter path totals.
use json_skimmer::{ErrorKind, Event, Mode, Parser, StringKind};
use serde::Serialize;
use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};

/// Resource limits. Each aggregation map retains at most `max_paths` entries.
#[derive(Clone, Debug)]
pub struct Config {
    pub max_depth: NonZeroUsize,
    pub max_candidate_bytes: Option<NonZeroU64>,
    /// Root is depth zero; this only limits profiling, not validation.
    pub profile_depth: usize,
    pub max_paths: NonZeroUsize,
    /// Maximum raw JSON bytes retained for any one key (excluding quotes).
    pub max_key_bytes: usize,
    /// Sum of decoded key bytes and array marker bytes along a retained path.
    pub max_path_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_depth: NonZeroUsize::new(1000).unwrap(),
            max_candidate_bytes: None,
            profile_depth: 32,
            max_paths: NonZeroUsize::new(10_000).unwrap(),
            max_key_bytes: 4096,
            max_path_bytes: 16_384,
        }
    }
}

/// Typed segments prevent collisions between keys, nested paths and array items.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    Key(String),
    Item,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}
type MetricKey = (Vec<Segment>, ValueType);

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Counts {
    pub occurrences: u64,
    /// Number of confirmed candidates containing this path/type at least once.
    pub records: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Metric {
    pub path: Vec<Segment>,
    pub value_type: ValueType,
    pub counts: Counts,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct StreamStats {
    pub consumed_bytes: u64,
    pub outside_bytes: u64,
    pub confirmed_bytes: u64,
    pub rejected_bytes: u64,
    pub pending_bytes: u64,
    pub candidates_started: u64,
    pub candidates_confirmed: u64,
    pub candidates_rejected: u64,
    pub candidates_pending: u64,
    pub candidates_incomplete_at_eof: u64,
    /// Confirmed candidates whose profiling exceeded configured limits.
    pub candidates_profile_limited: u64,
    /// Confirmed candidates with keys that cannot be decoded as Unicode strings.
    pub candidates_key_decode_errors: u64,
    pub errors: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Snapshot {
    pub finished: bool,
    pub stream: StreamStats,
    /// Committed metrics only. Sorted by typed path and value type.
    pub metrics: Vec<Metric>,
}
impl Snapshot {
    /// S3-sample-style occurrence counts. Roots are omitted. Backslashes, dots,
    /// equals signs and dollar signs in keys are backslash escaped so literal
    /// keys cannot collide with nested paths or `$_ARRAY_ITEM_$` segments.
    /// Record-presence counts remain available in the structured metrics.
    pub fn flat_metrics(&self) -> BTreeMap<String, u64> {
        self.metrics
            .iter()
            .filter(|metric| !metric.path.is_empty())
            .map(|metric| {
                let path = metric
                    .path
                    .iter()
                    .map(|segment| match segment {
                        Segment::Item => "$_ARRAY_ITEM_$".to_owned(),
                        Segment::Key(key) => {
                            let mut escaped = String::new();
                            for ch in key.chars() {
                                if matches!(ch, '\\' | '.' | '=' | '$') {
                                    escaped.push('\\');
                                }
                                escaped.push(ch);
                            }
                            escaped
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(".");
                let kind = match metric.value_type {
                    ValueType::Object => "OBJECT",
                    ValueType::Array => "ARRAY",
                    ValueType::String => "STRING",
                    ValueType::Number => "NUMBER",
                    ValueType::Boolean => "BOOLEAN",
                    ValueType::Null => "NULL",
                };
                (format!("paths:{path}={kind}"), metric.counts.occurrences)
            })
            .collect()
    }
}

struct Frame {
    path: Option<Vec<Segment>>,
    key: Option<String>,
    object: bool,
}
struct Aggregator {
    config: Config,
    stats: StreamStats,
    totals: BTreeMap<MetricKey, Counts>,
    pending: BTreeMap<MetricKey, u64>,
    start: Option<u64>,
    frames: Vec<Frame>,
    key: Vec<u8>,
    key_overflow: bool,
    limited: bool,
    key_decode_error: bool,
    // A rejected byte can be reused as the next candidate's opener.
    rejected_end: Option<u64>,
}
/// Owns a scan-mode parser and bounded tentative/confirmed aggregates.
/// No complete JSON document or scalar value is retained.
pub struct Sampler {
    parser: Parser,
    aggregate: Aggregator,
    finished: bool,
}
impl Sampler {
    pub fn new(config: Config) -> Self {
        Self {
            parser: Parser::with_limits(Mode::Scan, config.max_depth, config.max_candidate_bytes),
            aggregate: Aggregator {
                config,
                stats: StreamStats::default(),
                totals: BTreeMap::new(),
                pending: BTreeMap::new(),
                start: None,
                frames: Vec::new(),
                key: Vec::new(),
                key_overflow: false,
                limited: false,
                key_decode_error: false,
                rejected_end: None,
            },
            finished: false,
        }
    }
    /// Consume arbitrary chunks. Calls after `finish` are ignored.
    pub fn push(&mut self, bytes: &[u8]) {
        let aggregate = &mut self.aggregate;
        self.parser
            .push(bytes, |event| aggregate.event(event, false));
        self.aggregate.account(self.parser.offset());
    }
    /// Finalize once at EOF. An open candidate is rejected, never committed.
    pub fn finish(&mut self) {
        let aggregate = &mut self.aggregate;
        self.parser.finish(|event| aggregate.event(event, true));
        self.aggregate.account(self.parser.offset());
        self.finished = true;
    }
    /// Returns an owned snapshot; taking one does not finalize the parser.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            finished: self.finished,
            stream: self.aggregate.stats.clone(),
            metrics: self
                .aggregate
                .totals
                .iter()
                .map(|((path, value_type), counts)| Metric {
                    path: path.clone(),
                    value_type: *value_type,
                    counts: counts.clone(),
                })
                .collect(),
        }
    }
}
impl Default for Sampler {
    fn default() -> Self {
        Self::new(Config::default())
    }
}
impl Aggregator {
    fn account(&mut self, consumed: u64) {
        self.stats.consumed_bytes = consumed;
        self.stats.pending_bytes = self.start.map_or(0, |start| consumed - start);
        self.stats.candidates_pending = u64::from(self.start.is_some());
        self.stats.outside_bytes = consumed
            - self.stats.confirmed_bytes
            - self.stats.rejected_bytes
            - self.stats.pending_bytes;
    }
    fn value(&mut self, value_type: ValueType) -> Option<Vec<Segment>> {
        let mut path = match self.frames.last_mut() {
            None => Some(Vec::new()),
            Some(frame) => frame.path.clone().and_then(|mut path| {
                let segment = if let Some(key) = frame.key.take() {
                    Segment::Key(key)
                } else if frame.object {
                    return None;
                } else {
                    Segment::Item
                };
                path.push(segment);
                Some(path)
            }),
        };
        if let Some(path_ref) = &path {
            let size: usize = path_ref
                .iter()
                .map(|s| match s {
                    Segment::Key(k) => k.len(),
                    Segment::Item => 1,
                })
                .sum();
            if path_ref.len() > self.config.profile_depth || size > self.config.max_path_bytes {
                self.limited = true;
                path = None;
            }
        }
        if let Some(path) = &path {
            let metric = (path.clone(), value_type);
            if let Some(count) = self.pending.get_mut(&metric) {
                *count += 1;
            } else if self.pending.len() < self.config.max_paths.get() {
                self.pending.insert(metric, 1);
            } else {
                self.limited = true;
            }
        }
        path
    }
    fn event(&mut self, event: Event<'_>, eof: bool) {
        match event {
            Event::CandidateStart { offset, .. } => {
                if self.rejected_end == offset.checked_add(1) {
                    self.stats.rejected_bytes -= 1;
                }
                self.rejected_end = None;
                self.start = Some(offset);
                self.stats.candidates_started += 1;
                self.pending.clear();
                self.frames.clear();
                self.limited = false;
                self.key_decode_error = false;
            }
            Event::ObjectStart { .. } => {
                let path = self.value(ValueType::Object);
                self.frames.push(Frame {
                    path,
                    key: None,
                    object: true,
                });
            }
            Event::ArrayStart { .. } => {
                let path = self.value(ValueType::Array);
                self.frames.push(Frame {
                    path,
                    key: None,
                    object: false,
                });
            }
            Event::ObjectEnd { .. } | Event::ArrayEnd { .. } => {
                self.frames.pop();
            }
            Event::StringStart {
                kind: StringKind::Key,
                ..
            } => {
                self.key.clear();
                self.key_overflow = false;
            }
            Event::StringFragment {
                kind: StringKind::Key,
                bytes,
                ..
            } => {
                if !self.key_overflow {
                    if bytes.len() <= self.config.max_key_bytes.saturating_sub(self.key.len()) {
                        self.key.extend_from_slice(bytes);
                    } else {
                        self.key_overflow = true;
                        self.limited = true;
                        self.key.clear();
                    }
                }
            }
            Event::StringEnd {
                kind: StringKind::Key,
                ..
            } => {
                let decoded = if self.key_overflow {
                    None
                } else {
                    let mut quoted = Vec::with_capacity(self.key.len() + 2);
                    quoted.push(b'"');
                    quoted.extend_from_slice(&self.key);
                    quoted.push(b'"');
                    serde_json::from_slice::<String>(&quoted).ok()
                };
                if decoded.is_none() && !self.key_overflow {
                    self.key_decode_error = true;
                }
                if let Some(frame) = self.frames.last_mut() {
                    frame.key = decoded;
                }
            }
            Event::StringStart {
                kind: StringKind::Value,
                ..
            } => {
                self.value(ValueType::String);
            }
            Event::NumberStart { .. } => {
                self.value(ValueType::Number);
            }
            Event::Boolean { .. } => {
                self.value(ValueType::Boolean);
            }
            Event::Null { .. } => {
                self.value(ValueType::Null);
            }
            Event::CandidateEnd { span, .. } => {
                self.stats.confirmed_bytes += span.end - span.start;
                self.stats.candidates_confirmed += 1;
                for (metric, occurrences) in std::mem::take(&mut self.pending) {
                    if let Some(count) = self.totals.get_mut(&metric) {
                        count.occurrences += occurrences;
                        count.records += 1;
                    } else if self.totals.len() < self.config.max_paths.get() {
                        self.totals.insert(
                            metric,
                            Counts {
                                occurrences,
                                records: 1,
                            },
                        );
                    } else {
                        self.limited = true;
                    }
                }
                self.stats.candidates_profile_limited += u64::from(self.limited);
                self.stats.candidates_key_decode_errors += u64::from(self.key_decode_error);
                self.start = None;
                self.frames.clear();
                self.key.clear();
            }
            Event::Invalid(error) => {
                *self
                    .stats
                    .errors
                    .entry(format!("{:?}", error.kind))
                    .or_default() += 1;
                if let Some(start) = self.start.take() {
                    let end = if eof || error.kind == ErrorKind::OffsetOverflow {
                        error.offset
                    } else {
                        error.offset + 1
                    };
                    self.stats.rejected_bytes += end - start;
                    self.rejected_end = Some(end);
                    self.stats.candidates_rejected += 1;
                    self.stats.candidates_incomplete_at_eof += u64::from(eof);
                }
                self.pending.clear();
                self.frames.clear();
                self.key.clear();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(input: &[u8], config: Config, chunks: &[usize]) -> Snapshot {
        let mut sampler = Sampler::new(config);
        let mut at = 0;
        for &size in chunks {
            let end = (at + size).min(input.len());
            sampler.push(&input[at..end]);
            at = end;
            invariant(&sampler.snapshot());
        }
        sampler.push(&input[at..]);
        sampler.finish();
        let snapshot = sampler.snapshot();
        invariant(&snapshot);
        snapshot
    }
    fn invariant(snapshot: &Snapshot) {
        let s = &snapshot.stream;
        assert_eq!(
            s.consumed_bytes,
            s.outside_bytes + s.confirmed_bytes + s.rejected_bytes + s.pending_bytes
        );
        assert_eq!(
            s.candidates_started,
            s.candidates_confirmed + s.candidates_rejected + s.candidates_pending
        );
    }
    fn splits(input: &[u8], config: Config) -> Snapshot {
        let expected = run(input, config.clone(), &[]);
        for at in 0..=input.len() {
            assert_eq!(run(input, config.clone(), &[at]), expected, "split {at}");
        }
        assert_eq!(run(input, config.clone(), &vec![1; input.len()]), expected);
        let mut seed = 73_u64;
        let chunks: Vec<_> = (0..input.len())
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (seed % 13 + 1) as usize
            })
            .collect();
        assert_eq!(run(input, config, &chunks), expected);
        expected
    }
    fn metric<'a>(s: &'a Snapshot, path: &[Segment], kind: ValueType) -> &'a Counts {
        &s.metrics
            .iter()
            .find(|m| m.path == path && m.value_type == kind)
            .unwrap()
            .counts
    }
    fn key(s: &str) -> Segment {
        Segment::Key(s.into())
    }
    #[test]
    fn paths_types_presence_and_duplicate_keys() {
        let input = br#"{"a":[1,2,"x",null,true],"a":[3],"a.b":{},"a/b":{"~":false}}{"a":[4]}"#;
        let s = splits(input, Config::default());
        assert_eq!(s.stream.candidates_confirmed, 2);
        assert_eq!(s.stream.outside_bytes, 0);
        assert_eq!(
            *metric(&s, &[key("a"), Segment::Item], ValueType::Number),
            Counts {
                occurrences: 4,
                records: 2
            }
        );
        assert_eq!(
            *metric(&s, &[key("a")], ValueType::Array),
            Counts {
                occurrences: 3,
                records: 2
            }
        );
        for (path, kind) in [
            (vec![], ValueType::Object),
            (vec![key("a.b")], ValueType::Object),
            (vec![key("a/b"), key("~")], ValueType::Boolean),
            (vec![key("a"), Segment::Item], ValueType::String),
            (vec![key("a"), Segment::Item], ValueType::Null),
        ] {
            assert!(metric(&s, &path, kind).occurrences > 0);
        }
    }
    #[test]
    fn decoded_keys_are_chunk_independent_and_typed() {
        let s = splits(
            "{\"é\":1,\"\\u00e9\":2,\"\\uD83D\\uDE00\":3,\"\\\"\\\\\":4,\"\":5,\"item\":6}[7]"
                .as_bytes(),
            Config::default(),
        );
        assert_eq!(
            *metric(&s, &[key("é")], ValueType::Number),
            Counts {
                occurrences: 2,
                records: 1
            }
        );
        for k in ["😀", "\"\\", "", "item"] {
            assert_eq!(metric(&s, &[key(k)], ValueType::Number).occurrences, 1);
        }
        assert_eq!(
            metric(&s, &[Segment::Item], ValueType::Number).occurrences,
            1
        );
    }
    #[test]
    fn speculative_counts_never_leak_and_lifecycle_is_idempotent() {
        let mut sampler = Sampler::default();
        sampler.push(br#"noise {"x":1"#);
        let pending = sampler.snapshot();
        assert!(pending.metrics.is_empty());
        assert_eq!(pending.stream.pending_bytes, 6);
        assert_eq!(pending.stream.outside_bytes, 6);
        sampler.push(b"} [1,X] {}");
        let before = sampler.snapshot();
        assert_eq!(before.stream.candidates_confirmed, 2);
        assert_eq!(before.stream.candidates_rejected, 1);
        assert!(
            !before
                .metrics
                .iter()
                .any(|m| m.value_type == ValueType::Array)
        );
        sampler.finish();
        let final_snapshot = sampler.snapshot();
        sampler.finish();
        sampler.push(b"[]");
        assert_eq!(sampler.snapshot(), final_snapshot);
        invariant(&final_snapshot);
    }
    #[test]
    fn recovery_byte_belongs_to_new_candidate_once() {
        let s = splits(b"noise {\"a\":1{} tail", Config::default());
        assert_eq!(
            (
                s.stream.confirmed_bytes,
                s.stream.rejected_bytes,
                s.stream.outside_bytes
            ),
            (2, 6, 11)
        );
        assert_eq!(
            (s.stream.candidates_confirmed, s.stream.candidates_rejected),
            (1, 1)
        );
        assert_eq!(s.metrics.len(), 1);
    }
    #[test]
    fn rejection_and_eof_corpus() {
        for input in [
            &b""[..],
            b"words 123 true null",
            b"{} []",
            b"{",
            b"[1",
            b"[tru",
            b"[1e]{}",
            b"[01]{}",
            b"[\"\\q\"]{}",
            b"[\"\\uXX00\"]{}",
            b"[\"\xff\"]{}",
            b"[\"\xc3",
            b"{\"a\":null,\"b\":false}garbage",
        ] {
            let s = splits(input, Config::default());
            assert_eq!(s.stream.consumed_bytes, input.len() as u64);
            assert_eq!(s.stream.pending_bytes, 0);
        }
        let s = splits(b"{} [\"\xc3", Config::default());
        assert_eq!(s.stream.candidates_incomplete_at_eof, 1);
        assert_eq!(s.stream.errors["InvalidUtf8"], 1);
        assert_eq!(s.stream.confirmed_bytes, 2);
        assert_eq!(s.stream.rejected_bytes, 3);
    }
    #[test]
    fn candidate_and_parser_depth_limits_recover() {
        let config = Config {
            max_candidate_bytes: NonZeroU64::new(2),
            ..Config::default()
        };
        let s = splits(b"{}[0{}", config);
        assert_eq!(s.stream.confirmed_bytes, 2);
        assert_eq!(s.stream.rejected_bytes, 3);
        assert_eq!(s.stream.outside_bytes, 1);
        assert_eq!(s.stream.errors["CandidateLimitExceeded"], 1);
        let config = Config {
            max_depth: NonZeroUsize::new(1).unwrap(),
            ..Config::default()
        };
        let s = splits(b"[[]]{}", config);
        assert_eq!(s.stream.rejected_bytes, 2);
        assert_eq!(s.stream.outside_bytes, 2);
        assert_eq!(s.stream.errors["DepthLimitExceeded"], 1);
        for (input, limit, confirmed, incomplete) in
            [(&b"{}"[..], 2, 1, 0), (b"[0", 2, 0, 1), (b"{}", 1, 0, 0)]
        {
            let s = splits(
                input,
                Config {
                    max_candidate_bytes: NonZeroU64::new(limit),
                    ..Config::default()
                },
            );
            assert_eq!(s.stream.candidates_confirmed, confirmed);
            assert_eq!(s.stream.candidates_incomplete_at_eof, incomplete);
        }
    }
    #[test]
    fn profile_limits_are_explicit_and_rollback_is_clean() {
        let configs = [
            Config {
                profile_depth: 0,
                ..Config::default()
            },
            Config {
                max_key_bytes: 1,
                ..Config::default()
            },
            Config {
                max_path_bytes: 1,
                ..Config::default()
            },
            Config {
                max_paths: NonZeroUsize::new(1).unwrap(),
                ..Config::default()
            },
        ];
        for config in configs {
            let cap = config.max_paths.get();
            let s = splits(br#"{"long":{"x":[1]},"ok":0} {"bad":1,X}"#, config);
            assert_eq!(s.stream.candidates_profile_limited, 1);
            assert_eq!(s.stream.candidates_confirmed, 1);
            assert!(s.metrics.len() <= cap);
            assert!(!s.metrics.iter().any(|m| m.path.contains(&key("bad"))));
        }
        let s = splits(
            br#"{"long":{"x":1},"a":2}"#,
            Config {
                max_key_bytes: 1,
                ..Config::default()
            },
        );
        assert_eq!(s.metrics.len(), 2);
        assert_eq!(metric(&s, &[key("a")], ValueType::Number).occurrences, 1);
        let s = splits(
            br#"{"a":0}{"b":1}{"a":2}"#,
            Config {
                max_paths: NonZeroUsize::new(2).unwrap(),
                ..Config::default()
            },
        );
        assert_eq!(s.stream.candidates_profile_limited, 1);
        assert_eq!(metric(&s, &[key("a")], ValueType::Number).records, 2);
    }
    #[test]
    fn bounded_volcano_corpus_matches_across_chunks() {
        use json_volcano::{Config as VolcanoConfig, Scenario, Shape};
        for scenario in [Scenario::Valid, Scenario::Invalid, Scenario::Truncated] {
            let mut bytes = Vec::new();
            json_volcano::generate(
                VolcanoConfig {
                    bytes: 256,
                    depth: 3,
                    shape: Shape::Object,
                    scenario,
                    ..VolcanoConfig::default()
                },
                &mut bytes,
            )
            .unwrap();
            let s = splits(&bytes, Config::default());
            if scenario == Scenario::Valid {
                assert!(s.stream.candidates_confirmed > 0);
            } else {
                assert!(s.stream.candidates_rejected > 0);
            }
        }
    }
    #[test]
    fn unpaired_surrogate_keys_are_explicitly_omitted() {
        let s = splits(
            br#"{"\ud800":{"x":1},"ok":2}{"\udc00":0,X}"#,
            Config::default(),
        );
        assert_eq!(s.stream.candidates_confirmed, 1);
        assert_eq!(s.stream.candidates_key_decode_errors, 1);
        assert_eq!(s.stream.candidates_profile_limited, 0);
        assert_eq!(s.metrics.len(), 2);
        assert_eq!(metric(&s, &[key("ok")], ValueType::Number).occurrences, 1);
    }
    #[test]
    fn long_stream_and_large_values_need_only_incremental_input() {
        let mut sampler = Sampler::new(Config {
            max_paths: NonZeroUsize::new(4).unwrap(),
            ..Config::default()
        });
        for _ in 0..1000 {
            sampler.push(br#"noise {"n":1}"#);
        }
        let before = sampler.snapshot();
        assert_eq!(before.stream.candidates_confirmed, 1000);
        assert_eq!(
            metric(&before, &[key("n")], ValueType::Number).records,
            1000
        );
        sampler.push(br#"{"text":""#);
        let block = [b'x'; 4096];
        for _ in 0..128 {
            sampler.push(&block);
        }
        assert_eq!(sampler.snapshot().metrics, before.metrics);
        assert!(sampler.snapshot().stream.pending_bytes > 500_000);
        sampler.push(b"\"}");
        sampler.finish();
        let after = sampler.snapshot();
        assert_eq!(after.stream.candidates_confirmed, 1001);
        assert_eq!(
            metric(&after, &[key("text")], ValueType::String).occurrences,
            1
        );
        assert_eq!(after.stream.candidates_profile_limited, 0);
        invariant(&after);
    }
    #[test]
    fn exact_key_path_and_depth_boundaries() {
        let s = splits(
            br#"{"a":{"b":1}}"#,
            Config {
                profile_depth: 2,
                max_key_bytes: 1,
                max_path_bytes: 2,
                ..Config::default()
            },
        );
        assert_eq!(s.stream.candidates_profile_limited, 0);
        assert_eq!(
            metric(&s, &[key("a"), key("b")], ValueType::Number).occurrences,
            1
        );
        let s = splits(
            br#"{"":0,"x":1}"#,
            Config {
                max_key_bytes: 0,
                max_path_bytes: 0,
                ..Config::default()
            },
        );
        assert_eq!(s.metrics.len(), 2);
        assert_eq!(metric(&s, &[key("")], ValueType::Number).occurrences, 1);
        assert_eq!(s.stream.candidates_profile_limited, 1);
    }
    #[test]
    fn flat_metrics_match_s3_sample_and_preserve_distinct_paths() {
        let s = splits(r#"{"a":{"b":1},"a.b":2,"arr":[1,2],"$_ARRAY_ITEM_$":3,"":null,"x=y":true,"back\\slash":"é"}"#.as_bytes(), Config::default());
        let flat = s.flat_metrics();
        let expected = BTreeMap::from([
            ("paths:a=OBJECT".into(), 1),
            ("paths:a.b=NUMBER".into(), 1),
            ("paths:a\\.b=NUMBER".into(), 1),
            ("paths:arr=ARRAY".into(), 1),
            ("paths:arr.$_ARRAY_ITEM_$=NUMBER".into(), 2),
            ("paths:\\$_ARRAY_ITEM_\\$=NUMBER".into(), 1),
            ("paths:=NULL".into(), 1),
            ("paths:x\\=y=BOOLEAN".into(), 1),
            ("paths:back\\\\slash=STRING".into(), 1),
        ]);
        assert_eq!(flat, expected);
        assert!(Sampler::default().snapshot().flat_metrics().is_empty());
    }
}
