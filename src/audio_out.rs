use std::time::Duration;

use anyhow;
use crossbeam::channel::Sender;

use crate::{decode::Decoder, message::PlayerMsg, StreamParams};

#[cfg(feature = "pulse")]
use crate::pulse_out::PulseAudioOutput;
#[cfg(feature = "rodio")]
use crate::rodio_out::RodioAudioOutput;

pub trait AudioOutput {
    fn enqueue_new_stream(
        &mut self,
        decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        stream_params: StreamParams,
        device: &Option<String>,
    );

    fn unpause(&mut self) -> bool;

    fn pause(&mut self) -> bool;

    fn stop(&mut self);

    fn flush(&mut self);

    fn shift(&mut self);

    fn get_dur(&self) -> Duration;

    fn get_output_device_names(&self) -> anyhow::Result<Vec<(String, Option<String>)>>;
}

pub fn make_audio_output(
    system: &str,
    #[cfg(feature = "rodio")] device: &Option<String>,
) -> anyhow::Result<Box<dyn AudioOutput>> {
    Ok(match system {
        #[cfg(feature = "pulse")]
        "pulse" => Box::new(PulseAudioOutput::try_new()?),
        #[cfg(feature = "rodio")]
        "rodio" => Box::new(RodioAudioOutput::try_new(device)?),
        _ => unreachable!(),
    })
}
