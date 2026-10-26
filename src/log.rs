//! Optional diagnostics, silent by default and switched on as in the C driver:
//! `NVD_LOG=1` writes to stdout, `NVD_LOG=<path>` appends to that file, `NVD_LOG_VERBOSE` (not "0") adds the
//! per-call trace (`debug!`).
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

enum Sink {
    Stdout,
    File(File),
}
struct Log {
    sink: Option<Sink>,
    verbose: bool,
}

static LOG: LazyLock<Log> = LazyLock::new(|| {
    let sink = std::env::var("NVD_LOG").ok().map(|target| {
        if target == "1" {
            Sink::Stdout
        } else {
            OpenOptions::new().create(true).append(true).open(&target).map_or(Sink::Stdout, Sink::File)
        }
    });
    let verbose = std::env::var("NVD_LOG_VERBOSE").is_ok_and(|v| v != "0");
    Log { sink, verbose }
});

pub(crate) fn enabled() -> bool {
    LOG.sink.is_some()
}
pub(crate) fn verbose() -> bool {
    LOG.verbose && enabled()
}
/// One line in the C driver's format: `[rustnv] sec.nsec [pid-tid] file:line message`.
pub(crate) fn write(file: &str, line: u32, message: std::fmt::Arguments) {
    let Some(sink) = &LOG.sink else { return };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    // SAFETY: gettid has no preconditions.
    let tid = unsafe { libc_gettid() };
    let text = format!(
        "[rustnv] {:10}.{:09} [{}-{}] {}:{:4} {}\n",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id(),
        tid,
        file.rsplit('/').next().unwrap_or(file),
        line,
        message
    );
    match sink {
        Sink::Stdout => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
        Sink::File(file) => {
            let _ = (&*file).write_all(text.as_bytes());
        }
    }
}
#[allow(non_camel_case_types)]
extern "C" {
    #[link_name = "gettid"]
    fn libc_gettid() -> i32;
}

macro_rules! log {
    ($($arg:tt)*) => {
        if $crate::log::enabled() {
            $crate::log::write(file!(), line!(), format_args!($($arg)*))
        }
    };
}
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::verbose() {
            $crate::log::write(file!(), line!(), format_args!($($arg)*))
        }
    };
}
pub(crate) use {debug, log};
