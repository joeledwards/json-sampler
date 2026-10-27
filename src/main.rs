use clap::{Parser, ValueEnum};
use json_sampler::{Config, Sampler};
use std::io::{self, Read, Write};
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Eof,
    Interrupted,
}
const INTERRUPT_POLL: Duration = Duration::from_millis(100);

#[cfg(unix)]
mod interrupt {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REQUESTED: AtomicBool = AtomicBool::new(false);
    extern "C" fn handle(_: libc::c_int) {
        // No allocation, locks, output, or parser work in the signal handler.
        REQUESTED.store(true, Ordering::Relaxed);
    }
    pub fn requested() -> bool {
        REQUESTED.load(Ordering::Relaxed)
    }
    pub fn install() -> std::io::Result<()> {
        // SAFETY: sigaction is initialized before use; handler has C ABI and
        // process lifetime. Installed before the reader thread is spawned.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}
#[cfg(not(unix))]
mod interrupt {
    pub fn requested() -> bool {
        false
    }
    pub fn install() -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum Format {
    #[default]
    Structured,
    Flat,
}
#[derive(Clone, Copy)]
struct OutputOptions {
    final_only: bool,
    format: Format,
}
impl From<bool> for OutputOptions {
    fn from(final_only: bool) -> Self {
        Self {
            final_only,
            format: Format::Structured,
        }
    }
}

#[derive(Parser)]
#[command(
    version,
    about = "Profile every JSON object/array candidate in a mixed stdin stream"
)]
struct Args {
    /// Collection format: structured path/type entries or S3-sample-style counts.
    #[arg(long, value_enum, default_value = "structured")]
    format: Format,
    /// Seconds between cumulative JSON snapshots, including while input is idle.
    #[arg(long, default_value = "5", value_parser = interval)]
    interval: Duration,
    /// Emit only the final snapshot at EOF.
    #[arg(long)]
    final_only: bool,
    #[arg(long, default_value = "1000")]
    max_depth: NonZeroUsize,
    #[arg(long)]
    max_candidate_bytes: Option<NonZeroU64>,
    /// Root is depth zero; deeper values are validated but not profiled.
    #[arg(long, default_value = "32")]
    profile_depth: usize,
    /// Maximum path/type entries in each tentative and confirmed map.
    #[arg(long, default_value = "10000")]
    max_paths: NonZeroUsize,
    #[arg(long, default_value = "4096")]
    max_key_bytes: usize,
    #[arg(long, default_value = "16384")]
    max_path_bytes: usize,
}
fn interval(s: &str) -> Result<Duration, String> {
    let seconds: f64 = s.parse().map_err(|_| "expected positive seconds")?;
    if !seconds.is_finite() || seconds <= 0.0 || seconds > 86400.0 {
        return Err("interval must be positive and at most 86400 seconds".into());
    }
    let duration = Duration::try_from_secs_f64(seconds).map_err(|e| e.to_string())?;
    if duration.is_zero() {
        return Err("interval is too small".into());
    }
    Ok(duration)
}
fn write_report(
    writer: &mut impl Write,
    sampler: &Sampler,
    format: Format,
    stop_reason: Option<&str>,
) -> io::Result<()> {
    let snapshot = sampler.snapshot();
    #[derive(serde::Serialize)]
    struct Report<'a, M> {
        finished: bool,
        stream: &'a json_sampler::StreamStats,
        metrics: M,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<&'a str>,
    }
    match format {
        Format::Structured => serde_json::to_writer(
            &mut *writer,
            &Report {
                finished: snapshot.finished,
                stream: &snapshot.stream,
                metrics: &snapshot.metrics,
                stop_reason,
            },
        )?,
        Format::Flat => serde_json::to_writer(
            &mut *writer,
            &Report {
                finished: snapshot.finished,
                stream: &snapshot.stream,
                metrics: snapshot.flat_metrics(),
                stop_reason,
            },
        )?,
    }
    writer.write_all(b"\n")?;
    writer.flush()
}
#[cfg(test)]
fn report(writer: &mut impl Write, sampler: &Sampler) -> io::Result<()> {
    write_report(writer, sampler, Format::Structured, None)
}
fn consume(
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    writer: &mut impl Write,
    sampler: &mut Sampler,
    interval: Duration,
    options: impl Into<OutputOptions>,
) -> io::Result<Stop> {
    consume_with(
        |timeout| match timeout {
            Some(timeout) => receiver.recv_timeout(timeout),
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        },
        Instant::now,
        interrupt::requested,
        writer,
        sampler,
        interval,
        options,
    )
}
fn consume_with(
    mut receive: impl FnMut(Option<Duration>) -> Result<io::Result<Vec<u8>>, RecvTimeoutError>,
    mut now: impl FnMut() -> Instant,
    mut interrupted: impl FnMut() -> bool,
    writer: &mut impl Write,
    sampler: &mut Sampler,
    interval: Duration,
    options: impl Into<OutputOptions>,
) -> io::Result<Stop> {
    let OutputOptions { final_only, format } = options.into();
    let mut deadline = now() + interval;
    loop {
        if interrupted() {
            write_report(writer, sampler, format, Some("sigint"))?;
            return Ok(Stop::Interrupted);
        }
        if !final_only && now() >= deadline {
            write_report(writer, sampler, format, None)?;
            deadline = now() + interval;
        }
        // Bound waits even in --final-only mode so a blocked reader cannot
        // delay SIGINT reporting until more data or EOF arrives.
        let timeout = Some(if final_only {
            INTERRUPT_POLL
        } else {
            INTERRUPT_POLL.min(deadline.saturating_duration_since(now()))
        });
        match receive(timeout) {
            Ok(Ok(bytes)) => sampler.push(&bytes),
            Ok(Err(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                sampler.finish();
                write_report(writer, sampler, format, None)?;
                return Ok(Stop::Eof);
            }
        }
    }
}
fn run(args: Args) -> io::Result<Stop> {
    interrupt::install()?;
    let mut sampler = Sampler::new(Config {
        max_depth: args.max_depth,
        max_candidate_bytes: args.max_candidate_bytes,
        profile_depth: args.profile_depth,
        max_paths: args.max_paths,
        max_key_bytes: args.max_key_bytes,
        max_path_bytes: args.max_path_bytes,
    });
    // Bounded read-ahead. A stdin reader blocked at shutdown is detached;
    // process exit releases it, including when downstream closes its pipe.
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut bytes = vec![0; 64 * 1024];
            let message = match stdin.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.truncate(n);
                    Ok(bytes)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => Err(e),
            };
            let failed = message.is_err();
            if sender.send(message).is_err() || failed {
                break;
            }
        }
    });
    consume(
        receiver,
        &mut io::stdout().lock(),
        &mut sampler,
        args.interval,
        OutputOptions {
            final_only: args.final_only,
            format: args.format,
        },
    )
}
fn main() -> std::process::ExitCode {
    match run(Args::parse()) {
        Ok(Stop::Eof) => std::process::ExitCode::SUCCESS,
        Ok(Stop::Interrupted) => std::process::ExitCode::from(130),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("json-sampler: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn intervals_are_bounded() {
        for invalid in ["0", "-1", "NaN", "inf", "86401", "1e-100"] {
            assert!(interval(invalid).is_err(), "{invalid}");
        }
        assert_eq!(interval("0.5").unwrap(), Duration::from_millis(500));
    }
    struct Writer {
        bytes: Vec<u8>,
        fail: Option<io::ErrorKind>,
        flush_fail: bool,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(kind) = self.fail {
                return Err(kind.into());
            }
            let n = bytes.len().min(3);
            self.bytes.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.flush_fail {
                Err(io::ErrorKind::Other.into())
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn final_report_handles_partial_writes_and_eof() {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(b"x{}[".to_vec())).unwrap();
        drop(tx);
        let mut writer = Writer {
            bytes: vec![],
            fail: None,
            flush_fail: false,
        };
        consume(
            rx,
            &mut writer,
            &mut Sampler::default(),
            Duration::from_secs(1),
            true,
        )
        .unwrap();
        let expected = concat!(
            "{\"finished\":true,\"stream\":{\"consumed_bytes\":4,\"outside_bytes\":1,",
            "\"confirmed_bytes\":2,\"rejected_bytes\":1,\"pending_bytes\":0,",
            "\"candidates_started\":2,\"candidates_confirmed\":1,\"candidates_rejected\":1,",
            "\"candidates_pending\":0,\"candidates_incomplete_at_eof\":1,",
            "\"candidates_profile_limited\":0,\"candidates_key_decode_errors\":0,\"errors\":{\"UnexpectedEnd\":1}},",
            "\"metrics\":[{\"path\":[],\"value_type\":\"object\",\"counts\":{\"occurrences\":1,\"records\":1}}]}\n"
        );
        assert_eq!(String::from_utf8(writer.bytes).unwrap(), expected);
    }
    #[test]
    fn io_errors_propagate() {
        for fail in [
            Some(io::ErrorKind::BrokenPipe),
            Some(io::ErrorKind::PermissionDenied),
            None,
        ] {
            let mut writer = Writer {
                bytes: vec![],
                fail,
                flush_fail: fail.is_none(),
            };
            assert!(report(&mut writer, &Sampler::default()).is_err());
        }
        let (tx, rx) = mpsc::channel();
        tx.send(Err(io::ErrorKind::UnexpectedEof.into())).unwrap();
        drop(tx);
        let mut sampler = Sampler::default();
        assert_eq!(
            consume(rx, &mut vec![], &mut sampler, Duration::from_secs(1), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(!sampler.snapshot().finished);
    }
    #[test]
    fn periodic_snapshots_include_pending_input_and_idle_time() {
        use std::cell::Cell;
        let clock = Cell::new(Instant::now());
        let tick = Duration::from_secs(5);
        let mut step = 0;
        let mut output = Vec::new();
        consume_with(
            |timeout| {
                assert!(timeout.is_some());
                step += 1;
                match step {
                    1 => Ok(Ok(b"{}[".to_vec())),
                    2 => {
                        clock.set(clock.get() + tick);
                        Err(RecvTimeoutError::Timeout)
                    }
                    3 => {
                        clock.set(clock.get() + tick);
                        Err(RecvTimeoutError::Timeout)
                    }
                    _ => Err(RecvTimeoutError::Disconnected),
                }
            },
            || clock.get(),
            || false,
            &mut output,
            &mut Sampler::default(),
            tick,
            false,
        )
        .unwrap();
        let lines: Vec<serde_json::Value> = output
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        for line in &lines[..2] {
            assert_eq!(line["finished"], false);
            assert_eq!(line["stream"]["candidates_confirmed"], 1);
            assert_eq!(line["stream"]["pending_bytes"], 1);
        }
        assert_eq!(lines[2]["finished"], true);
        assert_eq!(lines[2]["stream"]["candidates_incomplete_at_eof"], 1);
    }
    #[test]
    fn interrupt_reports_pending_without_eof_and_ignores_queued_data() {
        use std::cell::Cell;
        for final_only in [false, true] {
            let requested = Cell::new(false);
            let mut output = vec![];
            let mut sampler = Sampler::default();
            let stop = consume_with(
                |timeout| {
                    assert!(timeout.unwrap() <= INTERRUPT_POLL);
                    requested.set(true);
                    Ok(Ok(b"{}[1".to_vec()))
                },
                Instant::now,
                || requested.get(),
                &mut output,
                &mut sampler,
                Duration::from_secs(3600),
                final_only,
            )
            .unwrap();
            assert_eq!(stop, Stop::Interrupted);
            let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(report["stop_reason"], "sigint");
            assert_eq!(report["finished"], false);
            assert_eq!(report["stream"]["candidates_confirmed"], 1);
            assert_eq!(report["stream"]["pending_bytes"], 2);
            assert_eq!(report["stream"]["candidates_rejected"], 0);
            assert_eq!(report["stream"]["candidates_incomplete_at_eof"], 0);
            assert_eq!(report["metrics"].as_array().unwrap().len(), 1);
        }
    }
    #[test]
    fn idle_interrupt_reports_and_writer_failures_propagate() {
        use std::cell::Cell;
        let requested = Cell::new(false);
        let mut output = vec![];
        let stop = consume_with(
            |timeout| {
                assert_eq!(timeout, Some(INTERRUPT_POLL));
                requested.set(true);
                Err(RecvTimeoutError::Timeout)
            },
            Instant::now,
            || requested.get(),
            &mut output,
            &mut Sampler::default(),
            Duration::from_secs(3600),
            true,
        )
        .unwrap();
        assert_eq!(stop, Stop::Interrupted);
        assert_eq!(output.iter().filter(|&&b| b == b'\n').count(), 1);
        for fail in [
            Some(io::ErrorKind::BrokenPipe),
            Some(io::ErrorKind::PermissionDenied),
            None,
        ] {
            let mut writer = Writer {
                bytes: vec![],
                fail,
                flush_fail: fail.is_none(),
            };
            let error = consume_with(
                |_| panic!("must not read after interrupt"),
                Instant::now,
                || true,
                &mut writer,
                &mut Sampler::default(),
                Duration::from_secs(1),
                true,
            )
            .unwrap_err();
            assert_eq!(error.kind(), fail.unwrap_or(io::ErrorKind::Other));
        }
    }
    #[test]
    fn flat_reports_have_exact_sorted_counts_and_preserve_interrupt_state() {
        let mut sampler = Sampler::default();
        sampler.push(br#"{"a":[1,2]}[3"#);
        let mut writer = Writer {
            bytes: vec![],
            fail: None,
            flush_fail: false,
        };
        let stop = consume_with(
            |_| panic!("interrupted before reading"),
            Instant::now,
            || true,
            &mut writer,
            &mut sampler,
            Duration::from_secs(1),
            OutputOptions {
                final_only: true,
                format: Format::Flat,
            },
        )
        .unwrap();
        assert_eq!(stop, Stop::Interrupted);
        let text = String::from_utf8(writer.bytes).unwrap();
        assert!(
            text.contains(r#""metrics":{"paths:a.$_ARRAY_ITEM_$=NUMBER":2,"paths:a=ARRAY":1}"#)
        );
        let report: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["finished"], false);
        assert_eq!(report["stop_reason"], "sigint");
        assert_eq!(report["stream"]["pending_bytes"], 2);
        sampler.finish();
        let mut bytes = vec![];
        write_report(&mut bytes, &sampler, Format::Flat, None).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(report["finished"], true);
        assert!(report.get("stop_reason").is_none());
        assert_eq!(report["stream"]["candidates_rejected"], 1);
        assert_eq!(report["metrics"].as_object().unwrap().len(), 2);
        for fail in [
            Some(io::ErrorKind::BrokenPipe),
            Some(io::ErrorKind::PermissionDenied),
            None,
        ] {
            let mut writer = Writer {
                bytes: vec![],
                fail,
                flush_fail: fail.is_none(),
            };
            assert!(write_report(&mut writer, &sampler, Format::Flat, None).is_err());
        }
        let args = Args::try_parse_from(["json-sampler", "--format", "flat"]).unwrap();
        assert!(matches!(args.format, Format::Flat));
        assert!(matches!(
            Args::try_parse_from(["json-sampler"]).unwrap().format,
            Format::Structured
        ));
        assert!(Args::try_parse_from(["json-sampler", "--format", "unknown"]).is_err());
    }
}
