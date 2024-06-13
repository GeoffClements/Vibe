use std::{
    io::Write,
    net::{Ipv4Addr, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::bail;
use crossbeam::{atomic::AtomicCell, channel::Sender};
use log::{error, info, warn};
use slimproto::{status::StatusData, ClientMessage};

use crate::{
    decode::{self, Decoder},
    PlayerMsg,
};

pub struct Stream;
#[derive(Default)]
pub struct AudioOutput {
    playing: Option<Stream>,
    next_up: Option<Stream>,
}

impl AudioOutput {
    pub fn try_new() -> anyhow::Result<Self> {
        Ok(AudioOutput::default())
    }

    pub fn make_stream(
        &mut self,
        server_ip: Ipv4Addr,
        default_ip: &Ipv4Addr,
        server_port: u16,
        http_headers: String,
        status: Arc<Mutex<StatusData>>,
        _slim_tx: Sender<ClientMessage>,
        stream_in: Sender<PlayerMsg>,
        threshold: u32,
        format: slimproto::proto::Format,
        pcmsamplerate: slimproto::proto::PcmSampleRate,
        pcmchannels: slimproto::proto::PcmChannels,
        _skip: Arc<AtomicCell<Duration>>,
        _volume: Arc<Mutex<Vec<f32>>>,
        _output_threshold: Duration,
    ) {
        let default_ip = default_ip.clone();

        thread::spawn(move || {
            let ip = if server_ip.is_unspecified() {
                default_ip
            } else {
                server_ip
            };

            let data_stream = match make_connection(ip, server_port, http_headers) {
                Ok(data_s) => data_s,
                Err(_) => {
                    warn!("Unable to connect to data stream at {}", ip);
                    return;
                }
            };

            stream_in.send(PlayerMsg::Connected).ok();

            let mss = decode::media_source(data_stream, status.clone(), threshold);
            stream_in.send(PlayerMsg::BufferThreshold).ok();

            let decoder = match Decoder::try_new(mss, format, pcmsamplerate, pcmchannels) {
                Some(decoder) => decoder,
                None => {
                    stream_in.send(PlayerMsg::NotSupported).ok();
                    return;
                }
            };

            // Do stream creation stuff

            stream_in.send(PlayerMsg::StreamEstablished(Stream)).ok();
        });
    }

    pub fn enqueue(&mut self, stream: Stream) {
        if self.playing.is_some() {
            self.next_up = Some(stream);
        } else {
            self.playing = Some(stream);
        }
    }

    pub fn play_next(&mut self) -> bool {
        self.playing = self.next_up.take();
        self.playing.is_some()
    }

    pub fn unpause(&mut self) -> bool {
        self.playing.is_some()
    }
}

pub fn get_output_device_names() -> anyhow::Result<Vec<String>> {
    Ok(vec!["dummy_out1".to_string(), "dummy_out2".to_string()])
}

// TODO: put this in slimproto
fn make_connection(ip: Ipv4Addr, port: u16, http_headers: String) -> anyhow::Result<TcpStream> {
    let mut data_stream = TcpStream::connect((ip, port))?;
    data_stream.write(http_headers.as_bytes())?;
    data_stream.flush()?;
    Ok(data_stream)
}
