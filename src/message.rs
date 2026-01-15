use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use crossbeam::{atomic::AtomicCell, channel::Sender};
use log::{info, warn};
use slimproto::{
    status::{StatusCode, StatusData},
    ClientMessage, ServerMessage,
};

#[cfg(feature = "notify")]
use crate::notify::notify;
use crate::{
    audio_out::{self, AudioOutput},
    decode, StreamParams,
};

#[allow(unused)]
pub enum PlayerMsg {
    EndOfDecode,
    Drained,
    Pause,
    Unpause,
    Connected,
    BufferThreshold,
    NotSupported,
    StreamEstablished,
    TrackStarted,
    Decoder((decode::Decoder, StreamParams)),
}

#[allow(clippy::too_many_arguments)]
pub fn process_slim_msg(
    output: &mut Option<Box<dyn AudioOutput>>,
    msg: ServerMessage,
    server_default_ip: &mut Ipv4Addr,
    name: Arc<RwLock<String>>,
    slim_tx_in: Sender<ClientMessage>,
    volume: Arc<Mutex<Vec<f32>>>,
    status: Arc<Mutex<StatusData>>,
    stream_in: Sender<PlayerMsg>,
    skip: Arc<AtomicCell<Duration>>,
    start_time: &Instant,
    output_system: &str,
    #[cfg(feature = "rodio")] device: &Option<String>,
) -> anyhow::Result<()> {
    // println!("{:?}", msg);
    match msg {
        ServerMessage::Serv { ip_address, .. } => {
            info!("Switching to server at {ip_address}");
            *server_default_ip = ip_address;
        }

        ServerMessage::Queryname => {
            log::info!("Name query from server");
            if let Ok(name) = name.read() {
                info!("Sending name: {name}");
                slim_tx_in.send(ClientMessage::Name(name.to_owned())).ok();
            }
        }

        ServerMessage::Setname(new_name) => {
            if let Ok(mut name) = name.write() {
                info!("Set name to {new_name}");
                *name = new_name;
            }
        }

        ServerMessage::Gain(left, right) => {
            info!("Setting volume to ({left}, {right})");
            if let Ok(mut vol) = volume.lock() {
                let left = if left > 1.0 { 1.0f32 } else { left as f32 };
                let right = if right > 1.0 { 1.0f32 } else { right as f32 };
                vol[0] = left.sqrt();
                vol[1] = right.sqrt();
            }
        }

        ServerMessage::Status(ts) => {
            // info!("Received status tick from server with timestamp {:#?}", ts);
            let play_time = match output {
                Some(output) => output.get_dur(),
                None => Duration::ZERO,
            };

            if let Ok(mut status) = status.lock() {
                // info!("Sending status update - jiffies: {:?}", status.get_jiffies());
                status.set_elapsed_milli_seconds(play_time.as_millis() as u32);
                status.set_elapsed_seconds(play_time.as_secs() as u32);
                status.set_timestamp(ts);

                let msg = status.make_status_message(StatusCode::Timer);
                slim_tx_in.send(msg).ok();
            }
        }

        ServerMessage::Stop => {
            info!("Stop playback received");
            if let Some(output) = output {
                output.stop();
            }

            if let Ok(mut status) = status.lock() {
                status.set_elapsed_milli_seconds(0);
                status.set_elapsed_seconds(0);
                status.set_output_buffer_size(0);
                status.set_output_buffer_fullness(0);
                info!("Player flushed");
                let msg = status.make_status_message(StatusCode::Flushed);
                slim_tx_in.send(msg).ok();
            }
        }

        ServerMessage::Flush => {
            info!("Flushing");
            if let Some(output) = output {
                output.flush();
            }

            if let Ok(mut status) = status.lock() {
                status.set_elapsed_milli_seconds(0);
                status.set_elapsed_seconds(0);
                status.set_output_buffer_size(0);
                status.set_output_buffer_fullness(0);
                info!("Player flushed");
                let msg = status.make_status_message(StatusCode::Flushed);
                slim_tx_in.send(msg).ok();
            }
        }

        ServerMessage::Pause(interval) => {
            let play_time = match output {
                Some(output) => output.get_dur(),
                None => Duration::ZERO,
            };

            info!("Pause requested with interval {:?}", interval);
            if let Some(output) = output {
                if interval.is_zero() {
                    if output.pause() {
                        if let Ok(mut status) = status.lock() {
                            info!("Sending paused to server");
                            status.set_elapsed_milli_seconds(play_time.as_millis() as u32);
                            status.set_elapsed_seconds(play_time.as_secs() as u32);
                            let msg = status.make_status_message(StatusCode::Pause);
                            slim_tx_in.send(msg).ok();
                        }
                    }
                } else if output.pause() {
                    let stream_in = stream_in.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(interval);
                        stream_in.send(PlayerMsg::Unpause).ok();
                    });
                }
            }
        }

        ServerMessage::Unpause(interval) => {
            info!("Resume requested with interval {:?}", interval);

            let play_time = match output {
                Some(output) => output.get_dur(),
                None => Duration::ZERO,
            };

            if interval.is_zero() {
                if let Some(output) = output {
                    if output.unpause() {
                        if let Ok(mut status) = status.lock() {
                            info!("Sending resumed to server");
                            status.set_elapsed_milli_seconds(play_time.as_millis() as u32);
                            status.set_elapsed_seconds(play_time.as_secs() as u32);
                            let msg = status.make_status_message(StatusCode::Resume);
                            slim_tx_in.send(msg).ok();
                        }
                    }
                }
            } else {
                let dur = interval.saturating_sub(Instant::now() - *start_time);
                info!("Resuming in {:?}", dur);
                let stream_in = stream_in.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(dur);
                    stream_in.send(PlayerMsg::Unpause).ok();
                    if let Ok(mut status) = status.lock() {
                        info!("Sending resumed to server");
                        status.set_elapsed_milli_seconds(play_time.as_millis() as u32);
                        status.set_elapsed_seconds(play_time.as_secs() as u32);
                        let msg = status.make_status_message(StatusCode::Resume);
                        slim_tx_in.send(msg).ok();
                    }
                });
            }
        }

        ServerMessage::Skip(interval) => {
            info!("Skip ahead: {:?}", interval);
            skip.store(interval);
        }

        ServerMessage::Stream {
            http_headers,
            server_ip,
            server_port,
            threshold,
            format,
            pcmsamplesize,
            pcmsamplerate,
            pcmchannels,
            autostart,
            output_threshold,
            ..
        } => {
            info!("Start stream command from server");
            info!("\tFormat: {:?}", format);
            info!("\tThreshold: {} bytes", threshold);
            info!("\tOutput threshold: {:?}", output_threshold);
            if let Some(http_headers) = http_headers {
                let num_crlf = http_headers.matches("\r\n").count();

                if num_crlf > 0 {
                    if let Ok(mut status) = status.lock() {
                        status.add_crlf(num_crlf as u8);
                    }

                    let stream_in_r = stream_in.clone();
                    let default_ip = *server_default_ip;
                    std::thread::spawn(move || {
                        match decode::make_decoder(
                            server_ip,
                            default_ip,
                            server_port,
                            http_headers,
                            stream_in_r.clone(),
                            status,
                            threshold,
                            format,
                            pcmsamplesize,
                            pcmsamplerate,
                            pcmchannels,
                            autostart,
                            volume.clone(),
                            #[cfg(any(feature = "pulse", feature = "pipewire"))]
                            skip.clone(),
                            output_threshold,
                        ) {
                            Ok(decoder_params) => {
                                stream_in_r.send(PlayerMsg::Decoder(decoder_params)).ok();
                            }
                            Err(e) => {
                                warn!("{}", e);
                                stream_in_r.send(PlayerMsg::NotSupported).ok();
                            }
                        }
                    });
                }
            }
        }

        ServerMessage::Enable(spdif, dac) => {
            if spdif && dac {
                info!("Connecting output");
                *output = audio_out::make_audio_output(
                    output_system,
                    #[cfg(feature = "rodio")]
                    device,
                )
                .ok();
            } else {
                info!("Disconnecting output");
                *output = None;
            }
        }

        ServerMessage::DisableDac => {
            info!("Disconnecting output");
            *output = None;
        }

        cmd => {
            warn!("Unimplemented command: {:?}", cmd);
        }
    }

    Ok(())
}

