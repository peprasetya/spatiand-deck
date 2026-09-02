//! A queue between the thread that renders a window's sound and the thread that plays it.
//!
//! There are two of them and they are not in step. A window's audio arrives when its app
//! decides to produce some; the glasses ask for audio when the sound card's clock says so.
//! Neither may wait for the other — both are real-time callbacks where blocking is a dropout —
//! so what sits between them has to be a queue that never locks, never allocates, and never
//! makes either side wait for the other to finish.
//!
//! Exactly one thread writes and exactly one reads, which is what makes this possible without
//! locks: each side owns one of the two counters, so neither ever has to modify what the other
//! is reading. The counters run forever and are wrapped only when used, so a full ring and an
//! empty one are never mistaken for each other — the bug that a plain pair of wrapped indices
//! invites.
//!
//! ## What happens when it runs dry, and why that is the right answer
//!
//! An under-run is filled with silence rather than by waiting or by repeating what came
//! before. Waiting would stall the sound card and take out every other window's audio with it,
//! and repeating turns a gap into a buzz. A window whose app has stopped producing should go
//! quiet, which is exactly what silence does.
//!
//! An over-run drops the oldest audio instead of the newest. A window that produces faster than
//! the card consumes is drifting, and the listener would rather lose a moment than fall
//! progressively further behind the picture.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A single-producer, single-consumer queue of samples.
pub struct Ring {
    /// Only ever touched through the two disciplines below: the writer writes the range it
    /// owns, the reader reads the range it owns, and the counters keep those ranges apart.
    samples: UnsafeCell<Box<[f32]>>,
    /// How many samples have ever been written. Owned by the producer.
    written: AtomicUsize,
    /// How many samples have ever been read. Owned by the consumer.
    read: AtomicUsize,
    capacity: usize,
}

