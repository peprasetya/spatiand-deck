//! Which machines this host has met.
//!
//! Pairing is done once, from a shell on the host, because that shell is already the proof of
//! who you are: anyone who can run `spatiand-host --pair` has an account here, and no scheme
//! invented on top of that would add anything.
//!
//! ```text
//! spatiand-host --fingerprint       # what this host is
//! spatiand-host --pair              # trust the next session that connects, for two minutes
//! spatiand-host --trust <60 hex>    # trust one by name
//! spatiand-host --paired            # who is trusted
//! spatiand-host --forget <8 hex>    # and undo it
//! ```
//!
//! This is deliberately *not* a PIN typed into the headset. A PIN exists so that two machines
//! with no shared context can authenticate over an untrusted network; ssh has already done
//! that job here, and better. When this has to work for somebody with no shell — a person
//! installing a package on a machine with a screen — a PIN is the thing to add, and the trust
//! store below does not change.
//!
//! **A window rather than a permanent invitation.** `--pair` trusts the *next* session to
//! connect and then closes, so a host is never left standing open. Two minutes is long enough
//! to put a headset on.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use spatiand_stream::Fingerprint;

/// How long `--pair` waits for a session to appear.
pub const PAIRING_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// The machines allowed to connect.
#[derive(Debug, Default, Clone)]
pub struct Paired {
    trusted: BTreeSet<String>,
}

impl Paired {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("spatiand-host").join("paired")
    }

    /// Read the list, or an empty one.
    ///
    /// Empty means *nobody*, and a host with an empty list serves nothing. That is the correct
    /// default: a machine that has never been paired has no way to know who ought to be
    /// allowed, and guessing from an address range is how people end up trusting a phone
    /// tether's carrier network.
    pub fn load(path: &Path) -> Paired {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut trusted = BTreeSet::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            match line.parse::<Fingerprint>() {
                Ok(_) => {
                    trusted.insert(line.to_lowercase());
                }
                Err(e) => log::warn!("{}: ignoring {line}: {e}", path.display()),
            }
        }
        Paired { trusted }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = String::from("# Sessions this host will serve. One fingerprint per line.\n");
        for fingerprint in &self.trusted {
            text.push_str(fingerprint);
            text.push('\n');
        }
        std::fs::write(path, text)
    }

    pub fn fingerprints(&self) -> Vec<Fingerprint> {
        self.trusted
            .iter()
            .filter_map(|text| text.parse().ok())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.trusted.is_empty()
    }

    pub fn contains(&self, fingerprint: &Fingerprint) -> bool {
        self.trusted.contains(&fingerprint.to_string())
    }

    /// Add one. Says whether it was new, so pairing twice can say so rather than look like it
    /// did something.
    pub fn trust(&mut self, fingerprint: &Fingerprint) -> bool {
        self.trusted.insert(fingerprint.to_string())
    }

    /// Remove whichever entry starts with `prefix` — the short form is what gets printed, and
    /// retyping sixty-four characters to undo something is its own kind of mistake.
    ///
    /// A prefix matching more than one is refused rather than guessed at.
    pub fn forget(&mut self, prefix: &str) -> Result<String, String> {
        let prefix = prefix.trim().to_lowercase();
        if prefix.len() < 4 {
            return Err("give at least four characters, or the wrong machine goes".into());
        }
        let matches: Vec<String> = self
            .trusted
            .iter()
            .filter(|f| f.starts_with(&prefix))
            .cloned()
            .collect();
        match matches.len() {
            0 => Err(format!("nothing paired here starts with {prefix}")),
            1 => {
                self.trusted.remove(&matches[0]);
                Ok(matches[0].clone())
            }
            n => Err(format!("{n} paired machines start with {prefix}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(byte: u8) -> Fingerprint {
        Fingerprint([byte; 32])
    }

    #[test]
    fn a_host_that_has_never_been_paired_trusts_nobody() {
        let paired = Paired::load(Path::new("/nonexistent/spatiand/paired"));
        assert!(paired.is_empty());
        assert!(paired.fingerprints().is_empty());
    }

    #[test]
    fn what_is_trusted_survives_a_trip_through_the_file() {
        let path = std::env::temp_dir().join(format!("spatiand-paired-{}", std::process::id()));
        let mut paired = Paired::default();
        assert!(paired.trust(&fingerprint(0xab)));
        assert!(!paired.trust(&fingerprint(0xab)), "the second time adds nothing");
        paired.trust(&fingerprint(0x01));
        paired.save(&path).unwrap();

        let back = Paired::load(&path);
        assert!(back.contains(&fingerprint(0xab)));
        assert!(back.contains(&fingerprint(0x01)));
        assert!(!back.contains(&fingerprint(0x02)));
        assert_eq!(back.fingerprints().len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_line_that_is_not_a_fingerprint_is_skipped_not_fatal() {
        let path = std::env::temp_dir().join(format!("spatiand-paired-bad-{}", std::process::id()));
        std::fs::write(
            &path,
            format!("# a comment\n\nnot-a-fingerprint\n{}\n", fingerprint(0x7f)),
        )
        .unwrap();
        let paired = Paired::load(&path);
        assert_eq!(paired.fingerprints().len(), 1);
        assert!(paired.contains(&fingerprint(0x7f)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn forgetting_takes_the_short_form_but_not_an_ambiguous_one() {
        let mut paired = Paired::default();
        paired.trust(&fingerprint(0xab));
        paired.trust(&fingerprint(0x01));
        assert!(paired.forget("ab").is_err(), "too short to be sure");
        assert!(paired.forget("abababab").is_ok());
        assert!(!paired.contains(&fingerprint(0xab)));
        assert!(paired.forget("abababab").is_err(), "and it is gone");
    }

    #[test]
    fn an_ambiguous_prefix_removes_nothing() {
        let mut paired = Paired::default();
        let mut one = [0u8; 32];
        one[0] = 0xaa;
        one[1] = 0xbb;
        let mut two = [0u8; 32];
        two[0] = 0xaa;
        two[1] = 0xbb;
        two[2] = 0x01;
        paired.trust(&Fingerprint(one));
        paired.trust(&Fingerprint(two));
        assert!(paired.forget("aabb").is_err());
        assert_eq!(paired.fingerprints().len(), 2);
    }
}
