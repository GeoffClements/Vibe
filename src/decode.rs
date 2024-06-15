use std::{
    io::Write,
    net::{Ipv4Addr, TcpStream},
    sync::{Arc, Mutex},
};

use crossbeam::channel::Sender;
use log::warn;
use slimproto::{
    buffer::SlimBuffer,
    proto::{PcmChannels, PcmSampleRate},
    status::StatusData,
};
use symphonia::core::{
    audio::{AsAudioBufferRef, RawSampleBuffer, Signal},
    codecs::{Decoder as SymDecoder, DecoderOptions},
    formats::FormatOptions,
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::{Hint, ProbeResult},
    sample::SampleFormat,
};

use crate::PlayerMsg;

#[derive(Debug)]
pub enum DecoderError {
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

#[derive(Clone, Copy)]
pub enum AudioFormat {
    F32,
    I16,
    U16,
}

impl From<SampleFormat> for AudioFormat {
    fn from(value: SampleFormat) -> Self {
        match value {
            SampleFormat::U16 => AudioFormat::U16,
            SampleFormat::S16 => AudioFormat::I16,
            _ => AudioFormat::F32,
        }
    }
}

struct AudioSpec {
    channels: u8,
    sample_rate: u32,
    format: AudioFormat,
}

pub struct Decoder {
    pub probed: ProbeResult,
    pub decoder: Box<dyn SymDecoder>,
    spec: AudioSpec,
}

impl Decoder {
    pub fn try_new(
        mss: MediaSourceStream,
        format: slimproto::proto::Format,
        pcmsamplerate: slimproto::proto::PcmSampleRate,
        pcmchannels: slimproto::proto::PcmChannels,
    ) -> Option<Self> {
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

        let probed = match symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        ) {
            Ok(probed) => probed,
            Err(_) => {
                return None;
            }
        };

        let track = match probed.format.default_track() {
            Some(track) => track,
            None => {
                return None;
            }
        };

        let _sample_format = match track.codec_params.sample_format {
            Some(sample_format) => sample_format.into(),
            None => AudioFormat::F32,
        };

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

        // Create a decoder for the track.
        let decoder = match symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
        {
            Ok(decoder) => decoder,
            Err(_) => {
                return None;
            }
        };

        Some(Decoder {
            probed,
            decoder,
            spec: AudioSpec {
                channels,
                sample_rate,
                // format: sample_format,
                format: AudioFormat::F32,
            },
        })
    }

    pub fn channels(&self) -> u8 {
        self.spec.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.spec.sample_rate
    }

    pub fn format(&self) -> AudioFormat {
        self.spec.format
    }

    pub fn fill_buf(
        &mut self,
        buffer: &mut Vec<u8>,
        limit: usize,
        volume: Arc<Mutex<Vec<f32>>>,
    ) -> Result<(), DecoderError> {
        while buffer.len() < limit {
            let packet = match self.probed.format.next_packet() {
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

            let decoded = match self.decoder.decode(&packet) {
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

            let mut raw_buf =
                RawSampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());

            raw_buf.copy_interleaved_ref(sample_buf.as_audio_buffer_ref());
            buffer.extend_from_slice(raw_buf.as_bytes());
        }
        Ok(())
    }
}

pub fn make_decoder(
    server_ip: Ipv4Addr,
    default_ip: Ipv4Addr,
    server_port: u16,
    http_headers: String,
    stream_in: Sender<PlayerMsg>,
    status: Arc<Mutex<StatusData>>,
    threshold: u32,
    format: slimproto::proto::Format,
    pcmsamplerate: slimproto::proto::PcmSampleRate,
    pcmchannels: slimproto::proto::PcmChannels,
) -> Option<Decoder> {
    let ip = if server_ip.is_unspecified() {
        default_ip
    } else {
        server_ip
    };

    let data_stream = match make_connection(ip, server_port, http_headers) {
        Ok(data_s) => data_s,
        Err(_) => {
            warn!("Unable to connect to data stream at {}", ip);
            return None;
        }
    };

    stream_in.send(PlayerMsg::Connected).ok();

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(SlimBuffer::with_capacity(
            threshold as usize * 1024,
            data_stream,
            status,
            threshold,
            None,
        ))),
        Default::default(),
    );
    stream_in.send(PlayerMsg::BufferThreshold).ok();

    Decoder::try_new(mss, format, pcmsamplerate, pcmchannels)
}

fn make_connection(ip: Ipv4Addr, port: u16, http_headers: String) -> anyhow::Result<TcpStream> {
    let mut data_stream = TcpStream::connect((ip, port))?;
    data_stream.write(http_headers.as_bytes())?;
    data_stream.flush()?;
    Ok(data_stream)
}
