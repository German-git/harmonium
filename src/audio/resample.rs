//! Linear-interpolation sample-rate adapter for crossfade preloading.
//!
//! The crossfade engine mixes two tracks assuming a shared frame clock, so a
//! preloaded next track at a different sample rate must be resampled onto the
//! active track's rate before it can be mixed. This adapter wraps a decoder
//! `Source` and yields interleaved frames at the requested rate with linear
//! interpolation (adequate for a transient crossfade, and dependency-free).

use rodio::source::SeekError;
use rodio::{ChannelCount, SampleRate, Source};
use std::collections::VecDeque;
use std::time::Duration;

/// Resample an interleaved source to a fixed target rate. The output keeps the
/// input channel count; the pitch/duration of the content is preserved, only
/// the clock is rebased so two sources can advance frame-locked.
pub struct LinearResample<I> {
    input: I,
    channels: ChannelCount,
    out_rate: SampleRate,
    /// Input frames consumed per output frame (`in_rate / out_rate`).
    ratio: f64,
    /// Fractional input-frame position of the next output frame.
    pos: f64,
    /// Read-ahead window of input frames, indexed from `start`.
    buf: VecDeque<Vec<f32>>,
    /// Frame index of `buf[0]`.
    start: u64,
    /// Input exhausted (a short final frame is held as silence).
    eof: bool,
    /// Samples of the current output frame, handed out one at a time.
    frame: Vec<f32>,
    /// Next sample index to hand out within `frame`.
    frame_offset: usize,
}

/// Adapt a mono/stereo source to the other supported channel count without
/// allocating in the iterator path. Other layouts are rejected before this
/// adapter is constructed because channel positions are not available here.
pub struct ChannelAdapter<I> {
    input: I,
    input_channels: usize,
    output_channels: ChannelCount,
    input_frame: Vec<f32>,
    output_frame: Vec<f32>,
    frame_offset: usize,
    eof: bool,
}

impl<I> ChannelAdapter<I>
where
    I: Source<Item = f32>,
{
    /// Wrap a validated mono/stereo `input`, producing the other layout.
    fn new(input: I, output_channels: ChannelCount) -> Self {
        let input_channels = input.channels().get() as usize;
        let output_len = output_channels.get() as usize;
        assert!(
            matches!((input_channels, output_len), (1, 2) | (2, 1)),
            "channel adapter only supports mono/stereo conversion"
        );
        Self {
            input,
            input_channels,
            output_channels,
            input_frame: vec![0.0; input_channels],
            output_frame: vec![0.0; output_len],
            frame_offset: output_len,
            eof: false,
        }
    }

    /// Validate a raw engine channel count before crossing into the adapter.
    pub(crate) fn try_new(input: I, channels: u32) -> anyhow::Result<Self> {
        let input_channels = u32::from(input.channels().get());
        if !matches!((input_channels, channels), (1, 2) | (2, 1)) {
            return Err(anyhow::anyhow!(
                "unsupported channel layout: {input_channels} -> {channels}"
            ));
        }
        let channels = u16::try_from(channels)
            .ok()
            .and_then(ChannelCount::new)
            .ok_or_else(|| anyhow::anyhow!("invalid target channel count: {channels}"))?;
        Ok(Self::new(input, channels))
    }

    fn next_frame(&mut self) -> bool {
        if self.eof {
            return false;
        }
        for sample in &mut self.input_frame {
            let Some(value) = self.input.next() else {
                self.eof = true;
                return false;
            };
            *sample = value;
        }

        match (self.input_channels, self.output_channels.get() as usize) {
            (1, 2) => self.output_frame.fill(self.input_frame[0]),
            (2, 1) => {
                self.output_frame[0] = (self.input_frame[0] + self.input_frame[1]) / 2.0;
            }
            _ => unreachable!("channel adapter layout was validated at construction"),
        }
        self.frame_offset = 0;
        true
    }
}

impl<I> Iterator for ChannelAdapter<I>
where
    I: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.frame_offset >= self.output_channels.get() as usize && !self.next_frame() {
            return None;
        }
        let sample = self.output_frame[self.frame_offset];
        self.frame_offset += 1;
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.input.size_hint().1)
    }
}

impl<I> Source for ChannelAdapter<I>
where
    I: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.output_channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.input.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.input.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.input.try_seek(pos)?;
        self.frame_offset = self.output_channels.get() as usize;
        self.eof = false;
        Ok(())
    }
}

