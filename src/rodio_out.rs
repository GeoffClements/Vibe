use std::sync::{Arc, Mutex};

use anyhow;
use crossbeam::channel::Sender;
use rodio::{cpal::traits::HostTrait, DeviceTrait, Source};
use slimproto::status::StatusData;

use crate::{decode::Decoder, PlayerMsg, StreamParams};

pub struct DecoderSource {
    decoder: Decoder,
    frame: Vec<f32>,
    idx: usize,
    volume: Arc<Mutex<Vec<f32>>>,
}

impl DecoderSource {
    fn new(decoder: Decoder, volume: Arc<Mutex<Vec<f32>>>) -> Self {
        DecoderSource {
            decoder,
            frame: Vec::with_capacity(8 * 1024),
            idx: 0,
            volume: volume.clone(),
        }
    }
}

impl Source for DecoderSource {
    fn current_frame_len(&self) -> Option<usize> {
        match self.frame.len() {
            0 => None,
            len => Some(len),
        }
    }

    fn channels(&self) -> u16 {
        self.decoder.channels() as u16
    }

    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

impl Iterator for DecoderSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.frame.len() == 0 {
            self.decoder
                .fill_sample_buffer::<f32>(&mut self.frame, None, self.volume.clone())
                .ok()?;
            self.idx = 0;
        }

        let ret = match self.frame.len() {
            0 => None,
            _ => Some(self.frame[self.idx]),
        };
        self.idx += 1;
        ret
    }
}

pub struct AudioOutput;

impl AudioOutput {
    pub fn try_new() -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub fn enqueue_new_stream(
        &mut self,
        mut decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        status: Arc<Mutex<StatusData>>,
        stream_params: StreamParams,
        device: &Option<String>,
    ) {
    }

    pub fn unpause(&mut self) -> bool {
        true
    }

    pub fn pause(&mut self) -> bool {
        true
    }

    pub fn stop(&mut self) {}

    pub fn flush(&mut self) {}

    pub fn shift(&mut self) {}
}

pub fn get_output_device_names() -> anyhow::Result<Vec<String>> {
    let host = rodio::cpal::default_host();
    let devices = host.output_devices()?;
    let ret = devices
        .map(|d| d.name())
        .filter(|n| n.is_ok())
        .map(|n| n.unwrap())
        .collect();
    Ok(ret)
}
