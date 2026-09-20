//! Pair with a real host from a laptop, the way the headset does, and say what happened.
//!
//! ```text
//! cargo run -p spatiand-stream --example probe-pair -- workshop /tmp/probe-identity
//! ```
//!
//! On the host, `spatiand-host --pair` shows a code; this prints the one it computed. They
//! must match. The identity directory is this probe's own, so pairing it does not disturb a
//! headset's — forget it on the host afterwards with `--forget <first 8 hex>`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let address = args.first().cloned().unwrap_or_else(|| "workshop".into());
    let dir = std::path::PathBuf::from(
        args.get(1).cloned().unwrap_or_else(|| "/tmp/spatiand-probe-identity".into()),
    );
    let identity = spatiand_stream::Identity::load_or_create(&dir).expect("an identity");
    println!("this probe is {}", identity.fingerprint().short());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let outcome = runtime.block_on(spatiand_stream::link::pair(&identity, &address, |p| {
        let spatiand_stream::link::Pairing::Compare { fingerprint, code } = p;
        println!("host is {}; code {code}", fingerprint.short());
    }));
    match outcome {
        Ok(paired) => println!("PAIRED with {} ({})", paired.name, paired.fingerprint.short()),
        Err(e) => println!("NOT PAIRED: {e}"),
    }
}
