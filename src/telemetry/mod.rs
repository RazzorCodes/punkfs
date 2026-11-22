//! Local, structured diagnostics and request correlation.
//!
//! This module deliberately does not install a global subscriber or export
//! anything over the network. Applications may initialize a logger; libraries
//! can also use an explicitly supplied [`Logger`].
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tonic::{Request, Status};
use uuid::{Uuid, Variant, Version};

pub const CORRELATION_HEADER: &str = "x-punkfs-correlation-id";
const MAX_EVENT_BYTES: usize = 64 * 1024;

/// Numerical values follow the OpenTelemetry severity ranges. Critical is
/// distinct from fatal within the OTel FATAL range.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Severity {
    Trace = 1,
    Debug = 5,
    Info = 9,
    Warning = 13,
    Error = 17,
    Critical = 21,
    Fatal = 24,
}

impl std::str::FromStr for Severity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warning" | "warn" => Ok(Self::Warning),
            "error" => Ok(Self::Error),
            "critical" => Ok(Self::Critical),
            "fatal" => Ok(Self::Fatal),
            _ => Err(format!("invalid telemetry severity: {s}")),
        }
    }
}

/// Explicitly passed diagnostic identity for one incoming operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationContext {
    id: Uuid,
}

impl Default for OperationContext {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationContext {
    pub fn new() -> Self {
        Self { id: Uuid::new_v4() }
    }
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Validate remote metadata, or create a new identity for a direct client.
    pub fn from_request<T>(request: &Request<T>) -> Result<Self, Status> {
        let mut values = request.metadata().get_all(CORRELATION_HEADER).iter();
        let Some(value) = values.next() else {
            return Ok(Self::new());
        };
        if values.next().is_some() {
            return Err(Status::invalid_argument("duplicate correlation id"));
        }
        let text = value
            .to_str()
            .map_err(|_| Status::invalid_argument("invalid correlation id"))?;
        if text.len() != 36 {
            return Err(Status::invalid_argument("invalid correlation id"));
        }
        let id = Uuid::parse_str(text)
            .map_err(|_| Status::invalid_argument("invalid correlation id"))?;
        if id.get_version() != Some(Version::Random)
            || id.get_variant() != Variant::RFC4122
            || id.to_string() != text
        {
            return Err(Status::invalid_argument("invalid correlation id"));
        }
        Ok(Self { id })
    }

    pub fn response<T>(&self, value: T) -> tonic::Response<T> {
        let mut response = tonic::Response::new(value);
        response.metadata_mut().insert(
            CORRELATION_HEADER,
            self.id.to_string().parse().expect("UUID is valid metadata"),
        );
        response.extensions_mut().insert(self.clone());
        response
    }

    pub fn request<T>(&self, value: T) -> Request<T> {
        let mut request = Request::new(value);
        // Canonical UUIDs are valid ASCII metadata.
        request.metadata_mut().insert(
            CORRELATION_HEADER,
            self.id.to_string().parse().expect("UUID is valid metadata"),
        );
        request.extensions_mut().insert(self.clone());
        request
    }
}

/// A structured diagnostic event. Callers must not include secrets or content.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub timestamp_unix_ms: u128,
    pub severity: Severity,
    pub severity_number: u8,
    pub message: String,
    pub component: String,
    pub operation: Option<String>,
    pub correlation_id: Option<String>,
    pub service: String,
    pub process_id: u32,
    pub console: bool,
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl Event {
    pub fn new(
        severity: Severity,
        component: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            timestamp_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            severity,
            severity_number: severity as u8,
            message: message.into(),
            component: component.into(),
            operation: None,
            correlation_id: None,
            service: String::new(),
            process_id: std::process::id(),
            console: false,
            fields: BTreeMap::new(),
        }
    }
    pub fn with_context(
        mut self,
        context: &OperationContext,
        operation: impl Into<String>,
    ) -> Self {
        self.correlation_id = Some(context.id.to_string());
        self.operation = Some(operation.into());
        self
    }
}

#[derive(Clone, Debug)]
pub enum Destination {
    File(PathBuf),
    Stderr,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub threshold: Severity,
    pub destination: Destination,
    pub queue_capacity: usize,
    pub max_file_bytes: u64,
    pub retained_files: usize,
    pub service: String,
}

