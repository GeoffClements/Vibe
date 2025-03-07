use std::{
    io::Write,
    mem,
    net::{Ipv4Addr, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Error};
#[cfg(not(feature = "pulse"))]
use crossbeam::channel::Sender;
#[cfg(feature = "pulse")]
use crossbeam::{atomic::AtomicCell, channel::Sender};

use log::warn;
use slimproto::{
    buffer::SlimBuffer,
    proto::{PcmChannels, PcmSampleRate, PcmSampleSize},
    status::StatusData,
};

use symphonia::core::audio::conv::IntoSample;
use symphonia::core::{
    audio::{
        layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO},
        sample::{Sample, SampleFormat},
        Audio, AudioBuffer, AudioBytes, GenericAudioBufferRef,
    },
    codecs::{
        audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions},
        CodecParameters,
    },
    // conv::FromSample,
    formats::{probe::Hint, FormatOptions, FormatReader, Packet, TrackType},
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    // probe::{Hint, ProbeResult},
    // sample::SampleFormat,
};
// #[cfg(feature = "pulse")]
// use symphonia::core::audio::{RawSample, RawSampleBuffer};

// #[cfg(feature = "rodio")]
// use symphonia::core::{audio::SampleBuffer, sample::Sample};

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

pub struct Decoder<'s> {
    pub reader: Box<dyn FormatReader + 's>,
    pub decoder: Box<dyn AudioDecoder>,
    spec: AudioSpec,
}

impl<'s> Decoder<'s> {
    pub fn try_new(
        mss: MediaSourceStream<'s>,
        format: slimproto::proto::Format,
        pcmsamplesize: slimproto::proto::PcmSampleSize,
        pcmsamplerate: slimproto::proto::PcmSampleRate,
        pcmchannels: slimproto::proto::PcmChannels,
    ) -> anyhow::Result<Self> {
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

        let reader = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .context("Unrecognized container format")?;

        let track = reader
            .default_track(TrackType::Audio)
            .context("Unable to find default track")?;

        let sample_format = match pcmsamplesize {
            PcmSampleSize::Eight => Ok(SampleFormat::S8),
            PcmSampleSize::Sixteen => Ok(SampleFormat::S16),
            PcmSampleSize::Twenty => Ok(SampleFormat::S24),
            PcmSampleSize::ThirtyTwo => Ok(SampleFormat::S32),
            PcmSampleSize::SelfDescribing => match track.codec_params {
                Some(CodecParameters::Audio(AudioCodecParameters {
                    sample_format: Some(sf),
                    ..
                })) => Ok(sf),
                _ => Err(anyhow::Error::msg("Unable to set sample size")),
            },
        }?;

        let sample_rate = match pcmsamplerate {
            PcmSampleRate::Rate(rate) => Ok(rate),
            PcmSampleRate::SelfDescribing => match track.codec_params {
                Some(CodecParameters::Audio(AudioCodecParameters {
                    sample_rate: Some(sr),
                    ..
                })) => Ok(sr),
                _ => Err(anyhow::Error::msg("Unable to set sample rate")),
            },
        }?;

        let channels = match pcmchannels {
            PcmChannels::Mono => 1u8,
            PcmChannels::Stereo => 2,
            PcmChannels::SelfDescribing => match &track.codec_params {
                Some(CodecParameters::Audio(AudioCodecParameters {
                    channels: Some(channels),
                    ..
                })) => channels.count() as u8,
                _ => 2,
            },
        };

        // Create a decoder for the track.
        let audio_codec_params = match &track.codec_params {
            Some(CodecParameters::Audio(audio_codec_params)) => Ok(audio_codec_params),
            _ => Err(anyhow::Error::msg(
                "Unable to extract audio parameters from stream",
            )),
        }?;

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&audio_codec_params, &AudioDecoderOptions::default())
            .context("Unable to find suitable decoder")?;

