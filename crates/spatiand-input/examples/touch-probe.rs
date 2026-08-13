//! Watch the touchscreen, outside a compositor.
//!
//! The sidecar's touch handling is testable without hardware, and is tested that way. What it
//! cannot tell you is whether *this* panel reports the ranges we asked it for, or whether the
//! event node opens at all — so this exists to answer those two questions in ten seconds.
//!
//! ```text
//! sudo ~/spatiand-target-holo/release/examples/touch-probe
//! ```
//!
//! Root, because `/dev/input/event*` is `root:input` with no ACL for the logged-in user.
//! Spatiand itself does not need root: it asks logind for the device, the same way it asks for
//! the GPU. This is a probe, and a probe that needs a session is no use for finding out why a
//! session cannot open something.

fn main() {
    let nodes = spatiand_input::touch::find_touchscreens();
    if nodes.is_empty() {
        eprintln!("no direct-touch device found in /proc/bus/input/devices");
        std::process::exit(1);
    }
    for node in &nodes {
        println!("found: {} at {}", node.name, node.path.display());
    }

    let path = &nodes[0].path;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open {}: {e} (try sudo)", path.display());
            std::process::exit(1);
        }
    };
    let mut touch = match spatiand_input::Touchscreen::from_fd(file.into()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot set up {}: {e}", path.display());
            std::process::exit(1);
        }
    };

    // The panel is 800x1280 portrait and the sidecar is drawn landscape, so what matters is
    // not the raw reading but where it lands *after* the quarter turn. Printed both ways: a
    // touch at the top left of what you can read on the screen should print a landscape
    // position near (0, 0), whichever raw numbers it came from.
    let (panel_w, panel_h) = (800.0f32, 1280.0f32);
    let (land_w, land_h) = (panel_h, panel_w);
    println!("\ntouch the screen; ten seconds. corners are the interesting part.");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < end {
        for event in touch.poll() {
            if let spatiand_input::TouchEvent::Down(c) | spatiand_input::TouchEvent::Motion(c) =
                event
            {
                let (lx, ly) = (land_w * (1.0 - c.y), land_h * c.x);
                println!(
                    "slot {} raw ({:4.0}, {:4.0}) -> landscape ({:4.0}, {:3.0}) of {land_w}x{land_h}",
                    c.slot,
                    c.x * panel_w,
                    c.y * panel_h,
                    lx,
                    ly
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
