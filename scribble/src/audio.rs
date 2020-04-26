// The audio thread takes care of recording and playback. It has two methods of communication: it
// has an ExtEventSink for submitting commands (basically just recorded buffers) to the application,
// and it has a channel for receiving requests.

use cpal::traits::{EventLoopTrait, HostTrait};
use cpal::{EventLoop, StreamData, UnknownTypeInputBuffer, UnknownTypeOutputBuffer};
use druid::Data;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::thread;

use scribble_curves::{time, Diff, Time};

pub struct AudioState {
    event_loop: Arc<cpal::EventLoop>,
    input_device: cpal::Device,
    output_device: cpal::Device,
    format: cpal::Format,
    input_data: Arc<Mutex<AudioInput>>,
    output_data: Arc<Mutex<AudioOutput>>,
}

pub const SAMPLE_RATE: u32 = 48000;

#[derive(Deserialize, Serialize, Clone, Copy, Data, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AudioSnippetId(u64);

#[derive(Deserialize, Serialize, Clone, Data)]
pub struct AudioSnippetData {
    buf: Arc<Vec<i16>>,
    start_time: Time,
}

#[derive(Deserialize, Serialize, Clone, Data, Default)]
pub struct AudioSnippetsData {
    last_id: u64,
    snippets: Arc<BTreeMap<AudioSnippetId, AudioSnippetData>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CursorBuffer {
    id: AudioSnippetId,
    start: isize,
    len: isize,
}

#[derive(Default, Debug)]
pub struct Cursor {
    cur_idx: isize,
    all_cursors: Vec<CursorBuffer>,
    next_cursor: usize,
    active_cursors: Vec<CursorBuffer>,
}

#[derive(Debug)]
struct Buf<'a> {
    inner: &'a [i16],
    offset: usize,
    len: usize,
    direction: isize,
}

impl<'a> Buf<'a> {
    fn nonzero_start(&self) -> usize {
        self.offset
    }

    fn nonzero_end(&self) -> usize {
        self.offset + self.inner.len()
    }

    fn interpolated_index(&self, idx: f64) -> i16 {
        let idx0 = idx.floor() as isize;
        let idx1 = idx.ceil() as isize;
        let weight = idx - idx.floor();

        (self[idx0] as f64 * (1.0 - weight) + self[idx1] as f64 * weight) as i16
    }
}

// Signed indexing (negative numbers index from the end, and the sign must match `direction`).
// This panics if the sign of the index doesn't match `direction`, but otherwise it doesn't panic:
// if the requested index is out of bounds in either direction, we just return zero.
impl<'a> std::ops::Index<isize> for Buf<'a> {
    type Output = i16;

    fn index(&self, idx: isize) -> &i16 {
        if self.direction > 0 {
            assert!(idx >= 0);
        } else {
            assert!(idx <= 0);
        }

        let actual_idx = if self.direction > 0 {
            idx as usize
        } else {
            self.len
                .checked_sub((idx - 1).abs() as usize)
                .unwrap_or(std::usize::MAX)
        };

        if actual_idx < self.offset {
            &0
        } else {
            self.inner.get(actual_idx - self.offset).unwrap_or(&0i16)
        }
    }
}

impl CursorBuffer {
    fn new(
        id: AudioSnippetId,
        snip: &AudioSnippetData,
        time: Time,
        sample_rate: u32,
    ) -> CursorBuffer {
        CursorBuffer {
            id,
            start: (time - snip.start_time).as_audio_idx(sample_rate),
            len: snip.buf.len() as isize,
        }
    }

