use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crossbeam::{atomic::AtomicCell, channel::Sender};
use log::warn;
use slimproto::{status::StatusData, ClientMessage};

use crate::{
    decode::{self, DecoderError},
    PlayerMsg,
};

type Callback = Box<dyn FnMut() + Send + Sync + 'static>;

pub struct Stream {
    write_callback: Arc<Mutex<Callback>>,
    streaming: Arc<AtomicCell<bool>>,
}

impl Stream {
    fn new(cb: Callback) -> Self {
        Stream {
            write_callback: Arc::new(Mutex::new(cb)),
            streaming: Arc::new(AtomicCell::new(false)),
        }
    }

    fn play(&mut self) {
        let running = self.streaming.clone();
        let cb = self.write_callback.clone();
        thread::spawn(move || loop {
            if running.load() {
                if let Ok(mut cb) = cb.lock() {
                    cb();
                }
            }
            thread::sleep(Duration::from_millis(10));
        });
    }

    fn pause(&mut self) {
        self.streaming.store(false);
    }
}

// unsafe impl Send for Stream {}

#[derive(Default)]
pub struct AudioOutput {
    playing: Option<Stream>,
    next_up: Option<Stream>,
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
        _slim_tx: Sender<ClientMessage>,
        stream_in: Sender<PlayerMsg>,
        threshold: u32,
        format: slimproto::proto::Format,
        pcmsamplerate: slimproto::proto::PcmSampleRate,
        pcmchannels: slimproto::proto::PcmChannels,
        _skip: Arc<AtomicCell<Duration>>,
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
            match decoder.fill_buf(&mut audio_buf, threshold_samples as usize, volume.clone()) {
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
            let stream = Stream::new(Box::new(move || {
                match decoder.fill_buf(&mut audio_buf, threshold_samples as usize, volume.clone()) {
                    Ok(()) => {}
                    Err(DecoderError::EndOfDecode) => {
                        stream_in_r.send(PlayerMsg::EndOfDecode).ok();
                        draining = true;
                    }
                    Err(DecoderError::StreamError(e)) => {
                        warn!("Error reading data stream: {}", e);
                        stream_in_r.send(PlayerMsg::NotSupported).ok();
                        return;
                    }
                }

                if audio_buf.len() > 0 {
                    // Fill the real audio buffer here
                    // The dummy just throws it away
                    audio_buf.drain(..);
                } else if draining {
                    stream_in_r.send(PlayerMsg::Drained).ok();
                    draining = false;
                }
            }));

            stream_in.send(PlayerMsg::StreamEstablished(stream)).ok();
        });
    }

    pub fn enqueue(&mut self, stream: Stream) {
        if self.playing.is_some() {
            self.next_up = Some(stream);
        } else {
            self.playing = Some(stream);
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
        self.play()
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
        self.next_up = None;
        self.playing = None;
    }
}

pub fn get_output_device_names() -> anyhow::Result<Vec<String>> {
    Ok(vec!["dummy_out1".to_string(), "dummy_out2".to_string()])
}
