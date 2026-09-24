//! `--log <file>`: timestamped line logger behind the `log` facade, so our
//! own log lines AND those of dependencies (the pure-Rust zmq stack logs via
//! `log`) land in one file. No file, no logger, zero cost.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct FileLogger(Mutex<File>);

impl log::Log for FileLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut f = self.0.lock().unwrap();
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

    fn flush(&self) {}
}

/// Install the file logger. Errors are returned for the caller to print.
pub fn init(path: &std::path::Path) -> anyhow::Result<()> {
    let file = File::create(path)?;
    log::set_boxed_logger(Box::new(FileLogger(Mutex::new(file))))?;
    log::set_max_level(log::LevelFilter::Debug);
    Ok(())
}