    fn get_buf<'a>(&mut self, data: &'a AudioSnippetsData, from: isize, amount: isize) -> Buf<'a> {
        if amount > 0 {
            debug_assert!(self.start + from < self.len);
            debug_assert!(self.start + from + amount > 0);
        } else {
            debug_assert!(self.start + from > 0);
            debug_assert!(self.start + from + amount < self.len);
        }

        let snip = data.snippet(self.id);
        let (start, end) = if amount > 0 {
            (self.start + from, self.start + from + amount)
        } else {
            (self.start + from + amount, self.start + from)
        };
        let offset = (-start).max(0) as usize;
        let start = start.max(0) as usize;
        let end = (end as usize).min(snip.buf.len());

        Buf {
            inner: &snip.buf[start..end],
            offset,
            len: amount.abs() as usize,
            direction: amount.signum(),
        }
    }

    fn is_active(&self, from: isize, amount: isize) -> bool {
        if amount > 0 {
            self.start + from + amount > 0
        } else {
            self.start + from + amount < self.len
        }
    }

    fn is_finished(&self, idx: isize, direction: isize) -> bool {
        if direction >= 0 {
            self.start + idx >= self.len
        } else {
            self.start + idx < 0
        }
    }

    fn is_started(&self, idx: isize, direction: isize) -> bool {
        if direction >= 0 {
            self.start + idx >= 0
        } else {
            self.start + idx < self.len
        }
    }
}

impl Cursor {
    pub fn new(
        snippets: &AudioSnippetsData,
        time: Time,
        sample_rate: u32,
        velocity: f64,
    ) -> Cursor {
        let mut cursors = Vec::new();
        let direction = velocity.signum() as isize;

        for (&id, snip) in snippets.snippets.iter() {
            cursors.push(CursorBuffer::new(id, snip, time, sample_rate));
        }
        // TODO: explain
        if direction == 1 {
            cursors.sort_by_key(|c| -c.start);
        } else {
            cursors.sort_by_key(|c| c.start - c.len);
        }

        let mut active = Vec::new();
        let mut next_cursor = cursors.len();
        for (c_idx, c) in cursors.iter().enumerate() {
            if !c.is_started(0, direction) {
                next_cursor = c_idx;
                break;
            }

            if !c.is_finished(0, direction) {
                active.push(*c);
            }
        }

        Cursor {
            cur_idx: 0,
            all_cursors: cursors,
            next_cursor,
            active_cursors: active,
        }
    }

    pub fn mix_to_buffer<B: DerefMut<Target = [i16]>>(
        &mut self,
        data: &AudioSnippetsData,
        mut buf: B,
        speed_factor: f64,
    ) {
        // How many bytes do we need from the input buffers? This is signed: it is negative
        // if we are playing backwards.
        let input_amount = (buf.len() as f64 * speed_factor).ceil() as isize;
        let direction = speed_factor.signum() as isize;

        while self.next_cursor < self.all_cursors.len() {
            if self.all_cursors[self.next_cursor].is_active(self.cur_idx, input_amount) {
                self.active_cursors.push(self.all_cursors[self.next_cursor]);
                self.next_cursor += 1;
            } else {
                break;
            }
        }

        // TODO: we do a lot of rounding here. Maybe we should work with floats internally?
        for c in &mut self.active_cursors {
            let in_buf = c.get_buf(data, self.cur_idx, input_amount);

            // TODO: we could be more efficient here, because we're potentially copying a bunch of
            // zeros from in_buf, whereas we could simply skip to the non-zero section. But it's
            // unlikely to be very expensive, whereas getting the indexing right is fiddly...
            for (out_idx, out_sample) in buf.iter_mut().enumerate() {
                let in_idx = out_idx as f64 * speed_factor;
                *out_sample += in_buf.interpolated_index(in_idx);
            }
        }
        self.cur_idx += input_amount;
        let cur_idx = self.cur_idx;
        self.active_cursors
            .retain(|c| !c.is_finished(cur_idx, direction));
    }

    pub fn is_finished(&self) -> bool {
        self.active_cursors.is_empty() && self.next_cursor == self.all_cursors.len()
    }
}