pub fn process_stream_msg(
    msg: PlayerMsg,
    status: Arc<Mutex<StatusData>>,
    slim_tx_in: Sender<ClientMessage>,
    output: &mut Option<Box<dyn AudioOutput>>,
    stream_in: Sender<PlayerMsg>,
    device: &Option<String>,
    #[cfg(feature = "notify")] quiet: &bool,
) {
    match msg {
        PlayerMsg::EndOfDecode => {
            if let Ok(mut status) = status.lock() {
                info!("Decoder ready for new stream");
                let msg = status.make_status_message(StatusCode::DecoderReady);
                slim_tx_in.send(msg).ok();
            }
        }

        PlayerMsg::Drained => {
            info!("End of track");
            if let Some(output) = output {
                output.shift();
                output.unpause();
                if let Ok(mut status) = status.lock() {
                    status.set_elapsed_milli_seconds(0);
                    status.set_elapsed_seconds(0);
                }
            }
        }

        PlayerMsg::Pause => {
            info!("Pausing track");
            if let Some(output) = output {
                output.pause();
            }
        }

        PlayerMsg::Unpause => {
            if let Some(output) = output {
                if output.unpause() {
                    info!("Sending track unpaused by player");
                    if let Ok(mut status) = status.lock() {
                        let msg = status.make_status_message(StatusCode::TrackStarted);
                        slim_tx_in.send(msg).ok();
                    }
                }
            }
        }

        PlayerMsg::Connected => {
            if let Ok(mut status) = status.lock() {
                info!("Sending stream connected");
                let msg = status.make_status_message(StatusCode::Connect);
                slim_tx_in.send(msg).ok();
            }
        }

        PlayerMsg::BufferThreshold => {
            if let Ok(mut status) = status.lock() {
                info!("Sending buffer threshold reached");
                let msg = status.make_status_message(StatusCode::BufferThreshold);
                slim_tx_in.send(msg).ok();
            }
        }

        PlayerMsg::NotSupported => {
            warn!("Unsupported format");
            if let Ok(mut status) = status.lock() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx_in.send(msg).ok();
            }
        }

        PlayerMsg::StreamEstablished => {
            if let Ok(mut status) = status.lock() {
                info!("Sending stream established");
                let msg = status.make_status_message(StatusCode::StreamEstablished);
                slim_tx_in.send(msg).ok();
            }
        }

        PlayerMsg::TrackStarted => {
            info!("Sending track started");
            if let Ok(mut status) = status.lock() {
                status.set_elapsed_milli_seconds(0);
                status.set_elapsed_seconds(0);
                let msg = status.make_status_message(StatusCode::TrackStarted);
                slim_tx_in.send(msg).ok();
            }
        }

        #[cfg(not(feature = "notify"))]
        PlayerMsg::Decoder((decoder, stream_params)) => {
            if let Some(output) = output {
                output.enqueue_new_stream(decoder, stream_in.clone(), stream_params, device)
            }
        }

        #[cfg(feature = "notify")]
        PlayerMsg::Decoder((mut decoder, stream_params)) => {
            if let Some(metadata) = decoder.metadata() {
                if !quiet {
                    notify(metadata);
                }
            }

            if let Some(output) = output {
                output.enqueue_new_stream(decoder, stream_in.clone(), stream_params, device)
            }
        }
    }
}
