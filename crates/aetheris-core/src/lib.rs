//! aetheris core engine.
//! Zero-overhead Windows game-optimization service library.
//! No async runtime. Single-threaded main loop fed by dedicated threads.

pub mod actions;
pub mod config;
pub mod etw;
pub mod events;
pub mod foreground;
pub mod hotkey;
pub mod i18n;
pub mod ipc;
pub mod log;
pub mod network;
pub mod policy;
pub mod proc_table;
pub mod rules;
pub mod service;