impl AudioState {
    pub fn init() -> AudioState {
        let host = cpal::default_host();
        let event_loop = host.event_loop();
        let input_device = host.default_input_device().expect("no input device");
        let output_device = host.default_output_device().expect("no input device");
        let format = cpal::Format {
            channels: 1,
            sample_rate: cpal::SampleRate(SAMPLE_RATE as u32),
            data_type: cpal::SampleFormat::I16,
        };

        let ret = AudioState {
            event_loop: Arc::new(event_loop),
            input_device,
            output_device,
            format,
            input_data: Arc::new(Mutex::new(AudioInput::default())),
            output_data: Arc::new(Mutex::new(AudioOutput::default())),
        };

        let event_loop = Arc::clone(&ret.event_loop);
        let input_data = Arc::clone(&ret.input_data);
        let output_data = Arc::clone(&ret.output_data);
        thread::spawn(move || audio_thread(event_loop, input_data, output_data));
        ret
    }

    pub fn set_velocity(&mut self, vel: f64) {
        self.output_data.lock().unwrap().speed_factor = vel;
    }

    pub fn start_recording(&mut self) {
        let input_stream = self
            .event_loop
            .build_input_stream(&self.input_device, &self.format)
            .expect("couldn't build input stream");

        {
            let mut input = self.input_data.lock().unwrap();
            assert!(input.id.is_none());
            input.id = Some(input_stream.clone());
            input.buf.clear();
        }

        self.event_loop
            .play_stream(input_stream)
            .expect("failed to play");
    }

    pub fn stop_recording(&mut self) -> Vec<i16> {
        let mut input_data = self.input_data.lock().unwrap();
        self.event_loop
            .destroy_stream(input_data.id.take().unwrap());

        let mut buf = Vec::new();
        std::mem::swap(&mut input_data.buf, &mut buf);

        process_audio(buf)
    }

    pub fn start_playing(&mut self, data: AudioSnippetsData, time: Time, velocity: f64) {
        let cursor = Cursor::new(&data, time, SAMPLE_RATE, velocity);
        let output_stream = self
            .event_loop
            .build_output_stream(&self.output_device, &self.format)
            .expect("couldn't build output stream");

        {
            let mut output = self.output_data.lock().unwrap();
            assert!(output.id.is_none());
            output.id = Some(output_stream.clone());
            output.bufs = data;
            output.speed_factor = velocity;
            output.cursor = cursor;
        }

        self.event_loop
            .play_stream(output_stream)
            .expect("failed to play");
    }

    pub fn stop_playing(&mut self) {
        let mut output = self.output_data.lock().unwrap();
        if output.id.is_some() {
            self.event_loop.destroy_stream(output.id.take().unwrap());
        }
    }
}

impl AudioSnippetData {
    pub fn new(buf: Vec<i16>, start_time: Time) -> AudioSnippetData {
        AudioSnippetData {
            buf: Arc::new(buf),
            start_time,
        }
    }

    pub fn buf(&self) -> &[i16] {
        &self.buf
    }

    pub fn start_time(&self) -> Time {
        self.start_time
    }

    pub fn end_time(&self) -> Time {
        let length = time::Diff::from_audio_idx(self.buf().len() as i64, SAMPLE_RATE);
        self.start_time() + length
    }
}

impl AudioSnippetsData {
    pub fn with_new_snippet(&self, snip: AudioSnippetData) -> AudioSnippetsData {
        let mut ret = self.clone();
        ret.last_id += 1;
        let id = AudioSnippetId(ret.last_id);
        let mut map = ret.snippets.deref().clone();
        map.insert(id, snip);
        ret.snippets = Arc::new(map);
        ret
    }

    pub fn snippet(&self, id: AudioSnippetId) -> &AudioSnippetData {
        self.snippets.get(&id).unwrap()
    }

    pub fn snippets(&self) -> impl Iterator<Item = (AudioSnippetId, &AudioSnippetData)> {
        self.snippets.iter().map(|(k, v)| (*k, v))
    }

