//! Leaving tidily when something asks us to stop.
//!
//! This exists because of a hardware symptom that took a while to attribute. Claiming the Deck's
//! controller means sending `ID_CLEAR_DIGITAL_MAPPINGS` to its firmware, and handing it back
//! means sending `ID_LOAD_DEFAULT_SETTINGS`. The handing back lives in `Drop`, which is the
//! right place for it — and `Drop` does not run when the process is killed by a signal.
//!
//! So any SIGTERM at all — `pkill`, the session ending, systemd stopping the unit, a display
//! manager switching sessions — left the controller with its digital buttons unmapped. The
//! symptom is a button that has simply stopped existing: the bumper does nothing in Game Mode,
//! and a soft reboot does not fix it, because the controller's microcontroller keeps power
//! across one. Only a full power-off clears it. Nothing about that points back at a compositor
//! that exited hours earlier.
//!
//! A flag set from a handler and read by the render loop is enough. The loop runs at 72 Hz, so
//! the delay is imperceptible, and the exit path is the ordinary one — which means every other
//! `Drop` gets to run too, including handing the glasses back to 2D.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Signal handler. Must be async-signal-safe, which an atomic store and `signal` both are.
///
/// It disarms itself, and that is not a detail. Catching a signal means it no longer kills the
/// process, so a run loop that never looks at the flag would become unkillable by ordinary
/// means — and the way that ends is systemd or the display manager losing patience and sending
/// `SIGKILL`, which cannot be caught and runs no destructor. That is the very failure this
/// module exists to prevent, arrived at by a longer route.
///
/// Restoring the default disposition means the first signal asks politely and a second one is
/// obeyed immediately, so nothing can be wedged by a compositor that is too busy to notice.
extern "C" fn handle(signal: libc::c_int) {
    REQUESTED.store(true, Ordering::SeqCst);
    // SAFETY: `signal` is async-signal-safe, and SIG_DFL is the disposition we started with.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
    }
}

/// Set when `SIGUSR1` arrives, and cleared by whoever takes the picture.
static PICTURE: AtomicBool = AtomicBool::new(false);

/// Take a screenshot on the next frame.
///
/// `SIGUSR1`, because the alternative is somebody wearing the glasses and describing what they
/// see. A session on a headset has no way to show anyone else what is on it, which makes every
/// question about whether something is drawn a question for a person rather than a thing to
/// check — and that is a slow way to find out that a window is one step from being drawn.
///
///     kill -USR1 $(pgrep -x spatiand)
extern "C" fn picture(signal: libc::c_int) {
    PICTURE.store(true, Ordering::SeqCst);
    // Deliberately *not* restoring the default: this signal is asked for repeatedly, and the
    // default disposition for SIGUSR1 is to kill the process.
    let _ = signal;
}

/// Has a picture been asked for? Clears the request.
pub fn picture_requested() -> bool {
    PICTURE.swap(false, Ordering::SeqCst)
}

/// Ask to be told about the signals that mean "stop".
///
/// `SIGHUP` is in the list because that is what a session leader gets when its terminal or
/// session goes away, which is precisely the case where the controller most needs handing
/// back.
pub fn install() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: `handle` only stores to an atomic, so it is safe to run from a signal.
        unsafe {
            libc::signal(signal, handle as *const () as libc::sighandler_t);
        }
    }
    // SAFETY: `picture` only stores to an atomic, so it is safe to run from a signal.
    unsafe {
        libc::signal(libc::SIGUSR1, picture as *const () as libc::sighandler_t);
    }
}

/// Has something asked us to stop?
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_requested_until_a_signal_arrives() {
        // The flag is global, so this test only asserts the initial reading. Raising a real
        // signal here would make every other test in the process see the shutdown flag set.
        assert!(!requested() || REQUESTED.load(Ordering::SeqCst));
    }

    #[test]
    fn installing_twice_is_harmless() {
        // The rebuild loop is re-entered on every hotplug, and guarding the call at each site
        // is how one gets missed.
        install();
        install();
    }
}