        Ok(Decoder {
            reader,
            decoder,
            spec: AudioSpec {
                channels,
                sample_rate,
                format: sample_format.into(),
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

    fn get_audio_buffer(&mut self, volume: Arc<Mutex<Vec<f32>>>) -> Result<Vec<f32>, DecoderError> {
        let decoded = loop {
            let packet = self
                .reader
                .next_packet()
                .map_err(|err| match err {
                    symphonia::core::errors::Error::ResetRequired => {
                        self.decoder.reset();
                        DecoderError::Retry
                    }

                    error => DecoderError::StreamError(error),
                })?
                .ok_or(DecoderError::EndOfDecode)?;

            match self.decoder.decode(&packet) {
                symphonia::core::errors::Result::Ok(decoded) => break decoded,
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(e) => return Err(DecoderError::StreamError(e)),
            }
        };

        let (left_volume, right_volume) = volume
            .lock()
            .map(|vol| (vol[0], vol[1]))
            .unwrap_or((0.5, 0.5));

        let mut temp_buffer: Vec<f32> = Vec::with_capacity(decoded.byte_len_as::<f32>());
        decoded.copy_to_vec_interleaved(&mut temp_buffer);
        temp_buffer.as_mut_slice().chunks_exact_mut(2).for_each(|frame| {
            if let [l, r] = frame {
                    *l *= left_volume;
                    *r *= right_volume;
                }
        });

        Ok(temp_buffer)
    }

    #[cfg(feature = "rodio")]
    pub fn fill_sample_buffer(
        &mut self,
        buffer: &mut Vec<f32>,
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

            // let mut audio_spec = *audio_buffer.spec();
            // let mut sample_buffer = AudioBuffer::<T>::new(*audio_buffer.spec(), audio_buffer.capacity());
            // audio_buffer.as_generic_audio_buffer_ref().copy_bytes_interleaved_as::<T>(sample_buffer);
            // let mut sample_buffer =
            //     AudioBuffer::<T>::new(audio_buffer.capacity() as u64, *audio_buffer.spec());
            // sample_buffer.copy_interleaved_typed::<f32>(&audio_buffer);
            buffer.extend_from_slice(&audio_buffer[..]);
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
            buffer.extend_from_slice(
                &audio_buffer
                    .into_iter()
                    .flat_map(|s| s.to_ne_bytes().into_iter())
                    .collect()[..],
            );
            // match self.spec.format {
            //     AudioFormat::F32 => buffer.extend_from_slice(&audio_buffer.into_iter().flat_map(|s| s.to_le_bytes().into_iter()).collect()[..]),

            //     AudioFormat::I32 | AudioFormat::U32 => buffer.extend_from_slice(&audio_buffer.into_iter().map(|s| s as i32).collect()[..]),

            //     AudioFormat::F32 => {
            //         self.audio_to_raw::<f32>(audio_buffer, buffer);
            //     }

            //     AudioFormat::I32 | AudioFormat::U32 => {
            //         self.audio_to_raw::<i32>(audio_buffer, buffer)
            //     }

            //     AudioFormat::I16 | AudioFormat::U16 => {
            //         self.audio_to_raw::<i16>(audio_buffer, buffer);
            //     }
            // };
        }
        Ok(())
    }

    // #[cfg(feature = "pulse")]
    // fn audio_to_raw<T>(&self, audio_buffer: AudioBuffer<f32>, buffer: &mut Vec<u8>)
    // // where
    // //     T: RawSample + FromSample<f32>,
    // {
    //     let mut byte_buffer =
    //     // let mut raw_sample_buffer =
    //     //     RawSampleBuffer::<T>::new(audio_buffer.capacity() as u64, *audio_buffer.spec());
    //     // raw_sample_buffer.copy_interleaved_typed::<f32>(&audio_buffer);
    //     // buffer.extend_from_slice(raw_sample_buffer.as_bytes());
    // }

    #[cfg(feature = "notify")]
    pub fn metadata(&mut self) -> Option<MetadataRevision> {
        self.reader
            // .format
            .metadata()
            .current()
            .cloned()
        // .or_else(|| {
        //     self.reader
        //         .metadata
        //         .get()
        //         .as_ref()
        //         .and_then(|m| m.current().cloned())
        // })
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

pub fn make_decoder<'s>(
    server_ip: Ipv4Addr,
    default_ip: Ipv4Addr,
    server_port: u16,
    http_headers: String,
    stream_in: Sender<PlayerMsg>,
    status: Arc<Mutex<StatusData>>,
    threshold: u32,
    format: slimproto::proto::Format,
    pcmsamplesize: slimproto::proto::PcmSampleSize,
    pcmsamplerate: slimproto::proto::PcmSampleRate,
    pcmchannels: slimproto::proto::PcmChannels,
    autostart: slimproto::proto::AutoStart,
    volume: Arc<Mutex<Vec<f32>>>,
    #[cfg(feature = "pulse")] skip: Arc<AtomicCell<Duration>>,
    output_threshold: Duration,
) -> anyhow::Result<(Arc<Decoder<'s>>, StreamParams)> {
    let ip = if server_ip.is_unspecified() {
        default_ip
    } else {
        server_ip
    };

    let data_stream = make_connection(ip, server_port, http_headers)
        .context(format!("Unable to connect to data stream at {}", ip))?;
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

    Ok((
        Arc::new(Decoder::try_new(mss, format, pcmsamplesize, pcmsamplerate, pcmchannels)?),
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