    pub fn end_time(&self) -> Time {
        self.snippets
            .values()
            .map(|snip| snip.end_time())
            .max()
            .unwrap_or(time::ZERO)
    }
}

#[derive(Default)]
struct AudioInput {
    id: Option<cpal::StreamId>,
    buf: Vec<i16>,
}

#[derive(Default)]
struct AudioOutput {
    id: Option<cpal::StreamId>,
    speed_factor: f64,
    cursor: Cursor,
    bufs: AudioSnippetsData,
}

fn audio_thread(
    event_loop: Arc<EventLoop>,
    input: Arc<Mutex<AudioInput>>,
    output: Arc<Mutex<AudioOutput>>,
) {
    event_loop.run(move |stream_id, stream_data| {
        let stream_data = stream_data.expect("error on input stream");
        match stream_data {
            StreamData::Output {
                buffer: UnknownTypeOutputBuffer::I16(mut buf),
            } => {
                let mut output_data = output.lock().unwrap();
                let output_data = output_data.deref_mut();
                if output_data.id.as_ref() != Some(&stream_id) {
                    return;
                }
                for elem in buf.iter_mut() {
                    *elem = 0;
                }
                output_data
                    .cursor
                    .mix_to_buffer(&output_data.bufs, buf, output_data.speed_factor);
            }
            StreamData::Input {
                buffer: UnknownTypeInputBuffer::I16(buf),
            } => {
                let mut input_data = input.lock().unwrap();
                if input_data.id != Some(stream_id) {
                    return;
                }
                input_data.buf.extend_from_slice(&buf);
            }
            _ => {
                panic!("unexpected data");
            }
        }
    });
}

