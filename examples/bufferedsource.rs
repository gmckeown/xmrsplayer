use core::num::NonZero;
use rodio::Source;
use std::sync::{Arc, Mutex};
use xmrsplayer::xmrsplayer::XmrsPlayer;

pub const BUFFER_SIZE: usize = 2048;

pub struct BufferedSource<'a> {
    pub player: Arc<Mutex<XmrsPlayer<'a>>>,
    buffer: [f32; BUFFER_SIZE],
    buffer_index: usize,
    sample_rate: NonZero<u32>,
}

impl<'a> BufferedSource<'a> {
    pub fn new(player: Arc<Mutex<XmrsPlayer<'a>>>, sample_rate: u32) -> Self {
        BufferedSource {
            player,
            buffer: [0.0; BUFFER_SIZE],
            // Start past the end so the first `next()` triggers
            // `generate_samples()` before returning a value. Without this,
            // the first 2047 samples came out of the zero-initialised
            // buffer (~46 ms of silence at 44.1 kHz).
            buffer_index: BUFFER_SIZE,
            sample_rate: NonZero::new(sample_rate).expect("sample_rate must be non-zero"),
        }
    }

    fn generate_samples(&mut self) {
        let numsamples = self.buffer.len() / 2;
        for i in 0..numsamples {
            // Player API now produces `i16` PCM. rodio expects
            // f32 in [-1, 1] — convert at this demo boundary.
            match self.player.lock().unwrap().sample(true) {
                Some((left, right)) => {
                    self.buffer[2 * i] = left as f32 / i16::MAX as f32;
                    self.buffer[2 * i + 1] = right as f32 / i16::MAX as f32;
                }
                None => {}
            }
        }
    }
}

impl<'a> Source for BufferedSource<'a> {
    // rodio 0.21 renamed `current_frame_len` → `current_span_len`
    // (the renaming is part of their "frames are actually spans" cleanup).
    fn current_span_len(&self) -> Option<usize> {
        Some(BUFFER_SIZE - self.buffer_index)
    }
    // rodio 0.21: `channels` and `sample_rate` return NonZero<_>
    // (null values were always nonsensical — the type now forbids them).
    fn channels(&self) -> NonZero<u16> {
        // stereo, checked at compile time.
        NonZero::new(2).unwrap()
    }
    fn sample_rate(&self) -> NonZero<u32> {
        self.sample_rate
    }
    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

impl<'a> Iterator for BufferedSource<'a> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        // Refill first when we've drained the buffer, then read, then
        // advance. The previous code incremented before reading so
        // `buffer[0]` was never returned.
        if self.buffer_index >= BUFFER_SIZE {
            self.generate_samples();
            self.buffer_index = 0;
        }
        let sample = self.buffer[self.buffer_index];
        self.buffer_index += 1;
        Some(sample)
    }
}
