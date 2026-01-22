use std::{
    io::{BufRead, Write},
    net::{Ipv4Addr, TcpStream},
    time::Duration,
};

use anyhow::{bail, Context};
#[cfg(not(feature = "pulse"))]
use crossbeam::channel::Sender;
#[cfg(feature = "pulse")]
use crossbeam::channel::Sender;

use slimproto::{
    buffer::SlimBuffer,
    proto::{PcmChannels, PcmSampleRate},
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

#[cfg(any(feature = "pulse", feature = "pipewire"))]
use symphonia::core::audio::{RawSample, RawSampleBuffer};

#[cfg(feature = "rodio")]
use symphonia::core::{audio::SampleBuffer, sample::Sample};

#[cfg(feature = "notify")]
use symphonia::core::meta::MetadataRevision;

use crate::{message::PlayerMsg, StreamParams, STATUS, VOLUME};

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
    #[allow(unused)]
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
        _pcmsamplesize: slimproto::proto::PcmSampleSize,
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

    fn get_audio_buffer(&mut self) -> Result<AudioBuffer<f32>, DecoderError> {
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

        let vol = VOLUME.lock().map(|v| v[0]).unwrap_or(0.5);

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
    ) -> Result<(), DecoderError>
    where
        T: Sample + FromSample<f32>,
    {
        let limit = limit.unwrap_or_else(|| buffer.capacity().max(1024));

        while buffer.len() < limit {
            let audio_buffer = self.get_audio_buffer()?;
            let mut sample_buffer =
                SampleBuffer::<T>::new(audio_buffer.capacity() as u64, *audio_buffer.spec());
            sample_buffer.copy_interleaved_typed::<f32>(&audio_buffer);
            buffer.extend_from_slice(sample_buffer.samples());
        }

        Ok(())
    }

    #[cfg(any(feature = "pulse", feature = "pipewire"))]
    pub fn fill_raw_buffer(
        &mut self,
        buffer: &mut Vec<u8>,
        limit: Option<usize>,
    ) -> Result<(), DecoderError> {
        let limit = limit.unwrap_or_else(|| (buffer.capacity() / 2).max(1024));

        while buffer.len() < limit {
            let audio_buffer = self.get_audio_buffer()?;

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

    #[cfg(any(feature = "pulse", feature = "pipewire"))]
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

    #[allow(unused)]
    pub fn samples_to_dur(&self, samples: u64) -> Duration {
        Duration::from_millis(
            samples * 1_000 / (self.spec.sample_rate as u64 * self.spec.channels as u64),
        )
    }

    pub fn dur_to_samples(&self, dur: Duration) -> u64 {
        self.spec.sample_rate as u64 * self.spec.channels as u64 * dur.as_micros() as u64
            / 1_000_000
    }
}

#[allow(clippy::too_many_arguments)]
pub fn make_decoder(
    server_ip: Ipv4Addr,
    default_ip: Ipv4Addr,
    server_port: u16,
    http_headers: String,
    stream_in: Sender<PlayerMsg>,
    threshold: u32,
    format: slimproto::proto::Format,
    pcmsamplesize: slimproto::proto::PcmSampleSize,
    pcmsamplerate: slimproto::proto::PcmSampleRate,
    pcmchannels: slimproto::proto::PcmChannels,
    autostart: slimproto::proto::AutoStart,
    output_threshold: Duration,
) -> anyhow::Result<(Decoder, StreamParams)> {
    let ip = if server_ip.is_unspecified() {
        default_ip
    } else {
        server_ip
    };

    let data_stream = make_connection(ip, server_port, http_headers)
        .context(format!("Unable to connect to data stream at {}", ip))?;
    stream_in.send(PlayerMsg::Connected).ok();

    let mut data_stream = SlimBuffer::with_capacity(
        threshold as usize * 1024,
        data_stream,
        STATUS.clone(),
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
        Decoder::try_new(mss, format, pcmsamplesize, pcmsamplerate, pcmchannels)?,
        StreamParams {
            autostart,
            output_threshold,
        },
    ))
}

fn make_connection(ip: Ipv4Addr, port: u16, http_headers: String) -> anyhow::Result<TcpStream> {
    let mut data_stream = TcpStream::connect((ip, port))?;
    let headers = http_headers.trim();
    // headers.push("Icy-Metadata: 1");
    _ = data_stream.write((format!("{}{}", headers, "\r\n")).as_bytes())?;
    _ = data_stream.write("\r\n\r\n".as_bytes())?;
    data_stream.flush()?;
    Ok(data_stream)
}