impl Config {
    pub fn from_env(service: &str) -> Result<Self, String> {
        let threshold = std::env::var("PUNKFS_LOG_LEVEL")
            .unwrap_or_else(|_| "info".into())
            .parse()?;
        let destination = match std::env::var("PUNKFS_LOG_DESTINATION").as_deref() {
            Ok("stderr") => Destination::Stderr,
            Ok("file") | Err(_) => {
                let path = if let Some(path) = std::env::var_os("PUNKFS_LOG_FILE") {
                    PathBuf::from(path)
                } else {
                    let base = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from)
                        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                        .ok_or("set PUNKFS_LOG_FILE, XDG_STATE_HOME, HOME, or PUNKFS_LOG_DESTINATION=stderr")?;
                    base.join("punkfs")
                        .join(format!("{service}-{}.jsonl", std::process::id()))
                };
                Destination::File(path)
            }
            Ok(other) => return Err(format!("invalid PUNKFS_LOG_DESTINATION: {other}")),
        };
        Ok(Self {
            threshold,
            destination,
            queue_capacity: 1024,
            max_file_bytes: 10 * 1024 * 1024,
            retained_files: 3,
            service: service.into(),
        })
    }
}

#[derive(Default)]
struct Counters {
    dropped: AtomicU64,
    sink_failures: AtomicU64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoggerStats {
    pub dropped: u64,
    pub sink_failures: u64,
}

enum Command {
    Record(Vec<u8>, Option<String>),
    Flush(mpsc::SyncSender<bool>),
    Shutdown(mpsc::SyncSender<bool>),
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "event exceeds maximum size",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct Logger {
    tx: mpsc::SyncSender<Command>,
    threshold: Severity,
    service: Arc<str>,
    max_record_bytes: usize,
    counters: Arc<Counters>,
}

impl Logger {
    /// Emit an operation event with its explicit ingress identity.
    pub fn emit_in(&self, context: &OperationContext, mut event: Event) {
        event.correlation_id = Some(context.id.to_string());
        self.emit(event);
    }

    /// Never waits for the writer. A full queue or oversized event is counted.
    /// Use [`Logger::emit_in`] for operation-scoped events.
    pub fn emit(&self, mut event: Event) {
        if event.severity < self.threshold {
            return;
        }
        event.severity_number = event.severity as u8;
        event.service = self.service.to_string();
        let mut buffer = LimitedBuffer {
            bytes: Vec::new(),
            limit: self.max_record_bytes.saturating_sub(1),
        };
        if serde_json::to_writer(&mut buffer, &event).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mirror = event.console.then(|| event.message.clone());
        let mut line = buffer.bytes;
        line.push(b'\n');
        if self.tx.try_send(Command::Record(line, mirror)).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn stats(&self) -> LoggerStats {
        LoggerStats {
            dropped: self.counters.dropped.load(Ordering::Relaxed),
            sink_failures: self.counters.sink_failures.load(Ordering::Relaxed),
        }
    }
    /// Wait at most `timeout` for queued records to be flushed.
    pub fn flush(&self, timeout: Duration) -> bool {
        self.control(timeout, false)
    }
    fn control(&self, timeout: Duration, shutdown: bool) -> bool {
        let deadline = Instant::now() + timeout;
        let (tx, rx) = mpsc::sync_channel(1);
        let mut command = if shutdown {
            Command::Shutdown(tx)
        } else {
            Command::Flush(tx)
        };
        loop {
            match self.tx.try_send(command) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(c)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    command = c;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return false,
            }
        }
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(false)
    }
}

/// Owns the writer lifetime. Dropping it requests a bounded flush/shutdown.
pub struct LoggerGuard {
    logger: Logger,
    stopped: bool,
}
impl LoggerGuard {
    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        if self.stopped {
            return true;
        }
        self.stopped = self.logger.control(timeout, true);
        self.stopped
    }
}
impl Drop for LoggerGuard {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.shutdown(Duration::from_millis(200));
        }
    }
}

