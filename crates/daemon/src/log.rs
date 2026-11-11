use ::log::{LevelFilter, Log, Metadata, Record};

use crate::config::LogLevel;

struct Stderr;

impl Log for Stderr {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= ::log::max_level()
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            eprintln!("{} {}: {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

/// Install the stderr logger. The logger is set once per process; the level
/// is applied on every call, so repeated in-process runs just retune it.
pub fn init_logging(level: LogLevel) {
    let _ = ::log::set_logger(&Stderr);
    ::log::set_max_level(match level {
        LogLevel::Off => LevelFilter::Off,
        LogLevel::Warn => LevelFilter::Warn,
        LogLevel::Info => LevelFilter::Info,
        LogLevel::Debug => LevelFilter::Debug,
    });
}

#[cfg(test)]
mod tests {
    use ::log::Level;

    use super::*;

    #[test]
    fn level_filter_drops_lower_levels() {
        let on = |l| Stderr.enabled(&Metadata::builder().level(l).build());
        init_logging(LogLevel::Warn);
        assert!(on(Level::Error) && on(Level::Warn));
        assert!(!on(Level::Info) && !on(Level::Debug));
        init_logging(LogLevel::Off);
        assert!(
            ![Level::Error, Level::Warn, Level::Info, Level::Debug]
                .into_iter()
                .any(on)
        );
        init_logging(LogLevel::Info);
        assert!(on(Level::Info) && !on(Level::Debug));
        init_logging(LogLevel::Debug);
        assert!(on(Level::Debug));
        Stderr.log(
            &Record::builder()
                .args(format_args!("x"))
                .level(Level::Debug)
                .build(),
        );
        Stderr.flush();
    }
}