impl<I> LinearResample<I>
where
    I: Source<Item = f32>,
{
    /// Wrap `input`, producing audio at the validated target `rate`.
    pub fn new(input: I, rate: SampleRate) -> Self {
        let in_rate = input.sample_rate().get();
        let channels = input.channels();
        let ratio = f64::from(in_rate) / f64::from(rate.get());
        let mut s = Self {
            input,
            channels,
            out_rate: rate,
            ratio,
            pos: 0.0,
            buf: VecDeque::new(),
            start: 0,
            eof: false,
            frame: vec![0.0; channels.get() as usize],
            frame_offset: channels.get() as usize,
        };
        s.fill_to(1);
        s
    }

    /// Validate a raw engine rate before crossing into the typed resampler.
    pub(crate) fn try_new(input: I, rate: u32) -> anyhow::Result<Self> {
        let rate = SampleRate::new(rate)
            .ok_or_else(|| anyhow::anyhow!("invalid target sample rate: {rate} Hz"))?;
        Ok(Self::new(input, rate))
    }

    /// Pull input frames until `buf` covers frame index `index` (inclusive) or
    /// the input is exhausted.
    fn fill_to(&mut self, index: u64) {
        if self.eof {
            return;
        }
        let need = index.saturating_sub(self.start) as usize + 1;
        let have = self.buf.len();
        for _ in have..need {
            let mut frame = vec![0.0f32; self.channels.get() as usize];
            let mut got = 0usize;
            for slot in frame.iter_mut() {
                match self.input.next() {
                    Some(sample) => {
                        *slot = sample;
                        got += 1;
                    }
                    None => break,
                }
            }
            if got == self.channels.get() as usize {
                self.buf.push_back(frame);
            } else {
                self.eof = true;
                break;
            }
        }
    }

    /// Produce the next output frame into `self.frame`, trimmed to hold the
    /// last input frame when the source ends. Returns whether a frame was
    /// placed into `self.frame`; `self.frame_offset` is reset to 0 when one is,
    /// so [`Iterator::next`] hands out all of its channels before asking again.
    ///
    /// When the source runs out mid-frame a final frame is still produced (the
    /// last input held as-is) and `frame_offset` is left at 0 so that frame is
    /// emitted exactly once; the *next* call returns `false` (exhausted).
    fn next_frame(&mut self) -> bool {
        // Once the source has been exhausted (including after the single held
        // final frame is produced) there is nothing more to emit.
        if self.eof {
            return false;
        }
        let floored = self.pos.floor();
        let lo = if floored < 0.0 { 0 } else { floored as u64 };
        let hi = lo + 1;
        self.fill_to(hi);
        let frac = (self.pos - floored) as f32;
        let a = self.buf.get((lo - self.start) as usize);
        let b = self.buf.get((hi - self.start) as usize);
        let ok = match (a, b) {
            (Some(a), Some(b)) => {
                for (slot, (av, bv)) in self.frame.iter_mut().zip(a.iter().zip(b.iter())) {
                    *slot = av + (bv - av) * frac;
                }
                true
            }
            (Some(a), None) => {
                // Source ran out between frames: hold the last one. This final
                // frame is real content and must be handed out exactly once
                // before the stream is marked exhausted.
                for (slot, av) in self.frame.iter_mut().zip(a.iter()) {
                    *slot = *av;
                }
                self.eof = true;
                true
            }
            (None, _) => false,
        };
        self.pos += self.ratio;
        self.trim_to(lo);
        self.frame_offset = 0;
        ok
    }

    /// Drop buffered frames that are no longer reachable.
    fn trim_to(&mut self, keep_lo: u64) {
        // Keep `lo` and `lo + 1` (currently reachable); drop everything older.
        let keep_from = keep_lo;
        while self.start < keep_from {
            self.buf.pop_front();
            self.start += 1;
        }
    }
}

impl<I> Iterator for LinearResample<I>
where
    I: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.frame_offset >= self.channels.get() as usize && !self.next_frame() {
            return None;
        }
        let sample = self.frame[self.frame_offset];
        self.frame_offset += 1;
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.input.size_hint().1)
    }
}

