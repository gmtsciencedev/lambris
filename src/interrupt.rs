//! Cooperative cancellation for long-running operations.
//!
//! Heavy work (sorting, filtering, searching a big file) runs synchronously on
//! the main thread, so the only way to abort it is for the work itself to check
//! whether the user has asked to stop. [`requested`] does that by draining any
//! pending terminal input and looking for `Esc` or `Ctrl-C`; heavy loops call
//! it periodically and give up when it returns `true`.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

/// Latched once an interrupt key is seen, so every subsequent check within the
/// same operation keeps reporting it (the keypress is consumed only once).
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Cap on events drained per call, so a stuck/EOF input stream can't spin here.
const MAX_DRAIN: usize = 64;

/// Has the user pressed `Esc` or `Ctrl-C` since the current operation began?
/// Non-blocking, and inert when stdin is not a terminal (tests, pipes).
pub fn requested() -> bool {
    if REQUESTED.load(Ordering::Relaxed) {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        return false;
    }
    let mut drained = 0;
    while drained < MAX_DRAIN && event::poll(Duration::ZERO).unwrap_or(false) {
        drained += 1;
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press && is_interrupt(&k) => {
                REQUESTED.store(true, Ordering::Relaxed);
                return true;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    false
}

/// Read and clear the latched interrupt flag; call once after handling a
/// cancellable operation so the next one starts fresh.
pub fn take() -> bool {
    REQUESTED.swap(false, Ordering::Relaxed)
}

fn is_interrupt(k: &crossterm::event::KeyEvent) -> bool {
    k.code == KeyCode::Esc
        || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
}
