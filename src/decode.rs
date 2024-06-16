use std::{
    io::Write,
    net::{Ipv4Addr, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

use crossbeam::{atomic::AtomicCell, channel::Sender};
use log::warn;
use slimproto::{
    buffer::SlimBuffer,
    proto::{PcmChannels, PcmSampleRate},
    status::StatusData,
};
use symphonia::core::{
    audio::{AsAudioBufferRef, AudioBufferRef, RawSampleBuffer, Signal},
    codecs::{Decoder as SymDecoder, DecoderOptions},
    formats::FormatOptions,
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::{Hint, ProbeResult},
    sample::SampleFormat,
};

use crate::{PlayerMsg, StreamParams};

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
    I32,
    U32,
    I16,
    U16,
}

impl AudioFormat {
    pub fn size_of(&self) -> usize {
        match self {
            Self::F32 | Self::I32 | Self::U32 => 4,
            Self::I16 | Self::U16 => 2,
        }
    }
}

impl From<SampleFormat> for AudioFormat {
    fn from(value: SampleFormat) -> Self {
        match value {
            SampleFormat::U16 => AudioFormat::U16,
            SampleFormat::S16 => AudioFormat::I16,
            SampleFormat::U32 => AudioFormat::U32,
            SampleFormat::S32 => AudioFormat::I32,
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

        let sample_format = match track.codec_params.sample_format {
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
                format: sample_format,
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

            let vol = match volume.lock() {
                Ok(volume) => volume[0],
                Err(_) => 1.0,
            };

            let capacity = decoded.capacity();
            let spec = *decoded.spec();
            match decoded {
                AudioBufferRef::U16(mut audio_buffer) => {
                    audio_buffer
                        .to_mut()
                        .transform(|s| (s as f32 * vol).round() as u16);
                    let mut samples = audio_buffer.make_equivalent();
                    audio_buffer.convert::<i16>(&mut samples);

                    let mut sample_buffer = RawSampleBuffer::<i16>::new(capacity as u64, spec);
                    sample_buffer.copy_interleaved_ref(samples.as_audio_buffer_ref());
                    buffer.extend_from_slice(sample_buffer.as_bytes());
                }
                AudioBufferRef::S16(mut audio_buffer) => {
                    audio_buffer
                        .to_mut()
                        .transform(|s| (s as f32 * vol).round() as i16);

                    let mut sample_buffer = RawSampleBuffer::<i16>::new(capacity as u64, spec);
                    sample_buffer.copy_interleaved_ref(audio_buffer.as_audio_buffer_ref());
                    buffer.extend_from_slice(sample_buffer.as_bytes());
                }
                AudioBufferRef::U32(mut audio_buffer) => {
                    audio_buffer
                        .to_mut()
                        .transform(|s| (s as f32 * vol).round() as u32);
                    let mut samples = audio_buffer.make_equivalent();
                    audio_buffer.convert::<i32>(&mut samples);

                    let mut sample_buffer = RawSampleBuffer::<i32>::new(capacity as u64, spec);
                    sample_buffer.copy_interleaved_ref(samples.as_audio_buffer_ref());
                    buffer.extend_from_slice(sample_buffer.as_bytes());
                }
                AudioBufferRef::S32(mut audio_buffer) => {
                    audio_buffer
                        .to_mut()
                        .transform(|s| (s as f32 * vol).round() as i32);

                    let mut sample_buffer = RawSampleBuffer::<i32>::new(capacity as u64, spec);
                    sample_buffer.copy_interleaved_ref(audio_buffer.as_audio_buffer_ref());
                    buffer.extend_from_slice(sample_buffer.as_bytes());
                }
                AudioBufferRef::F32(mut audio_buffer) => {
                    audio_buffer.to_mut().transform(|s| s * vol);

                    let mut sample_buffer = RawSampleBuffer::<f32>::new(capacity as u64, spec);
                    sample_buffer.copy_interleaved_ref(audio_buffer.as_audio_buffer_ref());
                    buffer.extend_from_slice(sample_buffer.as_bytes());
                }
                // AudioBufferRef::F64(_) => println!("F64"),
                // AudioBufferRef::S24(_) => println!("S24"),
                // AudioBufferRef::S8(_) => println!("S8"),
                // AudioBufferRef::U24(_) => println!("U24"),
                // AudioBufferRef::U8(_) => println!("U8"),
                _ => {
                    return Err(DecoderError::StreamError(
                        symphonia::core::errors::Error::Unsupported("Unsupported format"),
                    ));
                }
            }
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
    autostart: slimproto::proto::AutoStart,
    volume: Arc<Mutex<Vec<f32>>>,
    skip: Arc<AtomicCell<Duration>>,
    output_threshold: Duration,
) -> Option<(Decoder, StreamParams)> {
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

    match Decoder::try_new(mss, format, pcmsamplerate, pcmchannels) {
        Some(decoder) => Some((
            decoder,
            StreamParams {
                autostart,
                volume,
                skip,
                output_threshold,
            },
        )),
        None => None,
    }
}

fn make_connection(ip: Ipv4Addr, port: u16, http_headers: String) -> anyhow::Result<TcpStream> {
    let mut data_stream = TcpStream::connect((ip, port))?;
    data_stream.write(http_headers.as_bytes())?;
    data_stream.flush()?;
    Ok(data_stream)
}
