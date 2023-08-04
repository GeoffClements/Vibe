use std::{
    cell::RefCell,
    collections::VecDeque,
    io::Write,
    net::{Ipv4Addr, TcpStream},
    rc::Rc,
    sync::{Arc, RwLock},
    time::Duration,
};

use crossbeam::channel::Sender;
use libpulse_binding as pa;
use log::{error, info};
use pa::{
    context::Context, operation::Operation, sample::Spec, stream::Stream, volume::ChannelVolumes,
};
use slimproto::{
    buffer::SlimBuffer,
    proto::{PcmChannels, PcmSampleRate},
    status::{StatusCode, StatusData},
    ClientMessage,
};
use symphonia::core::{
    audio::RawSampleBuffer,
    codecs::DecoderOptions,
    formats::FormatOptions,
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::Hint,
};

use crate::PlayerMsg;

pub struct StreamQueue {
    queue: VecDeque<Rc<RefCell<Stream>>>,
    draining: bool,
}

impl StreamQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(2),
            draining: false,
        }
    }

    pub fn add(&mut self, sm: Rc<RefCell<Stream>>) -> bool {
        self.queue.push_back(sm);
        self.queue.len() == 1
    }

    pub fn shift(&mut self) -> Option<Rc<RefCell<Stream>>> {
        self.queue.pop_front()
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

    pub fn set_volume(&self, volume: &ChannelVolumes, cx: Rc<RefCell<Context>>) {
        if self.queue.len() > 0 {
            if let Some(device) = self.queue[0].borrow().get_device_index() {
                let _op = cx
                    .borrow()
                    .introspect()
                    .set_sink_volume_by_index(device, volume, None);
                // while op.get_state() == pa::operation::State::Running {
                //     std::thread::sleep(Duration::from_millis(1));
                // }
            }
        }
    }

    pub fn is_draining(&self) -> bool {
        self.draining
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
    status: Arc<RwLock<StatusData>>,
    slim_tx: Sender<ClientMessage>,
    stream_in: Sender<PlayerMsg>,
    threshold: u32,
    format: slimproto::proto::Format,
    pcmsamplerate: slimproto::proto::PcmSampleRate,
    pcmchannels: slimproto::proto::PcmChannels,
    cx: Rc<RefCell<Context>>,
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

    if let Ok(status) = status.read() {
        info!("Sending stream connected");
        let msg = status.make_status_message(StatusCode::Connect);
        slim_tx.send(msg).ok();
    }

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(SlimBuffer::with_capacity(
            threshold as usize * 1024,
            data_stream,
            status.clone(),
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
            if let Ok(status) = status.read() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx.send(msg).ok();
            }
            return Ok(None);
        }
    };

    let track = match probed.format.default_track() {
        Some(track) => track,
        None => {
            if let Ok(status) = status.read() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx.send(msg).ok();
            }
            return Ok(None);
        }
    };

    if let Ok(status) = status.read() {
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
                if let Ok(status) = status.read() {
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

    let mut audio_buf = Vec::with_capacity(8 * 1024);

    // Add callback to pa_stream to feed music
    let status_ref = status.clone();
    let sm_ref = pa_stream.clone();
    (*pa_stream)
        .borrow_mut()
        .set_write_callback(Some(Box::new(move |len| {
            while audio_buf.len() < len {
                let packet = match probed.format.next_packet() {
                    Ok(packet) => packet,
                    Err(symphonia::core::errors::Error::IoError(err))
                        if err.kind() == std::io::ErrorKind::UnexpectedEof
                            && err.to_string() == "end of stream" =>
                    {
                        stream_in.send(PlayerMsg::EndOfDecode).ok();
                        break;
                    }
                    Err(e) => {
                        error!("Error reading data stream: {}", e);
                        if let Ok(status) = status.read() {
                            let msg = status.make_status_message(StatusCode::NotSupported);
                            slim_tx.send(msg).ok();
                        }
                        return;
                    }
                };

                let decoded = match decoder.decode(&packet) {
                    Ok(decoded) => decoded,
                    Err(_) => break,
                };

                if decoded.frames() == 0 {
                    break;
                }

                let mut raw_buf =
                    RawSampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());

                raw_buf.copy_interleaved_ref(decoded);
                audio_buf.extend_from_slice(raw_buf.as_bytes());
            }

            let buf_len = if audio_buf.len() < len {
                audio_buf.len()
            } else {
                len
            };

            unsafe {
                (*sm_ref.as_ptr())
                    .write_copy(
                        &audio_buf.drain(..buf_len).collect::<Vec<u8>>(),
                        0,
                        pa::stream::SeekMode::Relative,
                    )
                    .ok()
            };

            if let Ok(Some(stream_time)) = unsafe { (*sm_ref.as_ptr()).get_time() } {
                if let Ok(mut status) = status_ref.write() {
                    status.set_elapsed_milli_seconds(stream_time.as_millis() as u32);
                    status.set_elapsed_seconds(stream_time.as_secs() as u32);
                    status.set_output_buffer_size(audio_buf.capacity() as u32);
                    status.set_output_buffer_fullness(audio_buf.len() as u32);
                };
            }
        })));

    Ok(Some(pa_stream))
}
