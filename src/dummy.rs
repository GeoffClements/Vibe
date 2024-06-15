use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use crossbeam::{atomic::AtomicCell, channel::Sender};
use log::{info, warn};
use slimproto::status::StatusData;

use crate::{
    decode::{self, DecoderError},
    PlayerMsg,
};

type Callback = Box<dyn FnMut() + Send + Sync + 'static>;

pub struct Stream {
    write_callback: Arc<Mutex<Callback>>,
    streaming: Arc<AtomicCell<bool>>,
    alive: Arc<AtomicCell<bool>>,
}

impl Stream {
    fn new(cb: Callback) -> Self {
        Stream {
            write_callback: Arc::new(Mutex::new(cb)),
            streaming: Arc::new(AtomicCell::new(false)),
            alive: Arc::new(AtomicCell::new(true)),
        }
    }

    fn play(&mut self) {
        let streaming = self.streaming.clone();
        streaming.store(true);
        let alive = self.alive.clone();
        let cb = self.write_callback.clone();
        thread::spawn(move || loop {
            while alive.load() {
                if streaming.load() {
                    if let Ok(mut cb) = cb.lock() {
                        cb();
                    }
                }
            }
        });
    }

    fn pause(&mut self) {
        self.streaming.store(false);
    }

    fn unpause(&mut self) {
        if !self.alive.load() {
            self.alive.store(true);
            self.play();
        }
        self.streaming.store(true);
    }

    fn kill(&mut self) {
        self.alive.store(false);
    }

    pub fn disconnect(&mut self) {}
}

// unsafe impl Send for Stream {}

#[derive(Default)]
pub struct AudioOutput {
    playing: Option<Stream>,
    next_up: Option<Stream>,
    autostart: bool,
}

impl AudioOutput {
    pub fn try_new() -> anyhow::Result<Self> {
        Ok(AudioOutput::default())
    }

    pub fn make_stream(
        &mut self,
        server_ip: Ipv4Addr,
        default_ip: &Ipv4Addr,
        server_port: u16,
        http_headers: String,
        status: Arc<Mutex<StatusData>>,
        stream_in: Sender<PlayerMsg>,
        threshold: u32,
        format: slimproto::proto::Format,
        pcmsamplerate: slimproto::proto::PcmSampleRate,
        pcmchannels: slimproto::proto::PcmChannels,
        volume: Arc<Mutex<Vec<f32>>>,
        output_threshold: Duration,
    ) {
        let default_ip = default_ip.clone();

        thread::spawn(move || {
            let mut decoder = match decode::make_decoder(
                server_ip,
                default_ip,
                server_port,
                http_headers,
                stream_in.clone(),
                status.clone(),
                threshold,
                format,
                pcmsamplerate,
                pcmchannels,
            ) {
                Some(decoder) => decoder,
                None => {
                    stream_in.send(PlayerMsg::NotSupported).ok();
                    return;
                }
            };
            // Create an audio buffer to hold raw u8 samples
            let threshold_samples = (output_threshold.as_millis()
                * decoder.channels() as u128
                * decoder.sample_rate() as u128
                / 1000)
                * 4;
            let mut audio_buf = Vec::with_capacity(threshold_samples as usize);

            // Prefill audio buffer to threshold
            match decoder.fill_buf(&mut audio_buf, 1024 as usize, volume.clone()) {
                Ok(()) => {}
                Err(DecoderError::EndOfDecode) => {
                    stream_in.send(PlayerMsg::EndOfDecode).ok();
                }
                Err(DecoderError::StreamError(e)) => {
                    warn!("Error reading data stream: {}", e);
                    stream_in.send(PlayerMsg::NotSupported).ok();
                    return;
                }
            }

            let stream_in_r = stream_in.clone();
            let mut draining = false;
            let mut drained = false;
            let mut display_count = 0;
            let start_time = Instant::now();
            let stream = Stream::new(Box::new(move || {
                if !drained {
                    match decoder.fill_buf(&mut audio_buf, 1024 as usize, volume.clone()) {
                        Ok(()) => {}
                        Err(DecoderError::EndOfDecode) => {
                            if !draining {
                                stream_in_r.send(PlayerMsg::EndOfDecode).ok();
                                draining = true;
                            }
                        }
                        Err(DecoderError::StreamError(e)) => {
                            warn!("Error reading data stream: {}", e);
                            stream_in_r.send(PlayerMsg::NotSupported).ok();
                            return;
                        }
                    }

                    if audio_buf.len() > 0 {
                        display_count = (display_count + 1) % 100;
                        if display_count == 0 {
                            info!("Play audio data ... length {}", audio_buf.len());
                        }

                        // Fill the real audio buffer here
                        // The dummy just throws it away
                        thread::sleep(Duration::from_micros(22) * (audio_buf.len() / 8) as u32);
                        audio_buf.drain(..);

                        let stream_time = Instant::now() - start_time;
                        if let Ok(mut status) = status.lock() {
                            status.set_elapsed_milli_seconds(stream_time.as_millis() as u32);
                            status.set_elapsed_seconds(stream_time.as_secs() as u32);
                            status.set_output_buffer_size(audio_buf.capacity() as u32);
                            status.set_output_buffer_fullness(audio_buf.len() as u32);
                        };
                    } else if draining {
                        stream_in_r.send(PlayerMsg::Drained).ok();
                        drained = true;
                    }
                }
            }));

            stream_in.send(PlayerMsg::StreamEstablished(stream)).ok();
        });
    }

    pub fn enqueue(&mut self, stream: Stream, stream_in: Sender<PlayerMsg>) {
        if self.playing.is_some() {
            self.next_up = Some(stream);
        } else {
            self.playing = Some(stream);
            if self.autostart {
                stream_in.send(PlayerMsg::TrackStarted).ok();
                self.play();
            }
        }
    }

    pub fn play(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            stream.play();
            true
        } else {
            false
        }
    }

    pub fn unpause(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            stream.unpause();
            true
        } else {
            false
        }
    }

    pub fn pause(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            stream.pause();
            true
        } else {
            false
        }
    }

    pub fn stop(&mut self) {
        if let Some(ref mut stream) = self.playing {
            stream.kill();
        }
        self.next_up = None;
        self.playing = None;
    }

    pub fn flush(&mut self) {
        self.stop();
    }

    pub fn shift(&mut self) -> Option<Stream> {
        let old_stream = self.playing.take();
        self.playing = self.next_up.take();
        old_stream
    }

    pub fn set_autostart(&mut self, val: bool) {
        self.autostart = val;
    }
}

pub fn get_output_device_names() -> anyhow::Result<Vec<String>> {
    Ok(vec!["dummy_out1".to_string(), "dummy_out2".to_string()])
}
