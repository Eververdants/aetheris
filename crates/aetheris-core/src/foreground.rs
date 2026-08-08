//! Foreground window watcher.
//!
//! Registers a WinEvent hook for `EVENT_SYSTEM_FOREGROUND` and runs a Win32
//! message pump on a dedicated thread. Focus changes are forwarded as
//! [`ForegroundEvent`] on a channel.
//!
//! The WinEvent callback has no context parameter, so the sender lives in a
//! process-global [`OnceLock`]. The watcher starts once at service init and
//! lives for the process lifetime, so this is acceptable; calling `start()`
//! twice in the same process returns an error (the `OnceLock` keeps the first
//! sender).

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{
    HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GetMessageW, GetWindowThreadProcessId,
    PeekMessageW, PM_NOREMOVE, PostThreadMessageW, MSG, WINEVENT_OUTOFCONTEXT, WM_QUIT,
};

use crate::events::ForegroundEvent;

/// Process-global sender used by the WinEvent callback (which has no context
/// parameter). Set once by [`ForegroundWatcher::start`].
static FOREGROUND_TX: OnceLock<Sender<ForegroundEvent>> = OnceLock::new();

pub struct ForegroundWatcher {
    rx: Receiver<ForegroundEvent>,
    handle: Option<JoinHandle<()>>,
    /// OS thread id of the message pump, so `stop()` can post WM_QUIT to it.
    pump_thread_id: u32,
}

/// WinEvent callback for `EVENT_SYSTEM_FOREGROUND`. Extracts the PID owning
/// the newly focused window and forwards it through the global sender.
unsafe extern "system" fn win_event_proc(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwm_event_time: u32,
) {
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return;
    }
    if let Some(tx) = FOREGROUND_TX.get() {
        let _ = tx.send(ForegroundEvent { pid });
    }
}

impl ForegroundWatcher {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = channel::<ForegroundEvent>();
        if FOREGROUND_TX.set(tx).is_err() {
            return Err("ForegroundWatcher::start called twice".to_string());
        }

        // The pump thread registers its OS thread id first so `stop()` can
        // target it with PostThreadMessageW. (PostMessageW to a NULL window
        // only reaches the *calling* thread, so it cannot stop a foreign pump.)
        let (tid_tx, tid_rx) = channel::<u32>();

        let handle = thread::spawn(move || {
            // Windows creates a thread's message queue lazily on the first
            // GetMessage/PeekMessage. `stop()` posts WM_QUIT to this thread id
            // via PostThreadMessageW, which fails with ERROR_INVALID_THREAD_ID
            // if the queue doesn't exist yet, leaving `join()` to hang. Force
            // the queue into existence BEFORE publishing the id so that once
            // `start()` returns, `stop()` always succeeds.
            let mut force_queue = std::mem::MaybeUninit::<MSG>::uninit();
            let _ = unsafe {
                PeekMessageW(force_queue.as_mut_ptr(), None, 0, 0, PM_NOREMOVE)
            };
            let _ = tid_tx.send(unsafe { GetCurrentThreadId() });
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(win_event_proc),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if !hook.0.is_null() {
                let mut msg = MSG::default();
                loop {
                    let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                    if r.0 == 0 {
                        break; // WM_QUIT
                    }
                    unsafe { DispatchMessageW(&msg) };
                }
                let _ = unsafe { UnhookWinEvent(hook) };
            }
        });

        let pump_thread_id = tid_rx
            .recv()
            .map_err(|_| "foreground pump thread exited before registering its id")?;

        Ok(Self {
            rx,
            handle: Some(handle),
            pump_thread_id,
        })
    }

    pub fn recv(&self) -> Option<ForegroundEvent> {
        self.rx.recv().ok()
    }

    pub fn stop(mut self) {
        // Ask the pump thread to exit by posting WM_QUIT to its queue, then join.
        unsafe {
            let _ = PostThreadMessageW(self.pump_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn start_and_stop_does_not_hang() {
        // The brief calls for manual verification only (the hook needs an
        // interactive session); this smoke test merely proves start/stop is
        // race-free: `stop()` must post WM_QUIT to the pump and join. A broken
        // quit delivery would hang here forever.
        let w = ForegroundWatcher::start().expect("watcher should start");
        let _ = w.rx.recv_timeout(Duration::from_millis(50)); // no focus change expected
        w.stop();
    }
}