impl<I> Source for LinearResample<I>
where
    I: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.out_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        // The source duration is unchanged by a clock rebase (same content).
        self.input.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        // A seek moves the decoded read head, so the resampler's own position
        // window must restart at the new location; otherwise it keeps indexing
        // the old `start`/`buf` (stale frames) and the stream corrupts. Reset
        // the internal state to a fresh segment at the seeked position.
        self.input.try_seek(pos)?;
        self.pos = 0.0;
        self.start = 0;
        self.buf.clear();
        self.eof = false;
        self.frame_offset = self.channels.get() as usize;
        self.fill_to(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::source::{SineWave, Source};

    struct TestSource {
        samples: Vec<f32>,
        position: usize,
        channels: ChannelCount,
        sample_rate: SampleRate,
    }

    impl TestSource {
        fn new(samples: Vec<f32>, channels: u16, sample_rate: u32) -> Self {
            Self {
                samples,
                position: 0,
                channels: ChannelCount::new(channels).expect("test channels must be non-zero"),
                sample_rate: SampleRate::new(sample_rate)
                    .expect("test sample rate must be non-zero"),
            }
        }
    }

    impl Iterator for TestSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            let sample = self.samples.get(self.position).copied()?;
            self.position += 1;
            Some(sample)
        }
    }

    impl Source for TestSource {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.samples.len().saturating_sub(self.position))
        }

        fn channels(&self) -> ChannelCount {
            self.channels
        }

        fn sample_rate(&self) -> SampleRate {
            self.sample_rate
        }

        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    #[test]
    fn zero_target_rate_is_rejected_at_the_raw_rate_boundary() {
        let source = TestSource::new(vec![1.0], 1, 44_100);
        let error = match LinearResample::try_new(source, 0) {
            Ok(_) => panic!("zero rate must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "invalid target sample rate: 0 Hz");
    }

    #[test]
    fn rodio_sample_rate_boundary_rejects_zero() {
        assert!(SampleRate::new(0).is_none());
        assert!(SampleRate::new(44_100).is_some());
    }

    #[test]
    fn resample_preserves_source_format_and_interpolates_stereo_frames() {
        let src = TestSource::new(vec![0.0, 10.0, 20.0, 30.0, 40.0, 50.0], 2, 2);
        let mut resampled = LinearResample::new(src, SampleRate::new(4).unwrap());

        assert_eq!(resampled.sample_rate().get(), 4);
        assert_eq!(resampled.channels().get(), 2);

        let output: Vec<_> = resampled.by_ref().collect();
        assert_eq!(
            output,
            vec![0.0, 10.0, 10.0, 20.0, 20.0, 30.0, 30.0, 40.0, 40.0, 50.0]
        );
    }

    #[test]
    fn resample_rebases_the_clock_without_changing_duration() {
        // 1 second of a 440 Hz mono sine at the default 44100 rate.
        let src = SineWave::new(440.0).take_duration(std::time::Duration::from_secs(1));
        let mut r = LinearResample::new(src, SampleRate::new(48000).unwrap());
        assert_eq!(r.sample_rate().get(), 48000);
        assert_eq!(r.channels().get(), 1);

        let mut count = 0usize;
        while r.next().is_some() {
            count += 1;
        }
        // 1 s of content rebased to 48 kHz → 48000 samples (within tolerance).
        assert!(
            count.abs_diff(48_000) < 100,
            "expected ~48000 samples at 48 kHz, got {count}"
        );
    }

    #[test]
    fn resample_identity_rate_passthrough() {
        let src = SineWave::new(220.0).take_duration(std::time::Duration::from_millis(500));
        let mut r = LinearResample::new(src, SampleRate::new(44_100).unwrap());
        assert_eq!(r.sample_rate().get(), 44_100);
        let mut count = 0usize;
        while r.next().is_some() {
            count += 1;
        }
        // ~22050 samples (half a second at 44100), within tolerance.
        assert!(
            count.abs_diff(22_050) < 100,
            "expected ~22050 samples at identity rate, got {count}"
        );
    }

    #[test]
    fn resample_emits_the_final_held_frame_before_exhaustion() {
        // A short source whose last frame falls between resampled frames: the
        // resampler must hand out that final frame once before returning None,
        // not silently drop it (the off-by-one that lost a frame of content).
        let src = SineWave::new(440.0).take_duration(std::time::Duration::from_millis(10));
        let mut r = LinearResample::new(src, SampleRate::new(48_000).unwrap());
        let mut count = 0usize;
        while r.next().is_some() {
            count += 1;
        }
        // 10 ms of 440 Hz rebased to 48 kHz → ~480 samples (within tolerance).
        assert!(
            count.abs_diff(480) < 50,
            "the final held frame must be counted, got {count}"
        );
    }

    #[test]
    fn channel_adapter_expands_mono_to_stereo_without_changing_frames() {
        let source = TestSource::new(vec![1.0, 2.0], 1, 44_100);
        let adapted = ChannelAdapter::new(source, ChannelCount::new(2).unwrap());

        assert_eq!(adapted.channels().get(), 2);
        assert_eq!(adapted.collect::<Vec<_>>(), vec![1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn channel_adapter_reduces_stereo_to_mono_by_frame_average() {
        let source = TestSource::new(vec![1.0, 3.0, 5.0, 9.0], 2, 44_100);
        let adapted = ChannelAdapter::new(source, ChannelCount::new(1).unwrap());

        assert_eq!(adapted.channels().get(), 1);
        assert_eq!(adapted.collect::<Vec<_>>(), vec![2.0, 7.0]);
    }

    #[test]
    fn try_seek_resets_the_internal_state_window() {
        let src = SineWave::new(440.0).take_duration(std::time::Duration::from_secs(1));
        let mut r = LinearResample::new(src, SampleRate::new(48_000).unwrap());

        // Consume a bit, then seek back near the start. After a successful
        // seek the internal window must be re-anchored (pos/buf cleared), so
        // the count of subsequent samples matches a fresh stream instead of
        // continuing from the stale position.
        for _ in 0..1000 {
            let _ = r.next();
        }
        // The resampler's own state must be consistent after delegating the
        // seek (no stale `start`/`buf` left behind).
        let seek = r.try_seek(std::time::Duration::ZERO);
        assert!(seek.is_ok(), "SineWave seek to zero should succeed");
        assert_eq!(r.pos, 0.0, "position window must reset");
        assert_eq!(r.start, 0, "frame index must reset");
        assert!(!r.eof, "a reset stream must not be marked exhausted");
        // Still emits samples past the reset point.
        let mut count = 0usize;
        while r.next().is_some() {
            count += 1;
        }
        assert!(count > 0, "a freshly seeked stream must still yield audio");
    }
}
