use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

pub struct RingLogger {
    inner: Mutex<VecDeque<(Level, String)>>,
    capacity: usize,
}

impl RingLogger {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn log(&self, level: Level, msg: String) {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            q.pop_front();
        }
        q.push_back((level, msg));
    }

    pub fn dump(&self) -> Vec<String> {
        let q = self.inner.lock().unwrap();
        q.iter()
            .map(|(l, m)| format!("{}: {}", format!("{:?}", l).to_uppercase(), m))
            .collect()
    }
}

pub static LOGGER: OnceLock<RingLogger> = OnceLock::new();

pub fn init(capacity: usize) {
    let _ = LOGGER.set(RingLogger::new(capacity));
}

pub fn info(msg: impl AsRef<str>) {
    if let Some(l) = LOGGER.get() {
        l.log(Level::Info, msg.as_ref().to_string());
    }
}

pub fn warn(msg: impl AsRef<str>) {
    if let Some(l) = LOGGER.get() {
        l.log(Level::Warn, msg.as_ref().to_string());
    }
}

pub fn error(msg: impl AsRef<str>) {
    if let Some(l) = LOGGER.get() {
        l.log(Level::Error, msg.as_ref().to_string());
    }
}

pub fn dump() -> Vec<String> {
    match LOGGER.get() {
        Some(l) => l.dump(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_logger_keeps_last_n_and_dumps_oldest_first() {
        let logger = RingLogger::new(3);
        logger.log(Level::Info, "one".into());
        logger.log(Level::Warn, "two".into());
        logger.log(Level::Error, "three".into());
        logger.log(Level::Info, "four".into()); // evicts "one"
        let lines = logger.dump();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("two"));
        assert!(lines[0].contains("WARN"));
        assert!(lines[2].contains("four"));
    }

    #[test]
    fn global_init_and_macros() {
        init(4);
        info("hello");
        warn("world");
        let lines = dump();
        assert!(lines.iter().any(|l| l.contains("hello")));
        assert!(lines.iter().any(|l| l.contains("world")));
    }
}
