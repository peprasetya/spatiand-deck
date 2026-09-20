//! Decode a stream the host produced, and say whether the pictures are real.
//!
//! The last check before any of this reaches a compositor: it runs on the Deck over ssh, needs
//! no session and no glasses, and answers the two questions that matter — does the headset's
//! GPU decode what the host's GPU encoded, and how long does each picture take.
//!
//! ```text
//! probe-decode /tmp/deck-seen.hevc [--out /tmp/frame.nv12] [--frame 100]
//! ```

use std::time::Instant;

use spatiand_stream::Codec;
use spatiand_video::{Converter, Decoder, Units};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("probe-decode <file.hevc> [--out <file.nv12>] [--frame n]");
        std::process::exit(2);
    };
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let out = value("--out");
    let wanted: u64 = value("--frame").and_then(|v| v.parse().ok()).unwrap_or(30);
    let node = std::env::var("SPATIAND_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());

    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let mut decoder = Decoder::new(&node, Codec::H265).expect("a decoder");

    // A file has no frame boundaries in it, so they are found first — the link does not need
    // this, because a frame arrives as a frame.
    let mut units = Units::new(Codec::H265).expect("a parser");
    let frames = units.frames(&bytes);
    println!("{} frames in the file", frames.len());

    let mut times: Vec<f32> = Vec::new();
    let mut decoded = 0u64;
    let mut kept = None;
    for (n, frame) in frames.iter().enumerate() {
        let began = Instant::now();
        let pictures = match decoder.decode(n as i64, frame) {
            Ok(pictures) => pictures,
            Err(e) => {
                eprintln!("frame {n}: {e}");
                continue;
            }
        };
        times.push(began.elapsed().as_secs_f32() * 1000.0);
        decoded += pictures.len() as u64;
        if n as u64 == wanted {
            // And through the colour conversion, which is the step between a decoded picture
            // and something a compositor can draw. Checked here because a fault in it looks
            // exactly like a fault in the decoder from the outside: a window full of nothing.
            if let Some(picture) = pictures.first() {
                let (device, frames) = decoder.device();
                match Converter::new(device, frames, (picture.width, picture.height))
                    .and_then(|mut c| c.convert(picture).map(|converted| converted.to_bgra()))
                {
                    Ok(Ok(bgra)) => {
                        let path = "/tmp/converted.bgra";
                        std::fs::write(path, &bgra).expect("writes");
                        let grey = bgra.chunks(4).all(|p| {
                            (p[0] as i16 - p[1] as i16).abs() < 3
                                && (p[1] as i16 - p[2] as i16).abs() < 3
                        });
                        println!(
                            "converted {}x{} to BGRA, {} bytes -> {path}{}",
                            picture.width,
                            picture.height,
                            bgra.len(),
                            if grey { "  (ALL GREY — the conversion produced nothing)" } else { "" }
                        );
                    }
                    Ok(Err(e)) | Err(e) => println!("conversion failed: {e}"),
                }
            }
            kept = pictures.into_iter().next();
        }
    }

    times.sort_by(f32::total_cmp);
    let mean: f32 = times.iter().sum::<f32>() / times.len().max(1) as f32;
    println!(
        "{decoded} pictures, {mean:.2} ms each (median {:.2}, worst {:.2}), stream {:?}",
        times.get(times.len() / 2).copied().unwrap_or(0.0),
        times.last().copied().unwrap_or(0.0),
        decoder.size()
    );

    if let (Some(out), Some(picture)) = (out, kept) {
        let nv12 = picture.to_nv12().expect("reads back");
        std::fs::write(&out, &nv12).expect("writes");
        println!(
            "wrote {out}: {}x{} NV12, {} bytes",
            picture.width,
            picture.height,
            nv12.len()
        );
    }
}