static GLOBAL: OnceLock<Logger> = OnceLock::new();
pub fn global() -> Option<&'static Logger> {
    GLOBAL.get()
}
/// Initialize the process-wide logger, called only by application entry points.
pub fn init_global(config: Config) -> io::Result<LoggerGuard> {
    if GLOBAL.get().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "logger already initialized",
        ));
    }
    let (logger, guard) = start(config)?;
    GLOBAL
        .set(logger)
        .map_err(|_| io::Error::new(io::ErrorKind::AlreadyExists, "logger already initialized"))?;
    Ok(guard)
}

/// Start an independently owned logger (suitable for embedding and tests).
pub fn start(config: Config) -> io::Result<(Logger, LoggerGuard)> {
    if config.queue_capacity == 0 || config.max_file_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "queue capacity and file size must be nonzero",
        ));
    }
    let mut sink = Sink::open(&config)?;
    let (tx, rx) = mpsc::sync_channel(config.queue_capacity);
    let counters = Arc::new(Counters::default());
    let worker_counters = counters.clone();
    std::thread::Builder::new()
        .name("punkfs-telemetry".into())
        .spawn(move || {
            while let Ok(command) = rx.recv() {
                match command {
                    Command::Record(bytes, console) => {
                        if sink.write_record(&bytes, console.as_deref()).is_err() {
                            worker_counters
                                .sink_failures
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Command::Flush(done) => {
                        let ok = sink.flush().is_ok();
                        if !ok {
                            worker_counters
                                .sink_failures
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        let _ = done.send(ok);
                    }
                    Command::Shutdown(done) => {
                        let ok = sink.flush().is_ok();
                        if !ok {
                            worker_counters
                                .sink_failures
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        let _ = done.send(ok);
                        break;
                    }
                }
            }
        })?;
    let max_record_bytes = match config.destination {
        Destination::File(_) => {
            MAX_EVENT_BYTES.min(config.max_file_bytes.min(usize::MAX as u64) as usize)
        }
        Destination::Stderr => MAX_EVENT_BYTES,
    };
    let logger = Logger {
        tx,
        threshold: config.threshold,
        service: Arc::from(config.service),
        max_record_bytes,
        counters,
    };
    Ok((
        logger.clone(),
        LoggerGuard {
            logger,
            stopped: false,
        },
    ))
}

enum Sink {
    File {
        path: PathBuf,
        file: File,
        size: u64,
        limit: u64,
        retained: usize,
    },
    Stderr,
}
impl Sink {
    fn open(config: &Config) -> io::Result<Self> {
        match &config.destination {
            Destination::Stderr => Ok(Self::Stderr),
            Destination::File(path) => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fs::create_dir_all(parent)?;
                }
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                let size = file.metadata()?.len();
                Ok(Self::File {
                    path: path.clone(),
                    file,
                    size,
                    limit: config.max_file_bytes,
                    retained: config.retained_files,
                })
            }
        }
    }
    fn write_record(&mut self, bytes: &[u8], console: Option<&str>) -> io::Result<()> {
        match self {
            Self::Stderr => {
                io::stderr().lock().write_all(bytes)?;
            }
            Self::File {
                path,
                file,
                size,
                limit,
                retained,
            } => {
                let file_result = (|| -> io::Result<()> {
                    if *size > 0 && size.saturating_add(bytes.len() as u64) > *limit {
                        file.flush()?;
                        rotate(path, *retained)?;
                        *file = OpenOptions::new().create(true).append(true).open(&*path)?;
                        *size = 0;
                    }
                    file.write_all(bytes)?;
                    *size += bytes.len() as u64;
                    Ok(())
                })();
                let console_result = if let Some(message) = console {
                    writeln!(io::stderr().lock(), "{message}")
                } else {
                    Ok(())
                };
                file_result.and(console_result)?;
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File { file, .. } => file.flush(),
            Self::Stderr => io::stderr().flush(),
        }
    }
}
fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}
fn rotate(path: &Path, retained: usize) -> io::Result<()> {
    if retained == 0 {
        return fs::remove_file(path);
    }
    for i in (1..retained).rev() {
        let from = rotated(path, i);
        if from.exists() {
            fs::rename(from, rotated(path, i + 1))?;
        }
    }
    fs::rename(path, rotated(path, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inclusive_severity_order_matches_otel() {
        let levels = [
            Severity::Trace,
            Severity::Debug,
            Severity::Info,
            Severity::Warning,
            Severity::Error,
            Severity::Critical,
            Severity::Fatal,
        ];
        for (i, threshold) in levels.iter().enumerate() {
            for (j, level) in levels.iter().enumerate() {
                assert_eq!(*level >= *threshold, j >= i);
            }
        }
        assert_eq!(Severity::Critical as u8, 21);
        assert_eq!(Severity::Fatal as u8, 24);
    }
    #[test]
    fn actual_sink_filters_all_levels_at_every_threshold() {
        let levels = [
            Severity::Trace,
            Severity::Debug,
            Severity::Info,
            Severity::Warning,
            Severity::Error,
            Severity::Critical,
            Severity::Fatal,
        ];
        let dir = tempfile::tempdir().unwrap();
        for threshold in levels {
            let path = dir.path().join(format!("{threshold:?}.jsonl"));
            let cfg = Config {
                threshold,
                destination: Destination::File(path.clone()),
                queue_capacity: 16,
                max_file_bytes: 100_000,
                retained_files: 1,
                service: "test".into(),
            };
            let (logger, mut guard) = start(cfg).unwrap();
            for level in levels {
                logger.emit(Event::new(level, "test", format!("{level:?}")));
            }
            assert!(guard.shutdown(Duration::from_secs(2)));
            let lines = fs::read_to_string(path).unwrap();
            let actual: Vec<u64> = lines
                .lines()
                .map(|line| {
                    serde_json::from_str::<serde_json::Value>(line).unwrap()["severity_number"]
                        .as_u64()
                        .unwrap()
                })
                .collect();
            let expected: Vec<u64> = levels
                .into_iter()
                .filter(|level| *level >= threshold)
                .map(|level| level as u64)
                .collect();
            assert_eq!(actual, expected, "threshold {threshold:?}");
        }
    }
    #[test]
    fn local_json_filter_and_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let cfg = Config {
            threshold: Severity::Info,
            destination: Destination::File(path.clone()),
            queue_capacity: 10,
            max_file_bytes: 100_000,
            retained_files: 2,
            service: "test".into(),
        };
        let (logger, mut guard) = start(cfg).unwrap();
        logger.emit(Event::new(Severity::Debug, "test", "hidden"));
        let ctx = OperationContext::new();
        let mut event = Event::new(Severity::Info, "test", "visible");
        event.operation = Some("read".into());
        logger.emit_in(&ctx, event);
        assert!(guard.shutdown(Duration::from_secs(2)));
        let lines = fs::read_to_string(path).unwrap();
        let values: Vec<serde_json::Value> = lines
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["correlation_id"], ctx.id().to_string());
        assert_eq!(values[0]["severity_number"], 9);
        assert_eq!(values[0]["service"], "test");
    }
    #[test]
    fn console_capture_child() {
        let Ok(mode) = std::env::var("PUNKFS_TEST_CONSOLE_MODE") else {
            return;
        };
        let destination = if mode == "stderr" {
            Destination::Stderr
        } else {
            Destination::File(PathBuf::from(
                std::env::var_os("PUNKFS_TEST_LOG_PATH").unwrap(),
            ))
        };
        let cfg = Config {
            threshold: Severity::Info,
            destination,
            queue_capacity: 10,
            max_file_bytes: 4096,
            retained_files: 1,
            service: "test".into(),
        };
        let (logger, mut guard) = start(cfg).unwrap();
        let mut event = Event::new(Severity::Info, "test", "CONSOLE_VISIBLE_X");
        event.console = true;
        logger.emit(event);
        let mut filtered = Event::new(Severity::Debug, "test", "CONSOLE_FILTERED_X");
        filtered.console = true;
        logger.emit(filtered);
        assert!(guard.shutdown(Duration::from_secs(2)));
    }
    #[test]
    fn console_flag_mirrors_once_and_never_bypasses_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        for mode in ["file", "stderr"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "telemetry::tests::console_capture_child",
                    "--nocapture",
                ])
                .env("PUNKFS_TEST_CONSOLE_MODE", mode)
                .env("PUNKFS_TEST_LOG_PATH", &path)
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(stderr.matches("CONSOLE_VISIBLE_X").count(), 1, "{stderr}");
            assert!(!stderr.contains("CONSOLE_FILTERED_X"));
        }
        let file = fs::read_to_string(path).unwrap();
        assert_eq!(file.matches("CONSOLE_VISIBLE_X").count(), 1);
        assert!(!file.contains("CONSOLE_FILTERED_X"));
    }
    #[test]
    fn full_queue_and_oversized_records_are_counted_without_blocking() {
        let (tx, _rx) = mpsc::sync_channel(1);
        assert!(tx.try_send(Command::Record(vec![b'x'], None)).is_ok());
        let logger = Logger {
            tx,
            threshold: Severity::Info,
            service: Arc::from("test"),
            max_record_bytes: 1024,
            counters: Arc::new(Counters::default()),
        };
        logger.emit(Event::new(Severity::Info, "test", "queue full"));
        logger.emit(Event::new(Severity::Info, "test", "x".repeat(10_000)));
        assert_eq!(logger.stats().dropped, 2);
    }
    #[test]
    fn file_rotation_limits_retained_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let cfg = Config {
            threshold: Severity::Info,
            destination: Destination::File(path.clone()),
            queue_capacity: 10,
            max_file_bytes: 400,
            retained_files: 2,
            service: "test".into(),
        };
        let (logger, mut guard) = start(cfg).unwrap();
        for n in 0..5 {
            logger.emit(Event::new(
                Severity::Info,
                "test",
                format!("{n}:{}", "a".repeat(100)),
            ));
        }
        assert!(guard.shutdown(Duration::from_secs(2)));
        assert!(path.exists());
        assert!(rotated(&path, 1).exists());
        assert!(rotated(&path, 2).exists());
        assert!(!rotated(&path, 3).exists());
        for p in [path.clone(), rotated(&path, 1), rotated(&path, 2)] {
            assert!(p.metadata().unwrap().len() <= 400);
            for line in fs::read_to_string(p).unwrap().lines() {
                let _: serde_json::Value = serde_json::from_str(line).unwrap();
            }
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn sink_failures_are_counted_without_failing_the_caller() {
        let cfg = Config {
            threshold: Severity::Info,
            destination: Destination::File(PathBuf::from("/dev/full")),
            queue_capacity: 10,
            max_file_bytes: 10_000,
            retained_files: 1,
            service: "test".into(),
        };
        let (logger, mut guard) = start(cfg).unwrap();
        logger.emit(Event::new(Severity::Info, "test", "synthetic failure"));
        assert!(logger.flush(Duration::from_secs(2)));
        assert!(logger.stats().sink_failures >= 1);
        let _ = guard.shutdown(Duration::from_secs(2));
    }
    #[test]
    fn context_rejects_bad_and_duplicate_metadata() {
        let ctx = OperationContext::new();
        let req = ctx.request(());
        assert_eq!(OperationContext::from_request(&req).unwrap(), ctx);
        let absent = OperationContext::from_request(&Request::new(())).unwrap();
        assert_ne!(absent, ctx);
        let mut bad = Request::new(());
        bad.metadata_mut()
            .insert(CORRELATION_HEADER, "bad".parse().unwrap());
        assert_eq!(
            OperationContext::from_request(&bad).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let mut upper = Request::new(());
        upper.metadata_mut().insert(
            CORRELATION_HEADER,
            "ABCDEFAB-CDEF-4ABC-8DEF-ABCDEFABCDEF".parse().unwrap(),
        );
        assert_eq!(
            OperationContext::from_request(&upper).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let mut oversized = Request::new(());
        oversized
            .metadata_mut()
            .insert(CORRELATION_HEADER, "x".repeat(1024).parse().unwrap());
        assert_eq!(
            OperationContext::from_request(&oversized)
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
        let mut dup = ctx.request(());
        dup.metadata_mut()
            .append(CORRELATION_HEADER, ctx.id().to_string().parse().unwrap());
        assert_eq!(
            OperationContext::from_request(&dup).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }
}