// SAFETY: the two counters partition the buffer between the producer and the consumer so that
// no byte is ever written by one while being read by the other, and the acquire/release pairs
// below make each side's writes visible to the other before the index that exposes them.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    /// A ring holding `capacity` samples, rounded up to a power of two so wrapping is a mask.
    pub fn new(capacity: usize) -> Ring {
        let capacity = capacity.next_power_of_two().max(2);
        Ring {
            samples: UnsafeCell::new(vec![0.0; capacity].into_boxed_slice()),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many samples are waiting to be read.
    pub fn available(&self) -> usize {
        self.written
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire))
    }

    /// Add samples, dropping the oldest if there is no room.
    ///
    /// Returns how many had to be dropped, which is worth logging: a ring that keeps
    /// overflowing means the two clocks have genuinely diverged rather than merely jittered.
    pub fn write(&self, input: &[f32]) -> usize {
        let written = self.written.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let free = self.capacity - written.wrapping_sub(read);

        // More than the whole ring: only the tail could ever be heard anyway. Whatever is
        // trimmed here counts as dropped just as much as what is pushed out below -- it is
        // audio that was handed over and will not be played -- and leaving it out of the
        // count would report a healthy ring while it threw sound away.
        let (input, trimmed) = if input.len() > self.capacity {
            (
                &input[input.len() - self.capacity..],
                input.len() - self.capacity,
            )
        } else {
            (input, 0)
        };
        let dropped = trimmed + input.len().saturating_sub(free);

        // SAFETY: the producer owns every slot from `written` up to `read + capacity`, and
        // `input` has been trimmed above so it cannot reach past that.
        let samples = unsafe { &mut *self.samples.get() };
        for (i, &s) in input.iter().enumerate() {
            samples[written.wrapping_add(i) % self.capacity] = s;
        }
        self.written
            .store(written.wrapping_add(input.len()), Ordering::Release);

        if dropped > 0 {
            // The oldest audio is now behind the reader; move it up so the reader does not
            // read samples that have been overwritten underneath it.
            self.read.store(
                written.wrapping_add(input.len()) - self.capacity,
                Ordering::Release,
            );
        }
        dropped
    }

    /// Fill `out` from the ring, padding with silence if there is not enough.
    ///
    /// Returns how many samples were short. Silence rather than a stall: a window whose app
    /// has stopped producing should go quiet, and making the sound card wait would take out
    /// every other window with it.
    pub fn read(&self, out: &mut [f32]) -> usize {
        let read = self.read.load(Ordering::Relaxed);
        let written = self.written.load(Ordering::Acquire);
        let ready = written.wrapping_sub(read).min(out.len());

        // SAFETY: the consumer owns every slot from `read` up to `written`, and `ready` is
        // clamped to that range.
        let samples = unsafe { &*self.samples.get() };
        for (i, slot) in out[..ready].iter_mut().enumerate() {
            *slot = samples[read.wrapping_add(i) % self.capacity];
        }
        out[ready..].fill(0.0);
        self.read.store(read.wrapping_add(ready), Ordering::Release);
        out.len() - ready
    }

    /// Throw away everything waiting, for when a window's sound is no longer wanted.
    pub fn clear(&self) {
        self.read
            .store(self.written.load(Ordering::Acquire), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_goes_in_comes_out_in_order() {
        let r = Ring::new(16);
        assert_eq!(r.write(&[1.0, 2.0, 3.0]), 0);
        let mut out = [0.0; 3];
        assert_eq!(r.read(&mut out), 0);
        assert_eq!(out, [1.0, 2.0, 3.0]);
        assert_eq!(r.available(), 0);
    }

    #[test]
    fn reading_more_than_there_is_gives_silence_rather_than_stale_audio() {
        // The under-run case, which is a window whose app has gone quiet. Repeating the last
        // buffer would turn a gap into a buzz.
        let r = Ring::new(16);
        r.write(&[1.0, 2.0]);
        let mut out = [9.0; 5];
        assert_eq!(r.read(&mut out), 3, "should have reported the shortfall");
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn an_empty_ring_reads_as_silence() {
        let r = Ring::new(8);
        let mut out = [1.0; 4];
        assert_eq!(r.read(&mut out), 4);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn overflowing_drops_the_oldest_and_says_so() {
        // A window running fast should lose a moment rather than fall ever further behind the
        // picture, so the newest audio is the audio that survives.
        let r = Ring::new(4);
        assert_eq!(r.write(&[1.0, 2.0, 3.0, 4.0]), 0);
        assert_eq!(r.write(&[5.0, 6.0]), 2);
        let mut out = [0.0; 4];
        r.read(&mut out);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn a_write_larger_than_the_whole_ring_keeps_its_tail() {
        let r = Ring::new(4);
        let dropped = r.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(
            dropped, 2,
            "the two trimmed samples were not counted as dropped"
        );
        let mut out = [0.0; 4];
        r.read(&mut out);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn the_indices_survive_running_round_many_times() {
        // A plain pair of wrapped indices cannot tell a full ring from an empty one, and the
        // symptom is audio that is fine for hours and then silent. These count forever and
        // are wrapped only where they are used.
        let r = Ring::new(8);
        let mut out = [0.0; 3];
        for round in 0..1000 {
            let v = round as f32;
            r.write(&[v, v + 0.1, v + 0.2]);
            assert_eq!(r.read(&mut out), 0, "starved on round {round}");
            assert_eq!(out, [v, v + 0.1, v + 0.2], "wrong on round {round}");
        }
    }

    #[test]
    fn clearing_leaves_nothing_behind() {
        let r = Ring::new(8);
        r.write(&[1.0, 2.0, 3.0]);
        r.clear();
        assert_eq!(r.available(), 0);
        let mut out = [9.0; 2];
        assert_eq!(r.read(&mut out), 2);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn a_writer_and_a_reader_on_two_threads_agree_on_every_sample() {
        // The claim the whole type rests on. Run under a thread sanitiser this is where a
        // missing acquire or release would show up; run plainly it still catches an index
        // that is simply wrong.
        use std::sync::Arc;
        const TOTAL: usize = 200_000;
        let ring = Arc::new(Ring::new(1024));
        let writer = {
            let ring = Arc::clone(&ring);
            std::thread::spawn(move || {
                let mut n = 0usize;
                while n < TOTAL {
                    let chunk: Vec<f32> = (0..64).map(|i| (n + i) as f32).collect();
                    // Wait for room rather than dropping, so the test can check every sample.
                    while ring.capacity() - ring.available() < chunk.len() {
                        std::hint::spin_loop();
                    }
                    ring.write(&chunk);
                    n += 64;
                }
            })
        };
        let mut expect = 0usize;
        let mut out = [0.0f32; 64];
        while expect < TOTAL {
            if ring.available() >= out.len() {
                assert_eq!(ring.read(&mut out), 0);
                for (i, got) in out.iter().enumerate() {
                    assert_eq!(*got, (expect + i) as f32, "at sample {}", expect + i);
                }
                expect += out.len();
            } else {
                std::hint::spin_loop();
            }
        }
        writer.join().unwrap();
    }
}
