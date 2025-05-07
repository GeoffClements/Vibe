use std::{
    io::{BufRead, Write},
    mem,
    net::{Ipv4Addr, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Context};
#[cfg(not(feature = "pulse"))]
use crossbeam::channel::Sender;
#[cfg(feature = "pulse")]
use crossbeam::{atomic::AtomicCell, channel::Sender};

use log::warn;
use slimproto::{
    buffer::SlimBuffer,
    proto::{PcmChannels, PcmSampleRate},
    status::StatusData,
};

use symphonia::core::{
    audio::{AudioBuffer, Signal},
    codecs::{Decoder as SymDecoder, DecoderOptions},
    conv::FromSample,
    formats::FormatOptions,
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::{Hint, ProbeResult},
    sample::SampleFormat,
};

#[cfg(feature = "pulse")]
use symphonia::core::audio::{RawSample, RawSampleBuffer};

#[cfg(feature = "rodio")]
use symphonia::core::{audio::SampleBuffer, sample::Sample};

#[cfg(feature = "notify")]
use symphonia::core::meta::MetadataRevision;

use crate::{message::PlayerMsg, StreamParams};

#[derive(Debug)]
pub enum DecoderError {
    EndOfDecode,
    // Unhandled,
    Retry,
    StreamError(symphonia::core::errors::Error),
}

impl std::fmt::Display for DecoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecoderError::EndOfDecode => write!(f, "End of decode stream"),
            // DecoderError::Unhandled => write!(f, "Unhandled format"),
            DecoderError::Retry => write!(f, "Decoder reset required"),
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
            Self::F32 => mem::size_of::<f32>(),
            Self::I32 => mem::size_of::<i32>(),
            Self::U32 => mem::size_of::<u32>(),
            Self::I16 => mem::size_of::<i16>(),
            Self::U16 => mem::size_of::<u16>(),
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
    ) -> anyhow::Result<Self> {
        // Create a hint to help the format registry guess what format reader is appropriate.
        let mut hint = Hint::new();
        hint.mime_type({
            match format {
                slimproto::proto::Format::Pcm => "audio/x-adpcm",
                slimproto::proto::Format::Mp3 => "audio/mpeg",
                slimproto::proto::Format::Aac => "audio/aac",
                slimproto::proto::Format::Ogg => "audio/ogg",
                slimproto::proto::Format::Flac => "audio/flac",
                _ => "",
            }
        });

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .context("Unrecognized container format")?;

        let track = match probed.format.default_track() {
            Some(track) => track,
            None => {
                bail!("Unable to find default track");
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
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .context("Unable to find suitable decoder")?;

        Ok(Decoder {
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

    #[cfg(feature = "pulse")]
    pub fn format(&self) -> AudioFormat {
        self.spec.format
    }

    fn get_audio_buffer(
        &mut self,
        volume: Arc<Mutex<Vec<f32>>>,
    ) -> Result<AudioBuffer<f32>, DecoderError> {
        let decoded = loop {
            let packet = self.probed.format.next_packet().map_err(|err| match err {
                symphonia::core::errors::Error::IoError(err)
                    if err.kind() == std::io::ErrorKind::UnexpectedEof
                        && err.to_string() == "end of stream" =>
                {
                    DecoderError::EndOfDecode
                }
                symphonia::core::errors::Error::ResetRequired => {
                    self.decoder.reset();
                    DecoderError::Retry
                }
                error => DecoderError::StreamError(error),
            })?;

            match self.decoder.decode(&packet) {
                Ok(decoded) => break decoded,
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(e) => return Err(DecoderError::StreamError(e)),
            }
        };

        let vol = volume.lock().map(|v| v[0]).unwrap_or_default();

        let mut audio_buffer = decoded.make_equivalent();
        decoded.convert::<f32>(&mut audio_buffer);
        audio_buffer.transform(|s| s * vol);
        Ok(audio_buffer)
    }

    #[cfg(feature = "rodio")]
    pub fn fill_sample_buffer<T>(
        &mut self,
        buffer: &mut Vec<T>,
        limit: Option<usize>,
        volume: Arc<Mutex<Vec<f32>>>,
    ) -> Result<(), DecoderError>
    where
        T: Sample + FromSample<f32>,
    {
        let limit = limit.unwrap_or_else(|| {
            if buffer.capacity() > 0 {
                buffer.capacity()
            } else {
                1024
            }
        });

        while buffer.len() < limit {
            let audio_buffer = self.get_audio_buffer(volume.clone())?;
            let mut sample_buffer =
                SampleBuffer::<T>::new(audio_buffer.capacity() as u64, *audio_buffer.spec());
            sample_buffer.copy_interleaved_typed::<f32>(&audio_buffer);
            buffer.extend_from_slice(sample_buffer.samples());
        }

        Ok(())
    }

    #[cfg(feature = "pulse")]
    pub fn fill_raw_buffer(
        &mut self,
        buffer: &mut Vec<u8>,
        limit: Option<usize>,
        volume: Arc<Mutex<Vec<f32>>>,
    ) -> Result<(), DecoderError> {
        let limit = limit.unwrap_or_else(|| {
            if buffer.capacity() > 0 {
                buffer.capacity()
            } else {
                1024
            }
        });

        while buffer.len() < limit {
            let audio_buffer = self.get_audio_buffer(volume.clone())?;

            match self.spec.format {
                AudioFormat::F32 => {
                    self.audio_to_raw::<f32>(audio_buffer, buffer);
                }

                AudioFormat::I32 | AudioFormat::U32 => {
                    self.audio_to_raw::<i32>(audio_buffer, buffer)
                }

                AudioFormat::I16 | AudioFormat::U16 => {
                    self.audio_to_raw::<i16>(audio_buffer, buffer);
                }
            };
        }
        Ok(())
    }

    #[cfg(feature = "pulse")]
    fn audio_to_raw<T>(&self, audio_buffer: AudioBuffer<f32>, buffer: &mut Vec<u8>)
    where
        T: RawSample + FromSample<f32>,
    {
        let mut raw_sample_buffer =
            RawSampleBuffer::<T>::new(audio_buffer.capacity() as u64, *audio_buffer.spec());
        raw_sample_buffer.copy_interleaved_typed::<f32>(&audio_buffer);
        buffer.extend_from_slice(raw_sample_buffer.as_bytes());
    }

    #[cfg(feature = "notify")]
    pub fn metadata(&mut self) -> Option<MetadataRevision> {
        self.probed
            .format
            .metadata()
            .current()
            .cloned()
            .or_else(|| {
                self.probed
                    .metadata
                    .get()
                    .as_ref()
                    .and_then(|m| m.current().cloned())
            })
    }

    // pub fn samples_to_dur(&self, samples: u64) -> Duration {
    //     Duration::from_micros(
    //         samples
    //             * self.spec.sample_rate as u64
    //             * self.spec.channels as u64
    //             * self.spec.format.size_of() as u64
    //             * 1_000_000,
    //     )
    // }

    pub fn dur_to_samples(&self, dur: Duration) -> u64 {
        self.spec.sample_rate as u64
            * self.spec.channels as u64
            * self.spec.format.size_of() as u64
            * dur.as_micros() as u64
            / 1_000_000
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
    #[cfg(feature = "pulse")] skip: Arc<AtomicCell<Duration>>,
    output_threshold: Duration,
) -> anyhow::Result<(Decoder, StreamParams)> {
    let ip = if server_ip.is_unspecified() {
        default_ip
    } else {
        server_ip
    };

    let data_stream = match make_connection(ip, server_port, http_headers) {
        Ok(data_s) => data_s,
        Err(e) => {
            warn!("Unable to connect to data stream at {}", ip);
            return Err(e);
        }
    };

    stream_in.send(PlayerMsg::Connected).ok();

    let mut data_stream = SlimBuffer::with_capacity(
        threshold as usize * 1024,
        data_stream,
        status.clone(),
        threshold,
        None,
    );

    stream_in.send(PlayerMsg::BufferThreshold).ok();

    // Read until we encounter the end of headers (a blank line: "\r\n\r\n")
    {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = data_stream.read_line(&mut line)?;
            if bytes_read == 0 || line == "\r\n" {
                break;
            }
        }
    }

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(data_stream)),
        Default::default(),
    );

    Ok((
        Decoder::try_new(mss, format, pcmsamplerate, pcmchannels)?,
        StreamParams {
            autostart,
            volume,
            #[cfg(feature = "pulse")]
            skip,
            output_threshold,
        },
    ))
}

fn make_connection(ip: Ipv4Addr, port: u16, http_headers: String) -> anyhow::Result<TcpStream> {
    let mut data_stream = TcpStream::connect((ip, port))?;
    let mut headers = Vec::new();
    headers.push(http_headers.trim());
    // headers.push("Icy-Metadata: 1");
    data_stream.write(headers.join("\r\n").as_bytes())?;
    data_stream.write("\r\n\r\n".as_bytes())?;
    data_stream.flush()?;
    Ok(data_stream)
}
