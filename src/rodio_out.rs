use std::{collections::VecDeque, time::Duration};

use anyhow::{self, bail, Context};
use crossbeam::channel::Sender;
use log::warn;
use rodio::{
    cpal::traits::HostTrait, Device, DeviceTrait, OutputStream, OutputStreamHandle, Sink, Source,
};
use slimproto::proto::AutoStart;

use crate::{
    audio_out::AudioOutput, decode::{Decoder, DecoderError}, message::PlayerMsg, StreamParams
};

const MIN_AUDIO_BUFFER_SIZE: usize = 4 * 1024;

pub struct DecoderSource {
    decoder: Decoder,
    frame: VecDeque<f32>,
    stream_params: StreamParams,
    stream_in: Sender<PlayerMsg>,
    start_flag: bool,
    eod_flag: bool,
}

impl DecoderSource {
    fn new(
        decoder: Decoder,
        stream_params: StreamParams,
        capacity: usize,
        stream_in: Sender<PlayerMsg>,
    ) -> Self {
        DecoderSource {
            decoder,
            frame: VecDeque::with_capacity(capacity),
            stream_params,
            stream_in,
            start_flag: true,
            eod_flag: false,
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
        if self.start_flag {
            self.stream_in.send(PlayerMsg::TrackStarted).ok();
            self.start_flag = false;
        }

        if self.frame.len() < MIN_AUDIO_BUFFER_SIZE && !self.eod_flag {
            let mut audio_buf = Vec::with_capacity(self.frame.capacity());
            loop {
                match self.decoder.fill_sample_buffer::<f32>(
                    &mut audio_buf,
                    Some(2 * MIN_AUDIO_BUFFER_SIZE),
                    self.stream_params.volume.clone(),
                ) {
                    Ok(()) => {}

                    Err(DecoderError::EndOfDecode) => {
                        if !self.eod_flag {
                            self.stream_in.send(PlayerMsg::EndOfDecode).ok();
                            self.eod_flag = true;
                        }
                    }

                    Err(DecoderError::StreamError(e)) => {
                        warn!("Error reading data stream: {}", e);
                        self.stream_in.send(PlayerMsg::NotSupported).ok();
                    }

                    Err(DecoderError::Retry) => {
                        continue;
                    }
                }

                if audio_buf.len() > 0 {
                    self.frame.extend(audio_buf);
                }
                break;
            }
        }

        self.frame.pop_front().or_else(|| {
            self.stream_in.send(PlayerMsg::Drained).ok();
            None
        })
    }
}

struct Stream {
    _output: OutputStream,
    _handle: OutputStreamHandle,
    sink: Sink,
}

impl Stream {
    fn try_from_device(device: &Device) -> anyhow::Result<Self> {
        let (output, handle) = OutputStream::try_from_device(device)?;
        let sink = Sink::try_new(&handle)?;
        Ok(Self {
            _output: output,
            _handle: handle,
            sink,
        })
    }

    fn play(&mut self, source: DecoderSource) {
        self.sink.append(source);
    }

    fn unpause(&self) {
        self.sink.play();
    }

    fn pause(&self) {
        self.sink.pause();
    }

    fn stop(&self) {
        self.sink.stop();
    }
}

pub struct RodioAudioOutput {
    host: rodio::cpal::Host,
    device: rodio::cpal::Device,
    playing: Option<Stream>,
}

impl RodioAudioOutput {
    pub fn try_new(device_name: &Option<String>) -> anyhow::Result<Self> {
        let host = rodio::cpal::default_host();
        let device = if let Some(dev_name) = device_name {
            match find_device(&host, &dev_name) {
                Some(device) => device,
                None => {
                    bail!("Cannot find device: {dev_name}");
                }
            }
        } else {
            host.default_output_device().context("No default device")?
        };

        Ok(Self {
            host,
            device,
            playing: None,
        })
    }
}

impl AudioOutput for RodioAudioOutput {
    fn enqueue_new_stream(
        &mut self,
        decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        stream_params: StreamParams,
        _device: &Option<String>,
    ) {
        let autostart = stream_params.autostart == AutoStart::Auto;

        let capacity = decoder.dur_to_samples(stream_params.output_threshold) as usize;
        let decoder_source =
            DecoderSource::new(decoder, stream_params, capacity, stream_in.clone());

        stream_in.send(PlayerMsg::StreamEstablished).ok();

        if let Some(ref mut playing_stream) = self.playing {
            playing_stream.play(decoder_source);
        } else {
            if let Ok(mut stream) = Stream::try_from_device(&self.device) {
                stream.play(decoder_source);
                if !autostart {
                    stream.pause();
                }
                self.playing = Some(stream);
            }
        }
    }

    fn unpause(&mut self) -> bool {
        if let Some(ref stream) = self.playing {
            (*stream).unpause();
            return true;
        }
        false
    }

    fn pause(&mut self) -> bool {
        if let Some(ref stream) = self.playing {
            (*stream).pause();
            return true;
        }
        false
    }

    fn stop(&mut self) {
        if let Some(ref stream) = self.playing {
            (*stream).stop();
        }
        self.flush();
    }

    fn flush(&mut self) {
        self.playing = None;
    }

    fn shift(&mut self) {
        // Noop - uses rodio's stream append
    }

    fn get_dur(&self) -> Duration {
        match self.playing {
            Some(ref stream) => stream.sink.get_pos(),
            None => Duration::ZERO,
        }
    }

    fn get_output_device_names(&self) -> anyhow::Result<Vec<(String, Option<String>)>> {
        let devices = self.host.output_devices()?;
        Ok(devices
            .map(|d| d.name())
            .filter(|n| n.is_ok())
            .map(|n| (n.unwrap(), None))
            .collect())
    }
}

fn find_device(host: &rodio::cpal::Host, name: &String) -> Option<Device> {
    let mut output_devices = host.output_devices().ok()?;
    output_devices.find(|d| match d.name() {
        Ok(n) => n == *name,
        Err(_) => false,
    })
}
