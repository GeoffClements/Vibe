use std::{
    cell::RefCell,
    collections::VecDeque,
    io::Write,
    net::{Ipv4Addr, TcpStream},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use crossbeam::{atomic::AtomicCell, channel::Sender};
use libpulse_binding as pa;
use log::{error, info, warn};
use pa::{context::Context, operation::Operation, sample::Spec, stream::Stream};
use slimproto::{
    buffer::SlimBuffer,
    proto::{PcmChannels, PcmSampleRate},
    status::{StatusCode, StatusData},
    ClientMessage,
};
use symphonia::core::{
    audio::{AsAudioBufferRef, RawSampleBuffer, Signal},
    codecs::{Decoder, DecoderOptions},
    formats::FormatOptions,
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::{Hint, ProbeResult},
};

use crate::PlayerMsg;

#[derive(std::fmt::Debug)]
enum DecoderError {
    EndOfDecode,
    StreamError(symphonia::core::errors::Error),
}

impl std::fmt::Display for DecoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecoderError::EndOfDecode => write!(f, "End of decode stream"),
            DecoderError::StreamError(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for DecoderError {}

pub struct StreamQueue {
    queue: VecDeque<Rc<RefCell<Stream>>>,
    draining: bool,
    buffering: bool,
}

impl StreamQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(2),
            draining: false,
            buffering: true,
        }
    }

    pub fn add(&mut self, sm: Rc<RefCell<Stream>>) -> bool {
        self.queue.push_back(sm);
        self.queue.len() == 1
    }

    pub fn shift(&mut self) -> Option<Rc<RefCell<Stream>>> {
        self.queue.pop_front()
    }

    // pub fn current_stream(&self) -> Option<Rc<RefCell<Stream>>> {
    //     if self.queue.len() > 0 {
    //         return Some(self.queue[0].clone());
    //     }
    //     None
    // }

    pub fn cork(&mut self) -> bool {
        if self.queue.len() > 0 {
            self.queue[0].borrow_mut().cork(None);
            return true;
        }
        false
    }

    pub fn uncork(&mut self) -> bool {
        if self.queue.len() > 0 {
            let op = self.queue[0].borrow_mut().uncork(None);
            self.do_op(op);
            self.draining = false;
            return true;
        }
        false
    }

    pub fn drain(&mut self, stream_in: Sender<PlayerMsg>) -> bool {
        if self.queue.len() > 0 && !self.draining {
            self.draining = true;
            let op = self.queue[0]
                .borrow_mut()
                .drain(Some(Box::new(move |success| {
                    if success {
                        stream_in.send(PlayerMsg::Drained).ok();
                    }
                })));
            self.do_op(op);
            return true;
        }
        false
    }

    pub fn flush(&self) {
        if self.queue.len() > 0 {
            let op = self.queue[0].borrow_mut().flush(None);
            self.do_op(op);
        }
    }

    pub fn stop(&mut self) {
        self.flush();
        while self.queue.len() > 0 {
            if let Some(sm) = self.shift() {
                sm.borrow_mut().disconnect().ok();
            }
        }
    }

    pub fn is_draining(&self) -> bool {
        self.draining
    }

    pub fn is_buffering(&self) -> bool {
        self.buffering
    }

    pub fn set_buffering(&mut self, buffering: bool) {
        self.buffering = buffering;
    }

    fn do_op(&self, op: Operation<dyn FnMut(bool)>) {
        std::thread::spawn(move || {
            while op.get_state() == pa::operation::State::Running {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }
}

pub fn make_stream(
    server_ip: Ipv4Addr,
    default_ip: &Ipv4Addr,
    server_port: u16,
    http_headers: String,
    status: Arc<Mutex<StatusData>>,
    slim_tx: Sender<ClientMessage>,
    stream_in: Sender<PlayerMsg>,
    threshold: u32,
    format: slimproto::proto::Format,
    pcmsamplerate: slimproto::proto::PcmSampleRate,
    pcmchannels: slimproto::proto::PcmChannels,
    cx: Rc<RefCell<Context>>,
    skip: Arc<AtomicCell<Duration>>,
    volume: Arc<Mutex<Vec<f32>>>,
    output_threshold: Duration,
) -> anyhow::Result<Option<Rc<RefCell<Stream>>>> {
    // The LMS sends an ip of 0, 0, 0, 0 when it wants us to default to it
    let ip = if server_ip.is_unspecified() {
        *default_ip
    } else {
        server_ip
    };

    let mut data_stream = TcpStream::connect((ip, server_port))?;
    data_stream.write(http_headers.as_bytes())?;
    data_stream.flush()?;

    if let Ok(mut status) = status.lock() {
        info!("Sending stream connected");
        let msg = status.make_status_message(StatusCode::Connect);
        slim_tx.send(msg).ok();
    }

    let status_ref = status.clone();
    let slim_tx_ref = slim_tx.clone();
    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(SlimBuffer::with_capacity(
            threshold as usize * 1024,
            data_stream,
            status.clone(),
            threshold,
            Some(Box::new(move || {
                info!("Sending buffer threshold reached");
                if let Ok(mut status) = status_ref.lock() {
                    let msg = status.make_status_message(StatusCode::BufferThreshold);
                    slim_tx_ref.send(msg).ok();
                }
            })),
        ))),
        Default::default(),
    );

    // Create a hint to help the format registry guess what format reader is appropriate.
    let mut hint = Hint::new();
    hint.mime_type({
        match format {
            slimproto::proto::Format::Pcm => "audio/x-adpcm",
            slimproto::proto::Format::Mp3 => "audio/mpeg3",
            slimproto::proto::Format::Aac => "audio/aac",
            slimproto::proto::Format::Ogg => "audio/ogg",
            slimproto::proto::Format::Flac => "audio/flac",
            _ => "",
        }
    });

    let mut probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(probed) => probed,
        Err(_) => {
            warn!("Unsupported format");
            if let Ok(mut status) = status.lock() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx.send(msg).ok();
            }
            return Ok(None);
        }
    };

    let track = match probed.format.default_track() {
        Some(track) => track,
        None => {
            warn!("Cannot find default track");
            if let Ok(mut status) = status.lock() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx.send(msg).ok();
            }
            return Ok(None);
        }
    };

    if let Ok(mut status) = status.lock() {
        info!("Sending stream established");
        let msg = status.make_status_message(StatusCode::StreamEstablished);
        slim_tx.send(msg).ok();
    }

    // Work with a sample format of F32
    let sample_format = pa::sample::Format::FLOAT32NE;

    let sample_rate = match pcmsamplerate {
        PcmSampleRate::Rate(rate) => rate,
        PcmSampleRate::SelfDescribing => track.codec_params.sample_rate.unwrap_or(44100),
    };

    let channels = match pcmchannels {
        PcmChannels::Mono => 1u8,
        PcmChannels::Stereo => 2,
        PcmChannels::SelfDescribing => match track.codec_params.channel_layout {
            Some(symphonia::core::audio::Layout::Mono) => 1,
            Some(symphonia::core::audio::Layout::Stereo) => 2,
            None => match track.codec_params.channels {
                Some(channels) => channels.count() as u8,
                _ => 2,
            },
            _ => 2,
        },
    };

    // Create a spec for the pa stream
    let spec = Spec {
        format: sample_format,
        rate: sample_rate,
        channels,
    };

    // Create a pulseaudio stream
    let pa_stream = Rc::new(RefCell::new(
        match Stream::new(&mut (*cx).borrow_mut(), "Music", &spec, None) {
            Some(stream) => stream,
            None => {
                if let Ok(mut status) = status.lock() {
                    let msg = status.make_status_message(StatusCode::NotSupported);
                    slim_tx.send(msg).ok();
                }
                return Ok(None);
            }
        },
    ));

    // Create a decoder for the track.
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    // Create an audio buffer to hold raw u8 samples
    let threshold_samples =
        (output_threshold.as_millis() * channels as u128 * sample_rate as u128 / 1000) * 4;
    let mut audio_buf = Vec::with_capacity(threshold_samples as usize);

    // Prefill audio buffer to threshold
    match fill_buf(
        &mut audio_buf,
        threshold_samples as usize,
        &mut probed,
        &mut decoder,
        volume.clone(),
    ) {
        Ok(()) => {}
        Err(DecoderError::EndOfDecode) => {
            stream_in.send(PlayerMsg::EndOfDecode).ok();
        }
        Err(DecoderError::StreamError(e)) => {
            error!("Error reading data stream: {}", e);
            if let Ok(mut status) = status.lock() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx.send(msg).ok();
            }
            return Err(e.into());
        }
    }

    // Add callback to pa_stream to feed music
    let status_ref = status.clone();
    let sm_ref = Rc::downgrade(&pa_stream);
    (*pa_stream)
        .borrow_mut()
        .set_write_callback(Some(Box::new(move |len| {
            match fill_buf(
                &mut audio_buf,
                len,
                &mut probed,
                &mut decoder,
                volume.clone(),
            ) {
                Ok(()) => {}
                Err(DecoderError::EndOfDecode) => {
                    stream_in.send(PlayerMsg::EndOfDecode).ok();
                }
                Err(DecoderError::StreamError(e)) => {
                    error!("Error reading data stream: {}", e);
                    if let Ok(mut status) = status.lock() {
                        let msg = status.make_status_message(StatusCode::NotSupported);
                        slim_tx.send(msg).ok();
                    }
                    return;
                }
            }

            let buf_len = if audio_buf.len() < len {
                audio_buf.len()
            } else {
                len
            };

            let offset = match skip.take() {
                dur if dur.is_zero() => 0i64,
                dur => {
                    let samples = dur.as_millis() as f64 * spec.rate as f64 / 1000.0;
                    samples.round() as i64 * spec.channels as i64 * 4
                }
            };

            if let Some(sm) = sm_ref.upgrade() {
                unsafe {
                    (*sm.as_ptr())
                        .write_copy(
                            &audio_buf.drain(..buf_len).collect::<Vec<u8>>(),
                            offset,
                            pa::stream::SeekMode::Relative,
                        )
                        .ok();
                    (*sm.as_ptr()).update_timing_info(None);

                    if let Ok(Some(stream_time)) = (*sm.as_ptr()).get_time() {
                        if let Ok(mut status) = status_ref.lock() {
                            status.set_elapsed_milli_seconds(stream_time.as_millis() as u32);
                            status.set_elapsed_seconds(stream_time.as_secs() as u32);
                            status.set_output_buffer_size(audio_buf.capacity() as u32);
                            status.set_output_buffer_fullness(audio_buf.len() as u32);
                        };
                    }
                };
            }
        })));

    Ok(Some(pa_stream))
}

fn fill_buf(
    buffer: &mut Vec<u8>,
    limit: usize,
    probed: &mut ProbeResult,
    decoder: &mut Box<dyn Decoder>,
    volume: Arc<Mutex<Vec<f32>>>,
) -> Result<(), DecoderError> {
    while buffer.len() < limit {
        let packet = match probed.format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof
                    && err.to_string() == "end of stream" =>
            {
                return Err(DecoderError::EndOfDecode);
            }
            Err(e) => {
                return Err(DecoderError::StreamError(e));
            }
        };

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };

        if decoded.frames() == 0 {
            continue;
        }

        let mut sample_buf = decoded.make_equivalent::<f32>();
        decoded.convert(&mut sample_buf);

        if let Ok(vol) = volume.lock() {
            for chan in 0..sample_buf.spec().channels.count() {
                let chan_samples = sample_buf.chan_mut(chan);
                chan_samples.iter_mut().for_each(|s| *s *= vol[chan % 2]);
            }
        }

        let mut raw_buf = RawSampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());

        raw_buf.copy_interleaved_ref(sample_buf.as_audio_buffer_ref());
        buffer.extend_from_slice(raw_buf.as_bytes());
    }
    Ok(())
}
