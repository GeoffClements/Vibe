use std::{
    net::TcpStream,
    sync::{Arc, Mutex},
};

use anyhow;
use crossbeam::channel::Sender;
use log::{info, warn};
use slimproto::{
    buffer::SlimBuffer,
    proto::{PcmChannels, PcmSampleRate},
    status::{StatusCode, StatusData},
    ClientMessage,
};
use symphonia::core::{
    codecs::{Decoder as SymDecoder, DecoderOptions},
    formats::FormatOptions,
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::{Hint, ProbeResult},
};

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

enum AudioFormat {
    F32,
    I16,
    U16,
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
        data_stream: TcpStream,
        status: Arc<Mutex<StatusData>>,
        slim_tx: Sender<ClientMessage>,
        threshold: u32,
        format: slimproto::proto::Format,
        pcmsamplerate: slimproto::proto::PcmSampleRate,
        pcmchannels: slimproto::proto::PcmChannels,
    ) -> anyhow::Result<Option<Self>> {
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

        let probed = match symphonia::default::get_probe().format(
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
            .make(&track.codec_params, &DecoderOptions::default())?;

        Ok(Some(Decoder {
            probed: probed,
            decoder: decoder,
            spec: AudioSpec {
                channels: channels,
                sample_rate: sample_rate,
                format: AudioFormat::F32,
            },
        }))
    }

    pub fn channels(&self) -> u8 {
        self.spec.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.spec.sample_rate
    }
}
