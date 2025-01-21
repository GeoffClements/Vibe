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
use symphonia::core::meta::{MetadataRevision, StandardTagKey};

use crate::{audio_out::AudioOutput, decode, StreamParams};

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
    TrackStarted(Option<MetadataRevision>),
    Decoder((decode::Decoder, StreamParams)),
}

pub fn process_slim_msg(
    output: &mut AudioOutput,
    msg: ServerMessage,
    server_default_ip: &mut Ipv4Addr,
    name: Arc<RwLock<String>>,
    slim_tx_in: Sender<ClientMessage>,
    volume: Arc<Mutex<Vec<f32>>>,
    status: Arc<Mutex<StatusData>>,
    stream_in: Sender<PlayerMsg>,
    skip: Arc<AtomicCell<Duration>>,
    start_time: &Instant,
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

        ServerMessage::Gain(l, r) => {
            info!("Setting volume to ({l}, {r})");
            if let Ok(mut vol) = volume.lock() {
                vol[0] = l.sqrt() as f32;
                vol[1] = r.sqrt() as f32;
            }
        }

        ServerMessage::Status(ts) => {
            // info!("Received status tick from server with timestamp {:#?}", ts);
            let dur = output.get_dur();
            if let Ok(mut status) = status.lock() {
                // info!("Sending status update - jiffies: {:?}", status.get_jiffies());
                status.set_elapsed_milli_seconds(dur.as_millis() as u32);
                status.set_elapsed_seconds(dur.as_secs() as u32);
                status.set_timestamp(ts);

                let msg = status.make_status_message(StatusCode::Timer);
                slim_tx_in.send(msg).ok();
            }
        }

        ServerMessage::Stop => {
            info!("Stop playback received");
            output.stop();
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
            output.flush();
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
            info!("Pause requested with interval {:?}", interval);
            if interval.is_zero() {
                if output.pause() {
                    if let Ok(mut status) = status.lock() {
                        info!("Sending paused to server");
                        let msg = status.make_status_message(StatusCode::Pause);
                        slim_tx_in.send(msg).ok();
                    }
                }
            } else {
                if output.pause() {
                    std::thread::spawn(move || {
                        std::thread::sleep(interval);
                        stream_in.send(PlayerMsg::Unpause).ok();
                    });
                }
            }
        }

        ServerMessage::Unpause(interval) => {
            info!("Resume requested with interval {:?}", interval);
            if interval.is_zero() {
                if output.unpause() {
                    if let Ok(mut status) = status.lock() {
                        info!("Sending resumed to server");
                        let msg = status.make_status_message(StatusCode::Resume);
                        slim_tx_in.send(msg).ok();
                    }
                }
            } else {
                let dur = interval.saturating_sub(Instant::now() - *start_time);
                info!("Resuming in {:?}", dur);
                std::thread::spawn(move || {
                    std::thread::sleep(dur);
                    stream_in.send(PlayerMsg::Unpause).ok();
                    if let Ok(mut status) = status.lock() {
                        info!("Sending resumed to server");
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
                    let default_ip = server_default_ip.clone();
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
                            pcmsamplerate,
                            pcmchannels,
                            autostart,
                            volume.clone(),
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
    output: &mut AudioOutput,
    stream_in: Sender<PlayerMsg>,
    device: &Option<String>,
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
            output.shift();
            output.unpause();
        }

        PlayerMsg::Pause => {
            info!("Pausing track");
            output.pause();
        }

        PlayerMsg::Unpause => {
            if output.unpause() {
                info!("Sending track unpaused by player");
                if let Ok(mut status) = status.lock() {
                    let msg = status.make_status_message(StatusCode::TrackStarted);
                    slim_tx_in.send(msg).ok();
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

        PlayerMsg::TrackStarted(metadata) => {
            info!("Sending track started");
            if let Ok(mut status) = status.lock() {
                status.set_elapsed_milli_seconds(0);
                status.set_elapsed_seconds(0);
                let msg = status.make_status_message(StatusCode::TrackStarted);
                slim_tx_in.send(msg).ok();
            }

            if let Some(md) = metadata {
                let artist = {
                    let artist = md
                        .tags()
                        .to_vec()
                        .iter()
                        .find(|t| t.std_key == Some(StandardTagKey::AlbumArtist))
                        .map(|t| match &t.value {
                            symphonia::core::meta::Value::String(s) => s.to_owned(),
                            _ => "{unknown}".to_owned(),
                        });

                    if artist.is_none() {
                        md.tags()
                            .to_vec()
                            .iter()
                            .find(|t| t.std_key == Some(StandardTagKey::Artist))
                            .map(|t| match &t.value {
                                symphonia::core::meta::Value::String(s) => s.to_owned(),
                                _ => "{unknown}".to_owned(),
                            })
                    } else {
                        artist
                    }
                };

                let track = md
                    .tags()
                    .to_vec()
                    .iter()
                    .find(|t| t.std_key == Some(StandardTagKey::TrackTitle))
                    .map(|t| match &t.value {
                        symphonia::core::meta::Value::String(s) => s.to_owned(),
                        _ => "{unknown}".to_owned(),
                    });

                let album = md
                    .tags()
                    .to_vec()
                    .iter()
                    .find(|t| t.std_key == Some(StandardTagKey::Album))
                    .map(|t| match &t.value {
                        symphonia::core::meta::Value::String(s) => s.to_owned(),
                        _ => "{unknown}".to_owned(),
                    });

                if let (Some(track), Some(album), Some(artist)) = (track, album, artist) {
                    info!("Playing {} by {} from {}", track, artist, album);
                }
            }
        }

        PlayerMsg::Decoder((decoder, stream_params)) => {
            output.enqueue_new_stream(decoder, stream_in.clone(), stream_params, device)
        }
    }
}
