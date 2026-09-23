//! What an application that takes the view says about itself.
//!
//! An application on a Wayland connection says these things through `spatiand_xr_v1`: it gets
//! an xr surface and sets its layer and its eye layout. The viewers this host runs are X11
//! programs behind XWayland, and cannot — their window is not a surface they can name. So each
//! application launched with `kind = "vr"` is handed one end of a socket, named in
//! `SPATIAND_CONTROL_FD`, and says the same two things down it in the same words:
//!
//! ```text
//! set_eye_layout side_by_side
//! set_layer projection
//! ```
//!
//! and, for an application that is the room and draws its own pointer at the depth of what
//! is under it (see `spatiand_xr_surface_v1.set_cursor_drawn`):
//!
//! ```text
//! set_cursor_drawn 1
//! ```
//!
//! The host says one thing back, whenever it changes:
//!
//! ```text
//! set_render_size 3840 1080
//! ```
//!
//! — the size the session wants the application's picture drawn at, both eyes together, from
//! the viewports it sends. An application that doubles its width for two eyes should double to
//! this, not to whatever size its window happened to be: a picture that is not the glasses' own
//! shape is stretched to fit them.
//!
//! One message per datagram (it is a `SOCK_SEQPACKET`), no framing to get wrong. Whatever is
//! said applies to every window the application has, which for a viewer is its one window. The
//! host passes it on as the stream's eye layout and a [`HostMessage::Layer`], and the session
//! claims it through `spatiand_xr_v1` on its own side — so what the application asked for is
//! judged by exactly the rules a local application's request would be.
//!
//! [`HostMessage::Layer`]: spatiand_stream::HostMessage::Layer

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use spatiand_stream::{Eyes, Layer};

use crate::state::Host;

/// A connected pair, both ends close-on-exec: `(ours, theirs)`.
pub fn pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: a libc call filling an array we own.
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: socketpair returned two fresh descriptors that are now ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// One thing an application can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Said {
    Eyes(Eyes),
    Layer(Layer),
    CursorDrawn(bool),
}

/// Read one message, in `spatiand_xr_v1`'s own words.
fn parse(message: &str) -> Option<Said> {
    let mut words = message.split_whitespace();
    let request = words.next()?;
    let value = words.next()?;
    if words.next().is_some() {
        return None;
    }
    match (request, value) {
        ("set_eye_layout", "mono") => Some(Said::Eyes(Eyes::Mono)),
        ("set_eye_layout", "side_by_side") => Some(Said::Eyes(Eyes::SideBySide)),
        ("set_eye_layout", "top_bottom") => Some(Said::Eyes(Eyes::TopBottom)),
        ("set_layer", "window") => Some(Said::Layer(Layer::Window)),
        ("set_layer", "projection") => Some(Said::Layer(Layer::Projection)),
        ("set_cursor_drawn", "0") => Some(Said::CursorDrawn(false)),
        ("set_cursor_drawn", "1") => Some(Said::CursorDrawn(true)),
        _ => None,
    }
}

