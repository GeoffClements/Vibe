use std::time::Duration;

use anyhow;
use crossbeam::channel::Sender;

use crate::{decode::Decoder, pulse_out, PlayerMsg, StreamParams};

#[cfg(feature = "rodio")]
use crate::rodio_out;

pub enum AudioOutput {
    Pulse(pulse_out::AudioOutput),
    #[cfg(feature = "rodio")]
    Rodio(rodio_out::AudioOutput),
}

impl AudioOutput {
    #[allow(unused)]
    pub fn try_new(system: &str, device: &Option<String>) -> anyhow::Result<Self> {
        Ok(match system {
            #[cfg(feature = "rodio")]
            "rodio" => Self::Rodio(rodio_out::AudioOutput::try_new(device)?),
            _ => Self::Pulse(pulse_out::AudioOutput::try_new()?),
        })
    }

    pub fn enqueue_new_stream(
        &mut self,
        decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        stream_params: StreamParams,
        device: &Option<String>,
    ) {
        match self {
            Self::Pulse(out) => out.enqueue_new_stream(decoder, stream_in, stream_params, device),
            #[cfg(feature = "rodio")]
            Self::Rodio(out) => out.enqueue_new_stream(decoder, stream_in, stream_params, device),
        }
    }

    pub fn unpause(&mut self) -> bool {
        match self {
            Self::Pulse(out) => out.unpause(),
            #[cfg(feature = "rodio")]
            Self::Rodio(out) => out.unpause(),
        }
    }

    pub fn pause(&mut self) -> bool {
        match self {
            Self::Pulse(out) => out.pause(),
            #[cfg(feature = "rodio")]
            Self::Rodio(out) => out.pause(),
        }
    }

    pub fn stop(&mut self) {
        match self {
            Self::Pulse(out) => out.stop(),
            #[cfg(feature = "rodio")]
            Self::Rodio(out) => out.stop(),
        }
    }

    pub fn flush(&mut self) {
        match self {
            Self::Pulse(out) => out.flush(),
            #[cfg(feature = "rodio")]
            Self::Rodio(out) => out.flush(),
        }
    }

    pub fn shift(&mut self) {
        match self {
            Self::Pulse(out) => out.shift(),
            #[cfg(feature = "rodio")]
            Self::Rodio(out) => out.shift(),
        }
    }

    pub fn get_dur(&self) -> Duration {
        match self {
            Self::Pulse(out) => out.get_dur(),
            #[cfg(feature = "rodio")]
            Self::Rodio(out) => out.get_dur(),
        }
    }

    pub fn get_output_device_names(&self) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Pulse(out) => out.get_output_device_names(),
            #[cfg(feature = "rodio")]
            Self::Rodio(out) => out.get_output_device_names(),
        }
    }
}
