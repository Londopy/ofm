// ctrl-c, and on windows closing the console, logging off or shutting down, ask the loops to
// stop so they can release PTT on the way out

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Call once everything that can key the radio has been dropped.
pub fn finished() {
    DONE.store(true, Ordering::SeqCst);
}

pub fn install() -> Result<()> {
    ctrlc::set_handler(|| {
        // a second press means the loop is stuck; leave anyway
        if STOP.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
    })
    .context("installing the ctrl-c handler")?;
    #[cfg(windows)]
    console::install()?;
    Ok(())
}

#[cfg(windows)]
mod console {
    use super::{DONE, STOP};
    use anyhow::{Context, Result};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    // windows ends the process as soon as a handler returns from these, and 5 s after the
    // event at the latest, so hold it here while the loop releases PTT
    unsafe extern "system" fn on_event(kind: u32) -> BOOL {
        if !matches!(
            kind,
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
        ) {
            // ctrl-c and ctrl-break go on to the ctrlc handler
            return 0;
        }
        STOP.store(true, Ordering::SeqCst);
        let start = Instant::now();
        while !DONE.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(4) {
            std::thread::sleep(Duration::from_millis(10));
        }
        1
    }

    pub fn install() -> Result<()> {
        // added after ctrlc's handler, so windows asks this one first
        if unsafe { SetConsoleCtrlHandler(Some(on_event), 1) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("installing the console close handler");
        }
        Ok(())
    }
}
