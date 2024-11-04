use std::{
    net::{Ipv4Addr, SocketAddrV4},
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use clap::{
    builder::{PossibleValuesParser, TypedValueParser},
    Parser,
};
use crossbeam::{
    atomic::AtomicCell,
    channel::{bounded, Select, Sender},
};

#[cfg(feature = "dummy")]
use dummy as output;

#[cfg(feature = "pulse")]
use pulse as output;

#[cfg(feature = "cpal")]
use cpal as output;

use log::{info, warn};
use output::AudioOutput;
use simple_logger::SimpleLogger;
use slimproto::{
    proto::{ClientMessage, ServerMessage, SLIM_PORT},
    status::{StatusCode, StatusData},
};

#[cfg(feature = "dummy")]
mod dummy;

#[cfg(feature = "pulse")]
mod pulse;

#[cfg(feature = "cpal")]
mod cpal;

mod decode;
mod proto;

#[derive(Parser)]
#[command(name = "Vibe", author, version, about, long_about = None)]
struct Cli {
    #[arg(short, name = "SERVER[:PORT]", value_parser = cli_server_parser, help = "Connect to the specified server, otherwise use autodiscovery")]
    server: Option<SocketAddrV4>,

    #[arg(
        short = 'o',
        name = "OUTPUT_DEVICE",
        help = "Output device [default: System default device]"
    )]
    device: Option<String>,

    #[arg(short, help = "List output devices")]
    list: bool,

    #[arg(short, default_value = "Vibe", help = "Set the player name")]
    name: String,

    #[arg(long,
        default_value = "off",
        value_parser = PossibleValuesParser::new(["trace", "debug", "error", "warn", "info", "off"])
            .map(|s| s.parse::<log::LevelFilter>().unwrap()),
        help = "Set the highest log level")]
    loglevel: log::LevelFilter,
}

fn cli_server_parser(value: &str) -> anyhow::Result<SocketAddrV4> {
    match value.split_once(':') {
        Some((ip_str, port_str)) if port_str.len() == 0 => {
            Ok(SocketAddrV4::new(Ipv4Addr::from_str(ip_str)?, SLIM_PORT))
        }
        Some(_) => Ok(value.parse()?),
        None => Ok(SocketAddrV4::new(Ipv4Addr::from_str(value)?, SLIM_PORT)),
    }
}

pub struct StreamParams {
    autostart: slimproto::proto::AutoStart,
    volume: Arc<Mutex<Vec<f32>>>,
    skip: Arc<AtomicCell<Duration>>,
    output_threshold: Duration,
}

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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    SimpleLogger::new()
        .with_colors(true)
        .with_level(cli.loglevel)
        .init()?;

    // Create a pulse audio threaded main loop and context
    // let (ml, cx) = pulse::setup()?;
    let mut output = AudioOutput::try_new()?;

    // List the output devices and terminate
    if cli.list {
        println!("Output devices:");
        let names = output::get_output_device_names()?;
        names
            .iter()
            .enumerate()
            .for_each(|(i, name)| println!("{}: {}", i, name));
        print!("Found {} device", names.len());
        if names.len() != 1 {
            print!("s");
        }
        println!();
        return Ok(());
    }

    loop {
        // Start the slim protocol threads
        let status = Arc::new(Mutex::new(StatusData::default()));
        let start_time = Instant::now();
        let mut server_default_ip = *cli.server.unwrap_or(SocketAddrV4::new(0.into(), 0)).ip();
        let name = Arc::new(RwLock::new(cli.name.to_owned()));
        let skip = Arc::new(AtomicCell::new(Duration::ZERO));
        let (slim_tx_in, slim_tx_out) = bounded(1);
        let (slim_rx_in, slim_rx_out) = bounded(1);
        proto::run(
            cli.server,
            name.clone(),
            slim_rx_in.clone(),
            slim_tx_out.clone(),
        );

        let volume = Arc::new(Mutex::new(vec![1.0f32, 1.0]));
        // let mut volume = ChannelVolumes::default();
        // volume.set_len(2);

        let (stream_in, stream_out) = bounded(10);

        let mut select = Select::new();
        let slim_idx = select.recv(&slim_rx_out);
        let stream_idx = select.recv(&stream_out);

        loop {
            match select.select() {
                op if op.index() == slim_idx => match op.recv(&slim_rx_out)? {
                    Some(msg) => process_slim_msg(
                        &mut output,
                        msg,
                        &mut server_default_ip,
                        name.clone(),
                        slim_tx_in.clone(),
                        volume.clone(),
                        status.clone(),
                        stream_in.clone(),
                        skip.clone(),
                        &start_time,
                    )?,
                    None => {
                        info!("Lost contact with server, resetting");
                        slim_tx_in.send(ClientMessage::Bye(1)).ok();
                        output.stop();
                        break;
                    }
                },
                op if op.index() == stream_idx => {
                    let msg = op.recv(&stream_out)?;
                    process_stream_msg(
                        msg,
                        status.clone(),
                        slim_tx_in.clone(),
                        &mut output,
                        stream_in.clone(),
                        &cli.device,
                    );
                }
                _ => {}
            }
        }
    }
}

fn process_slim_msg(
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
            if let Ok(mut status) = status.lock() {
                // info!("Sending status update - jiffies: {:?}", status.get_jiffies());
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

fn process_stream_msg(
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
            if output.unpause() {
                info!("Sending new track started");
                if let Ok(mut status) = status.lock() {
                    let msg = status.make_status_message(StatusCode::TrackStarted);
                    slim_tx_in.send(msg).ok();
                }
            }
        }
        PlayerMsg::Pause => {
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
        PlayerMsg::TrackStarted => {
            info!("Sending track started");
            if let Ok(mut status) = status.lock() {
                let msg = status.make_status_message(StatusCode::TrackStarted);
                slim_tx_in.send(msg).ok();
            }
        }
        PlayerMsg::Decoder((decoder, stream_params)) => output.enqueue_new_stream(
            decoder,
            stream_in.clone(),
            status.clone(),
            stream_params,
            device,
        ),
    }
}
