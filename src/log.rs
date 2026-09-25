//! Logger behind the `log` facade, so our own log lines AND those of
//! dependencies (the pure-Rust zmq stack logs via `log`) land in one place:
//! an in-memory ring (Info and up, shown by the `L` log viewer) and, with
//! `--log <file>`, a timestamped file that also gets Debug (kernel wire).

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Records kept for the log viewer; older ones are dropped.
const RING_CAP: usize = 2000;

pub struct Entry {
    /// Seconds since startup (dmesg-style; no clock/timezone dependency).
    pub secs: f64,
    pub level: log::Level,
    pub target: String,
    pub msg: String,
}

static RING: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());

struct Logger {
    start: Instant,
    file: Option<Mutex<File>>,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info || self.file.is_some()
    }

    fn log(&self, record: &log::Record) {
        if let Some(file) = &self.file {
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let mut f = file.lock().unwrap();
            let _ = writeln!(
                f,
                "{}.{:03} {:5} {}: {}",
                t.as_secs(),
                t.subsec_millis(),
                record.level(),
                record.target(),
                record.args()
            );
        }
        if record.level() <= log::Level::Info {
            let mut ring = RING.lock().unwrap();
            if ring.len() == RING_CAP {
                ring.pop_front();
            }
            ring.push_back(Entry {
                secs: self.start.elapsed().as_secs_f64(),
                level: record.level(),
                target: record.target().to_string(),
                msg: record.args().to_string(),
            });
        }
    }

    fn flush(&self) {}
}

/// Install the logger; `path` appends to a debug log file as well. Errors
/// (bad log path) are returned for the caller to print.
pub fn init(path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let file = match path {
        Some(p) => Some(Mutex::new(
            File::options().create(true).append(true).open(p)?,
        )),
        None => None,
    };
    let level = if file.is_some() {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    log::set_boxed_logger(Box::new(Logger {
        start: Instant::now(),
        file,
    }))?;
    log::set_max_level(level);
    Ok(())
}

/// Run `f` over the ring (oldest first) without copying it out.
pub fn with_entries<R>(f: impl FnOnce(&VecDeque<Entry>) -> R) -> R {
    f(&RING.lock().unwrap())
}
