use clap::{Parser, ValueEnum};
use json_sampler::{Config, Sampler};
use std::io::{self, IsTerminal, Read, Write};
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
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
    Human,
    #[value(skip)]
    HumanColor,
    Expanded,
    Flat,
}
#[derive(Clone, Copy)]
struct OutputOptions {
    final_only: bool,
    format: Format,
}
#[cfg(test)]
impl From<bool> for OutputOptions {
    fn from(final_only: bool) -> Self {
        Self {
            final_only,
            format: Format::Flat,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Color {
    Auto,
    Always,
    Never,
}
fn use_color(color: Color, terminal: bool, no_color: bool) -> bool {
    match color {
        Color::Always => true,
        Color::Never => false,
        Color::Auto => terminal && !no_color,
    }
}
fn safe_label(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}
fn colored_path(label: &str) -> String {
    const RESET: &str = "\x1b[0m";
    const BLUE: &str = "\x1b[34m";
    const ORANGE: &str = "\x1b[38;5;208m";
    let safe = safe_label(label);
    let Some((path, kind)) = safe.rsplit_once('=') else {
        return safe;
    };
    let mut result = String::new();
    let mut component = String::new();
    let mut escaped = false;
    let append = |result: &mut String, part: &str| {
        let color = if part == "$_ARRAY_ITEM_$" {
            ORANGE
        } else {
            BLUE
        };
        result.push_str(color);
        result.push_str(part);
        result.push_str(RESET);
    };
    for ch in path.chars() {
        if ch == '.' && !escaped {
            append(&mut result, &component);
            component.clear();
            result.push('.');
        } else {
            component.push(ch);
        }
        escaped = ch == '\\' && !escaped;
    }
    append(&mut result, &component);
    result.push('=');
    let color = match kind {
        "OBJECT" => BLUE,
        "ARRAY" | "NUMBER" => ORANGE,
        "STRING" => "\x1b[32m",
        "BOOLEAN" => "\x1b[35m",
        "NULL" => "\x1b[33m",
        _ => "",
    };
    result.push_str(color);
    result.push_str(kind);
    result.push_str(RESET);
    result
}

fn rows(
    writer: &mut impl Write,
    title: &str,
    rows: &[(String, u64)],
    color: bool,
    path_colors: bool,
) -> io::Result<()> {
    let (heading, number, reset) = if color {
        ("\x1b[1;36m", "\x1b[38;5;208m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    writeln!(writer, "{heading}{title}{reset}")?;
    if rows.is_empty() {
        writeln!(writer, "  (none)")?;
    }
    let width = rows
        .iter()
        .map(|(_, n)| n.to_string().len())
        .max()
        .unwrap_or(1);
    for (label, count) in rows {
        writeln!(
            writer,
            "  {number}{count:>width$}{reset} : {}",
            if color && path_colors {
                colored_path(label)
            } else {
                safe_label(label)
            }
        )?;
    }
    Ok(())
}
fn human_report(
    writer: &mut impl Write,
    snapshot: &json_sampler::Snapshot,
    color: bool,
    stop_reason: Option<&str>,
) -> io::Result<()> {
    human_report_parts(writer, snapshot, color, stop_reason, true)
}
fn human_report_parts(
    writer: &mut impl Write,
    snapshot: &json_sampler::Snapshot,
    color: bool,
    stop_reason: Option<&str>,
    include_paths: bool,
) -> io::Result<()> {
    let status = if stop_reason.is_some() {
        "interrupted"
    } else if snapshot.finished {
        "complete"
    } else {
        "live"
    };
    writeln!(writer, "JSON sampler ({status})")?;
    if include_paths {
        let counts = snapshot
            .flat_metrics()
            .into_iter()
            .map(|(path, n)| (path.strip_prefix("paths:").unwrap_or(&path).to_owned(), n))
            .collect::<Vec<_>>();
        rows(writer, "Path counts", &counts, color, true)?;
        writeln!(writer)?;
    }
    let stream = serde_json::to_value(&snapshot.stream)?;
    let mut metrics = Vec::new();
    for (key, value) in stream
        .as_object()
        .expect("stream statistics serialize as an object")
    {
        if let Some(count) = value.as_u64() {
            metrics.push((key.replace('_', " "), count));
        }
    }
    for (kind, count) in &snapshot.stream.errors {
        metrics.push((format!("errors: {kind}"), *count));
    }
    rows(writer, "Stream metrics", &metrics, color, false)?;
    writeln!(
        writer,
        "  pending depth truncated : {}",
        if snapshot.stream.pending_depth_truncated {
            "yes"
        } else {
            "no"
        }
    )?;
    writeln!(writer)?;
    writer.flush()
}

#[derive(Parser)]
#[command(
    version,
    about = "Profile every JSON object/array candidate in a mixed stdin stream"
)]
struct Args {
    /// Stdout format without --summary-file: human (default), flat JSON, or expanded JSON.
    #[arg(long, value_enum, default_value = "human")]
    format: Format,
    /// Color human reports automatically on terminals, always, or never.
    #[arg(long, value_enum, default_value = "auto")]
    color: Color,
    /// Seconds between cumulative JSON snapshots, including while input is idle.
    #[arg(long, default_value = "5", value_parser = interval)]
    interval: Duration,
    /// Write full expanded JSON reports to this file; stdout shows readable stream metrics.
    #[arg(long)]
    summary_file: Option<PathBuf>,
    /// Emit only the final report at EOF or SIGINT.
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
    if matches!(format, Format::Human | Format::HumanColor) {
        return human_report(
            writer,
            &snapshot,
            matches!(format, Format::HumanColor),
            stop_reason,
        );
    }
    #[derive(serde::Serialize)]
    struct Report<'a, M> {
        finished: bool,
        stream: &'a json_sampler::StreamStats,
        metrics: M,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<&'a str>,
    }
    match format {
        Format::Human | Format::HumanColor => unreachable!(),
        Format::Expanded => serde_json::to_writer(
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
trait ReportSink {
    fn emit(&mut self, sampler: &Sampler, format: Format, reason: Option<&str>) -> io::Result<()>;
}
impl<W: Write> ReportSink for W {
    fn emit(&mut self, sampler: &Sampler, format: Format, reason: Option<&str>) -> io::Result<()> {
        write_report(self, sampler, format, reason)
    }
}
struct FileReports<F, T> {
    file: F,
    terminal: T,
    terminal_color: bool,
    terminal_closed: bool,
}
fn file_error(error: io::Error) -> io::Error {
    // File failures are fatal even when their underlying kind is BrokenPipe.
    io::Error::other(format!("summary file: {error}"))
}
impl<F: Write, T: Write> ReportSink for FileReports<F, T> {
    fn emit(&mut self, sampler: &Sampler, _format: Format, reason: Option<&str>) -> io::Result<()> {
        write_report(&mut self.file, sampler, Format::Expanded, reason).map_err(file_error)?;
        if !self.terminal_closed {
            if let Err(error) = human_report_parts(
                &mut self.terminal,
                &sampler.snapshot(),
                self.terminal_color,
                reason,
                false,
            ) {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    self.terminal_closed = true;
                } else {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
fn report(writer: &mut impl Write, sampler: &Sampler) -> io::Result<()> {
    write_report(writer, sampler, Format::Expanded, None)
}
fn consume(
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    writer: &mut impl ReportSink,
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
    writer: &mut impl ReportSink,
    sampler: &mut Sampler,
    interval: Duration,
    options: impl Into<OutputOptions>,
) -> io::Result<Stop> {
    let OutputOptions { final_only, format } = options.into();
    let mut deadline = now() + interval;
    loop {
        if interrupted() {
            writer.emit(sampler, format, Some("sigint"))?;
            return Ok(Stop::Interrupted);
        }
        if !final_only && now() >= deadline {
            writer.emit(sampler, format, None)?;
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
                writer.emit(sampler, format, None)?;
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
    let summary_file = args
        .summary_file
        .as_ref()
        .map(std::fs::File::create)
        .transpose()
        .map_err(file_error)?;
    let terminal_color = use_color(
        args.color,
        io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some_and(|s| !s.is_empty()),
    );
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
    let options = OutputOptions {
        final_only: args.final_only,
        format: if matches!(args.format, Format::Human) && terminal_color {
            Format::HumanColor
        } else {
            args.format
        },
    };
    let mut stdout = io::stdout().lock();
    if let Some(file) = summary_file {
        let mut reports = FileReports {
            file,
            terminal: &mut stdout,
            terminal_color,
            terminal_closed: false,
        };
        consume(receiver, &mut reports, &mut sampler, args.interval, options)
    } else {
        consume(receiver, &mut stdout, &mut sampler, args.interval, options)
    }
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
            "\"candidates_profile_limited\":0,\"candidates_key_decode_errors\":0,\"max_depth_encountered\":0,\"candidates_depth_truncated\":0,\"pending_max_depth\":0,\"pending_depth_truncated\":false,\"errors\":{\"UnexpectedEnd\":1}},",
            "\"metrics\":{}}\n"
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
            assert!(report["metrics"].as_object().unwrap().is_empty());
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
            Format::Human
        ));
        assert!(matches!(
            Args::try_parse_from(["json-sampler", "--format", "expanded"])
                .unwrap()
                .format,
            Format::Expanded
        ));
        let mut expanded = vec![];
        write_report(&mut expanded, &sampler, Format::Expanded, None).unwrap();
        let expanded: serde_json::Value = serde_json::from_slice(&expanded).unwrap();
        assert!(expanded["metrics"].is_array());
        assert!(Args::try_parse_from(["json-sampler", "--format", "unknown"]).is_err());
    }
    #[test]
    fn readable_rows_align_counts_and_escape_controls() {
        let rows_data = vec![
            ("a=NUMBER".into(), 2),
            ("b=STRING".into(), 1000),
            ("bad\n\x1b".into(), 0),
        ];
        let mut plain = vec![];
        rows(&mut plain, "Path counts", &rows_data, false, false).unwrap();
        assert_eq!(
            String::from_utf8(plain.clone()).unwrap(),
            "Path counts\n     2 : a=NUMBER\n  1000 : b=STRING\n     0 : bad\\n\\u{1b}\n"
        );
        let mut colored = vec![];
        rows(&mut colored, "Path counts", &rows_data, true, false).unwrap();
        let colored = String::from_utf8(colored).unwrap();
        assert!(colored.starts_with("\x1b[1;36mPath counts\x1b[0m\n"));
        assert!(colored.contains("\x1b[38;5;208m   2\x1b[0m : a=NUMBER"));
        assert_eq!(
            colored
                .replace("\x1b[1;36m", "")
                .replace("\x1b[38;5;208m", "")
                .replace("\x1b[0m", "")
                .as_bytes(),
            plain
        );
        assert!(use_color(Color::Auto, true, false));
        assert!(!use_color(Color::Auto, false, false));
        assert!(!use_color(Color::Auto, true, true));
        assert!(use_color(Color::Always, false, true));
        assert!(!use_color(Color::Never, true, false));
    }
    #[test]
    fn readable_reports_put_paths_before_separated_stream_metrics() {
        let mut sampler = Sampler::default();
        sampler.push(br#"{"a":[1,2]}["#);
        for (format, reason, status) in [
            (Format::Human, None, "live"),
            (Format::HumanColor, Some("sigint"), "interrupted"),
        ] {
            let mut writer = Writer {
                bytes: vec![],
                fail: None,
                flush_fail: false,
            };
            write_report(&mut writer, &sampler, format, reason).unwrap();
            let text = String::from_utf8(writer.bytes).unwrap();
            assert!(text.starts_with(&format!("JSON sampler ({status})\n")));
            assert!(text.find("Path counts").unwrap() < text.find("Stream metrics").unwrap());
            let plain = [
                "\x1b[0m",
                "\x1b[34m",
                "\x1b[38;5;208m",
                "\x1b[32m",
                "\x1b[35m",
                "\x1b[33m",
                "\x1b[1;36m",
            ]
            .iter()
            .fold(text.clone(), |text, code| text.replace(code, ""));
            assert!(plain.contains("a.$_ARRAY_ITEM_$=NUMBER"));
            assert!(text.contains("\n\n"));
            assert!(text.replace("\x1b[0m", "").contains("1 : pending bytes"));
            for fail in [Some(io::ErrorKind::BrokenPipe), None] {
                let mut writer = Writer {
                    bytes: vec![],
                    fail,
                    flush_fail: fail.is_none(),
                };
                assert!(write_report(&mut writer, &sampler, format, reason).is_err());
            }
        }
        sampler.finish();
        let mut output = vec![];
        write_report(&mut output, &sampler, Format::Human, None).unwrap();
        assert!(output.starts_with(b"JSON sampler (complete)\n"));
        assert!(!output.contains(&0x1b));
    }
    #[test]
    fn path_components_and_types_use_consistent_colors_with_plain_separators() {
        for (kind, color) in [
            ("OBJECT", "\x1b[34m"),
            ("ARRAY", "\x1b[38;5;208m"),
            ("NUMBER", "\x1b[38;5;208m"),
            ("STRING", "\x1b[32m"),
            ("BOOLEAN", "\x1b[35m"),
            ("NULL", "\x1b[33m"),
        ] {
            assert_eq!(
                colored_path(&format!("a.$_ARRAY_ITEM_$.b={kind}")),
                format!(
                    "\x1b[34ma\x1b[0m.\x1b[38;5;208m$_ARRAY_ITEM_$\x1b[0m.\x1b[34mb\x1b[0m={color}{kind}\x1b[0m"
                )
            );
        }
        assert_eq!(
            colored_path(r"a\.b.\$_ARRAY_ITEM_\$=STRING"),
            "\x1b[34ma\\.b\x1b[0m.\x1b[34m\\$_ARRAY_ITEM_\\$\x1b[0m=\x1b[32mSTRING\x1b[0m"
        );
        let unsafe_key = colored_path("a\n\x1b=STRING");
        assert!(unsafe_key.contains(r"a\n\u{1b}"));
    }
    #[test]
    fn depth_reporting_distinguishes_confirmed_and_pending_truncation() {
        let mut sampler = Sampler::new(Config {
            profile_depth: 1,
            ..Config::default()
        });
        sampler.push(b"[[0");
        let mut text = vec![];
        write_report(&mut text, &sampler, Format::Human, None).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("2 : pending max depth"));
        assert!(text.contains("0 : max depth encountered"));
        assert!(text.contains("pending depth truncated : yes"));
        sampler.push(b"]]");
        let mut text = vec![];
        write_report(&mut text, &sampler, Format::Human, None).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("2 : max depth encountered"));
        assert!(text.contains("1 : candidates depth truncated"));
        assert!(text.contains("pending depth truncated : no"));
    }
    #[test]
    fn human_report_preserves_keys_starting_with_metric_namespace() {
        let mut sampler = Sampler::default();
        sampler.push(br#"{"paths:x":1,"x":2}"#);
        sampler.finish();
        let mut output = vec![];
        write_report(&mut output, &sampler, Format::Human, None).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("1 : paths:x=NUMBER\n"));
        assert!(output.contains("1 : x=NUMBER\n"));
    }
    #[test]
    fn summary_file_gets_full_reports_and_terminal_only_stream_metrics() {
        for format in [
            Format::Human,
            Format::HumanColor,
            Format::Flat,
            Format::Expanded,
        ] {
            let mut sampler = Sampler::default();
            sampler.push(br#"{"a":[1,2]}["#);
            let mut reports = FileReports {
                file: Writer {
                    bytes: vec![],
                    fail: None,
                    flush_fail: false,
                },
                terminal: vec![],
                terminal_color: true,
                terminal_closed: false,
            };
            reports.emit(&sampler, format, Some("sigint")).unwrap();
            let terminal = String::from_utf8(reports.terminal).unwrap();
            assert!(terminal.contains("Stream metrics"));
            assert!(terminal.contains("pending bytes"));
            assert!(!terminal.contains("Path counts"));
            assert!(!terminal.contains("a.$_ARRAY_ITEM_$"));
            assert!(terminal.contains("\x1b["));
            assert!(!reports.file.bytes.contains(&0x1b));
            let file = String::from_utf8(reports.file.bytes).unwrap();
            assert!(file.contains("a"));
            let value: serde_json::Value = serde_json::from_str(&file).unwrap();
            assert_eq!(value["stop_reason"], "sigint");
            assert_eq!(value["stream"]["pending_bytes"], 1);
            assert_eq!(value["finished"], false);
            let metrics = value["metrics"].as_array().unwrap();
            assert_eq!(metrics.len(), 3);
            let number = metrics
                .iter()
                .find(|metric| metric["value_type"] == "number")
                .unwrap();
            assert_eq!(number["counts"]["occurrences"], 2);
            assert_eq!(number["counts"]["records"], 1);
            assert_eq!(file.lines().count(), 1);
        }
        let args = Args::try_parse_from([
            "json-sampler",
            "--summary-file",
            "report.txt",
            "--final-only",
        ])
        .unwrap();
        assert_eq!(args.summary_file, Some(PathBuf::from("report.txt")));
        assert!(args.final_only);
    }
    #[test]
    fn summary_file_obeys_periodic_and_final_only_modes() {
        use std::cell::Cell;
        for final_only in [false, true] {
            let clock = Cell::new(Instant::now());
            let mut step = 0;
            let mut reports = FileReports {
                file: vec![],
                terminal: vec![],
                terminal_color: false,
                terminal_closed: false,
            };
            consume_with(
                |_| {
                    step += 1;
                    match step {
                        1 => Ok(Ok(br#"{"a":1}"#.to_vec())),
                        2 => {
                            clock.set(clock.get() + Duration::from_secs(10));
                            Err(RecvTimeoutError::Timeout)
                        }
                        _ => Err(RecvTimeoutError::Disconnected),
                    }
                },
                || clock.get(),
                || false,
                &mut reports,
                &mut Sampler::default(),
                Duration::from_secs(5),
                OutputOptions {
                    final_only,
                    format: Format::Expanded,
                },
            )
            .unwrap();
            let values = serde_json::Deserializer::from_slice(&reports.file)
                .into_iter::<serde_json::Value>()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(values.len(), if final_only { 1 } else { 2 });
            assert_eq!(values.last().unwrap()["finished"], true);
            let terminal = String::from_utf8(reports.terminal).unwrap();
            assert_eq!(terminal.matches("Stream metrics").count(), values.len());
            assert!(!terminal.contains("Path counts"));
        }
    }
    #[test]
    fn summary_file_survives_terminal_broken_pipe_and_propagates_failures() {
        let mut sampler = Sampler::default();
        sampler.push(b"{}");
        let mut reports = FileReports {
            file: vec![],
            terminal: Writer {
                bytes: vec![],
                fail: Some(io::ErrorKind::BrokenPipe),
                flush_fail: false,
            },
            terminal_color: false,
            terminal_closed: false,
        };
        reports.emit(&sampler, Format::Expanded, None).unwrap();
        sampler.finish();
        reports.emit(&sampler, Format::Expanded, None).unwrap();
        assert_eq!(reports.file.iter().filter(|&&b| b == b'\n').count(), 2);
        for fail in [
            Some(io::ErrorKind::BrokenPipe),
            Some(io::ErrorKind::PermissionDenied),
            None,
        ] {
            let mut reports = FileReports {
                file: Writer {
                    bytes: vec![],
                    fail,
                    flush_fail: fail.is_none(),
                },
                terminal: vec![],
                terminal_color: false,
                terminal_closed: false,
            };
            let error = reports.emit(&sampler, Format::Expanded, None).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::BrokenPipe);
            assert!(reports.terminal.is_empty());
        }
        let mut reports = FileReports {
            file: vec![],
            terminal: Writer {
                bytes: vec![],
                fail: None,
                flush_fail: true,
            },
            terminal_color: false,
            terminal_closed: false,
        };
        assert!(reports.emit(&sampler, Format::Expanded, None).is_err());
        assert!(!reports.file.is_empty());
    }
}