/// Listen to what `app` says on its end of a pair.
///
/// When it closes — the application exited — whatever it had claimed is given back, so a
/// crashed viewer does not leave the session showing a room nobody is drawing any more.
pub fn watch(host: &mut Host, app: String, ours: OwnedFd) {
    // A second descriptor onto our end, for saying things: the first goes to the event loop,
    // which owns it until the application hangs up.
    match ours.try_clone() {
        Ok(writer) => {
            host.app_controls.insert(app.clone(), (writer, None));
        }
        Err(e) => log::warn!("{app}'s control socket cannot be written to: {e}"),
    }
    let handle: LoopHandle<'static, Host> = host.loop_handle.clone();
    let source = Generic::new(ours, Interest::READ, Mode::Level);
    let inserted = handle.insert_source(source, move |_, fd, host: &mut Host| {
        let mut buffer = [0u8; 256];
        loop {
            // SAFETY: reading into a buffer we own, from a descriptor the source keeps open.
            let n = unsafe {
                libc::recv(
                    fd.as_fd().as_raw_fd(),
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n == 0 {
                log::info!("{app} closed its control socket; whatever it claimed is given back");
                if host.presentation.remove(&app).is_some() {
                    host.presentation_changed.push(app.clone());
                }
                if host.cursor_drawn.remove(&app) {
                    host.cursor_changed.push(app.clone());
                }
                host.app_controls.remove(&app);
                return Ok(PostAction::Remove);
            }
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(PostAction::Continue);
                }
                log::warn!("{app}'s control socket: {e}");
                return Ok(PostAction::Remove);
            }
            let message = String::from_utf8_lossy(&buffer[..n as usize]);
            match parse(&message) {
                Some(Said::CursorDrawn(drawn)) => {
                    let changed = if drawn {
                        host.cursor_drawn.insert(app.clone())
                    } else {
                        host.cursor_drawn.remove(&app)
                    };
                    if changed {
                        log::info!("{app} says {}", message.trim());
                        host.cursor_changed.push(app.clone());
                    }
                }
                Some(said) => {
                    let now = host.presentation.entry(app.clone()).or_default();
                    let before = *now;
                    match said {
                        Said::Eyes(eyes) => now.0 = eyes,
                        Said::Layer(layer) => now.1 = layer,
                        Said::CursorDrawn(_) => unreachable!("handled above"),
                    }
                    if *now != before {
                        log::info!("{app} says {}", message.trim());
                        host.presentation_changed.push(app.clone());
                    }
                }
                None => log::warn!("{app} said something this host does not know: {:?}", message.trim()),
            }
        }
    });
    if let Err(e) = inserted {
        log::warn!("could not listen to an application's control socket: {}", e.error);
    }
}

/// Tell every view-taking application the size the session wants, if it has not been told it.
///
/// Called each time round the compositor's loop. Cheap when nothing changed: it is a comparison
/// per application, and a message only when the size moved or an application is new.
pub fn tell_render_size(host: &mut Host, size: Option<(u32, u32)>) {
    let Some((width, height)) = size else { return };
    for (app, (fd, told)) in host.app_controls.iter_mut() {
        if *told == Some((width, height)) {
            continue;
        }
        let message = format!("set_render_size {width} {height}");
        // SAFETY: sending a buffer we own on a descriptor we own. MSG_NOSIGNAL: an application
        // that has just exited must cost an error, not the host.
        let sent = unsafe {
            libc::send(
                fd.as_raw_fd(),
                message.as_ptr() as *const libc::c_void,
                message.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if sent == message.len() as isize {
            log::info!("told {app}: {message}");
            *told = Some((width, height));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_understands_the_protocols_own_words() {
        assert_eq!(parse("set_layer projection"), Some(Said::Layer(Layer::Projection)));
        assert_eq!(parse("set_layer window\n"), Some(Said::Layer(Layer::Window)));
        assert_eq!(parse("set_eye_layout side_by_side"), Some(Said::Eyes(Eyes::SideBySide)));
        assert_eq!(parse("set_eye_layout mono"), Some(Said::Eyes(Eyes::Mono)));
        assert_eq!(parse("set_cursor_drawn 1"), Some(Said::CursorDrawn(true)));
        assert_eq!(parse("set_cursor_drawn 0"), Some(Said::CursorDrawn(false)));
    }

    #[test]
    fn it_refuses_what_it_does_not_know_rather_than_guessing() {
        assert_eq!(parse("set_layer equirect_360"), None, "not a remote layer yet");
        assert_eq!(parse("set_layer"), None);
        assert_eq!(parse("set_layer projection please"), None);
        assert_eq!(parse("make_it_so"), None);
        assert_eq!(parse("set_cursor_drawn yes"), None);
    }

    #[test]
    fn a_pair_carries_one_message_per_read() {
        let (ours, theirs) = pair().expect("pair");
        for said in ["set_eye_layout side_by_side", "set_layer projection"] {
            // SAFETY: writing a buffer we own to a descriptor we own.
            let n = unsafe {
                libc::send(theirs.as_raw_fd(), said.as_ptr() as *const libc::c_void, said.len(), 0)
            };
            assert_eq!(n as usize, said.len());
        }
        let mut buffer = [0u8; 256];
        for expected in ["set_eye_layout side_by_side", "set_layer projection"] {
            // SAFETY: reading into a buffer we own.
            let n = unsafe {
                libc::recv(ours.as_raw_fd(), buffer.as_mut_ptr() as *mut libc::c_void, buffer.len(), 0)
            };
            assert_eq!(std::str::from_utf8(&buffer[..n as usize]).unwrap(), expected);
        }
    }
}