// Processes the recorded audio.
// - Truncates the beginning and end a little bit (to remove to sound of the user pressing the keyboard to start/stop recording).
// - Runs noise removal using RNNoise.
const TRUNCATION_LEN: Diff = Diff::from_micros(100_000);
fn process_audio(mut buf: Vec<i16>) -> Vec<i16> {
    let trunc_samples = TRUNCATION_LEN.as_audio_idx(SAMPLE_RATE) as usize;
    if buf.len() <= 2 * trunc_samples {
        return Vec::new();
    }
    // We truncate the end of the buffer, but instead of truncating the beginning we set
    // it all to zero (because if we truncate it, it messes with the synchronization between
    // audio and animation).
    for i in 0..trunc_samples {
        buf[i] = 0;
    }

    // Truncate the buffer and convert it to f32 also, since that's what RNNoise wants.
    let buf_end = buf.len() - trunc_samples;
    let mut float_buf: Vec<f32> = buf[..buf_end].iter().map(|x| *x as f32).collect();

    // RNNoise likes the input to be a multiple of FRAME_SIZE.
    let fs = rnnoise_c::FRAME_SIZE;
    let new_size = ((float_buf.len() + (fs - 1)) / fs) * fs;
    float_buf.resize(new_size, 0.0);
    let mut out_buf = vec![0.0f32; float_buf.len()];
    let mut state = rnnoise_c::DenoiseState::new();
    for (in_chunk, out_chunk) in float_buf.chunks_exact(fs).zip(out_buf.chunks_exact_mut(fs)) {
        state.process_frame_mut(in_chunk, out_chunk);
    }
    out_buf.into_iter().map(|x| x as i16).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! snips {
        ($($time:expr => $buf:expr),*) => {
            {
                let mut ret = AudioSnippetsData::default();
                $(
                    let buf: &[i16] = $buf;
                    let time = Time::from_micros($time * 1000000);
                    ret = ret.with_new_snippet(AudioSnippetData::new(buf.to_owned(), time));
                )*

                ret
            }
        }
    }

    #[test]
    fn speed_1() {
        let snips = snips!(0 => &[1, 2, 3, 4, 5]);
        // a sample rate of 1 is silly, but it lets us get the indices right without any rounding issues.
        let mut c = Cursor::new(&snips, time::ZERO, 1, 1.0);
        let mut out = vec![0; 5];
        c.mix_to_buffer(&snips, &mut out[..], 1.0);
        assert_eq!(out, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn speed_1_offset() {
        let snips = snips!(5 => &[1, 2, 3, 4, 5]);
        let mut c = Cursor::new(&snips, time::ZERO, 1, 1.0);
        let mut out = vec![0; 15];
        c.mix_to_buffer(&snips, &mut out[..], 1.0);
        assert_eq!(out, vec![0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn speed_2() {
        let snips = snips!(0 => &[1, 2, 3, 4, 5, 6]);
        let mut c = Cursor::new(&snips, time::ZERO, 1, 2.0);
        let mut out = vec![0; 6];
        c.mix_to_buffer(&snips, &mut out[..], 2.0);
        assert_eq!(out, vec![1, 3, 5, 0, 0, 0]);
    }

    #[test]
    fn speed_2_offset() {
        let snips = snips!(2 => &[1, 2, 3, 4, 5, 6]);
        let mut c = Cursor::new(&snips, time::ZERO, 1, 2.0);
        let mut out = vec![0; 6];
        c.mix_to_buffer(&snips, &mut out[..], 2.0);
        assert_eq!(out, vec![0, 1, 3, 5, 0, 0]);
    }

    #[test]
    fn backwards_1() {
        let snips = snips!(2 => &[1, 2, 3, 4, 5]);
        let mut c = Cursor::new(&snips, Time::from_micros(9 * 1000000), 1, -1.0);
        let mut out = vec![0; 10];
        c.mix_to_buffer(&snips, &mut out[..], -1.0);
        assert_eq!(out, vec![0, 0, 5, 4, 3, 2, 1, 0, 0, 0]);
    }

    #[test]
    fn backwards_2() {
        let snips = snips!(2 => &[1, 2, 3, 4, 5]);
        let mut c = Cursor::new(&snips, Time::from_micros(9 * 1000000), 1, -2.0);
        let mut out = vec![0; 10];
        c.mix_to_buffer(&snips, &mut out[..], -2.0);
        assert_eq!(out, vec![0, 5, 3, 1, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn multiple_snippets() {
        let snips = snips!(
            0 => &[1, 2, 3],
            2 => &[1, 2, 3]
        );
        let mut c = Cursor::new(&snips, time::ZERO, 1, 1.0);
        let mut out = vec![0; 10];
        c.mix_to_buffer(&snips, &mut out[..], 1.0);
        assert_eq!(out, vec![1, 2, 4, 2, 3, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn multiple_snippets_backwards() {
        let snips = snips!(
            0 => &[1, 2, 3],
            2 => &[1, 2, 3]
        );
        let mut c = Cursor::new(&snips, Time::from_micros(10 * 1000000), 1, -1.0);
        let mut out = vec![0; 10];
        c.mix_to_buffer(&snips, &mut out[..], -1.0);
        assert_eq!(out, vec![0, 0, 0, 0, 0, 3, 2, 4, 2, 1]);
    }

    #[test]
    fn non_overlapping_snippets() {
        let snips = snips!(
            0 => &[1, 2, 3],
            12 => &[1, 2, 3]
        );
        let mut c = Cursor::new(&snips, time::ZERO, 1, 1.0);
        let mut out = vec![0; 10];
        c.mix_to_buffer(&snips, &mut out[..], 1.0);
        assert_eq!(out, vec![1, 2, 3, 0, 0, 0, 0, 0, 0, 0]);

        let mut out = vec![0; 10];
        c.mix_to_buffer(&snips, &mut out[..], 1.0);
        assert_eq!(out, vec![0, 0, 1, 2, 3, 0, 0, 0, 0, 0]);
    }
}