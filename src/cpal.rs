use std::sync::{Arc, Mutex};

use anyhow;
use cpal::traits::{DeviceTrait, HostTrait};
use crossbeam::channel::Sender;
use slimproto::status::StatusData;

use crate::{decode::Decoder, PlayerMsg, StreamParams};

pub struct AudioOutput;

impl AudioOutput {
    pub fn try_new() -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub fn enqueue_new_stream(
        &mut self,
        mut decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        status: Arc<Mutex<StatusData>>,
        stream_params: StreamParams,
        device: &Option<String>,
    ) {
    }

    pub fn unpause(&mut self) -> bool {
        true
    }

    pub fn pause(&mut self) -> bool {
        true
    }

    pub fn stop(&mut self) {}

    pub fn flush(&mut self) {}

    pub fn shift(&mut self) {}
}

pub fn get_output_device_names() -> anyhow::Result<Vec<String>> {
    let host = cpal::default_host();
    let devices = host.output_devices()?;
    let ret = devices
        .map(|d| d.name())
        .filter(|n| n.is_ok())
        .map(|n| n.unwrap())
        .collect();
    Ok(ret)
}
