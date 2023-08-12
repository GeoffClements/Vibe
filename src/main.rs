use std::{
    cell::RefCell,
    net::{Ipv4Addr, SocketAddrV4},
    rc::Rc,
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use anyhow;
use clap::{
    builder::{PossibleValuesParser, TypedValueParser},
    Parser,
};
use crossbeam::{
    atomic::AtomicCell,
    channel::{bounded, Select, Sender},
};
use libpulse_binding as pa;
use log::{info, warn};
use pa::{context::Context, mainloop::threaded::Mainloop};
use simple_logger::SimpleLogger;
use slimproto::{
    proto::{ClientMessage, ServerMessage, SLIM_PORT},
    status::{StatusCode, StatusData},
};

mod proto;
mod pulse;
mod stream;

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

pub enum PlayerMsg {
    EndOfDecode,
    Drained,
    Pause,
    Unpause,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    SimpleLogger::new()
        .with_colors(true)
        .with_level(cli.loglevel)
        .init()?;

    // Create a pulse audio threaded main loop and context
    let (ml, cx) = pulse::setup()?;

    // List the output devices and terminate
    if cli.list {
        print_output_devices(ml.clone(), cx.clone());
        return Ok(());
    }

    // Start the slim protocol threads
    let status = Arc::new(RwLock::new(StatusData::default()));
    let mut server_default_ip = *cli.server.unwrap_or(SocketAddrV4::new(0.into(), 0)).ip();
    let name = Arc::new(RwLock::new((&cli.name).to_owned()));
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

    let mut streams = stream::StreamQueue::new();
    let (stream_in, stream_out) = bounded(1);

    let mut select = Select::new();
    let slim_idx = select.recv(&slim_rx_out);
    let stream_idx = select.recv(&stream_out);

    loop {
        match select.select() {
            op if op.index() == slim_idx => {
                let msg = op.recv(&slim_rx_out)?;
                process_slim_msg(
                    msg,
                    &mut server_default_ip,
                    name.clone(),
                    slim_tx_in.clone(),
                    volume.clone(),
                    status.clone(),
                    stream_in.clone(),
                    ml.clone(),
                    cx.clone(),
                    &mut streams,
                    skip.clone(),
                    &cli,
                )?;
            }
            op if op.index() == stream_idx => {
                let msg = op.recv(&stream_out)?;
                process_stream_msg(
                    msg,
                    status.clone(),
                    slim_tx_in.clone(),
                    &mut streams,
                    stream_in.clone(),
                    ml.clone(),
                );
            }
            _ => {}
        }
    }
}

fn process_slim_msg(
    msg: ServerMessage,
    server_default_ip: &mut Ipv4Addr,
    name: Arc<RwLock<String>>,
    slim_tx_in: Sender<ClientMessage>,
    volume: Arc<Mutex<Vec<f32>>>,
    status: Arc<RwLock<StatusData>>,
    stream_in: Sender<PlayerMsg>,
    ml: Rc<RefCell<Mainloop>>,
    cx: Rc<RefCell<Context>>,
    streams: &mut stream::StreamQueue,
    skip: Arc<AtomicCell<Duration>>,
    cli: &Cli,
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
            if let Ok(mut status) = status.write() {
                status.set_timestamp(ts);
                let msg = status.make_status_message(StatusCode::Timer);
                // info!("Sending status update");
                slim_tx_in.send(msg).ok();
            }
        }
        ServerMessage::Stop => {
            info!("Stop playback received");
            ml.borrow_mut().lock();
            streams.stop();
            if let Ok(status) = status.read() {
                info!("Player flushed");
                let msg = status.make_status_message(StatusCode::Flushed);
                slim_tx_in.send(msg).ok();
            }
            ml.borrow_mut().unlock();
        }
        ServerMessage::Flush => {
            info!("Flushing");
            ml.borrow_mut().lock();
            streams.flush();
            if let Ok(status) = status.read() {
                info!("Player flushed");
                let msg = status.make_status_message(StatusCode::Flushed);
                slim_tx_in.send(msg).ok();
            }
            ml.borrow_mut().unlock();
        }
        ServerMessage::Pause(interval) => {
            info!("Pause requested with interval {:?}", interval);
            if interval.is_zero() {
                ml.borrow_mut().lock();
                if streams.cork() {
                    if let Ok(status) = status.read() {
                        info!("Sending paused to server");
                        let msg = status.make_status_message(StatusCode::Pause);
                        slim_tx_in.send(msg).ok();
                    }
                }
                ml.borrow_mut().unlock();
            } else {
                ml.borrow_mut().lock();
                if streams.cork() {
                    std::thread::spawn(move || {
                        std::thread::sleep(interval);
                        stream_in.send(PlayerMsg::Unpause).ok();
                    });
                }
                ml.borrow_mut().unlock();
            }
        }
        ServerMessage::Unpause(interval) => {
            info!("Resume requested with interval {:?}", interval);
            if interval.is_zero() {
                ml.borrow_mut().lock();
                if streams.uncork() {
                    if let Ok(status) = status.read() {
                        info!("Sending resumed to server");
                        let msg = status.make_status_message(StatusCode::Resume);
                        slim_tx_in.send(msg).ok();
                    }
                }
                ml.borrow_mut().unlock();
            } else {
                std::thread::spawn(move || {
                    std::thread::sleep(interval);
                    stream_in.send(PlayerMsg::Unpause).ok();
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
            ..
        } => {
            info!("Start stream command from server");
            if let Some(http_headers) = http_headers {
                let num_crlf = http_headers.matches("\r\n").count();

                if num_crlf > 0 {
                    if let Ok(mut status) = status.write() {
                        status.add_crlf(num_crlf as u8);
                    }

                    let new_stream = stream::make_stream(
                        server_ip,
                        &server_default_ip,
                        server_port,
                        http_headers,
                        status.clone(),
                        slim_tx_in.clone(),
                        stream_in.clone(),
                        threshold,
                        format,
                        pcmsamplerate,
                        pcmchannels,
                        cx,
                        skip,
                        volume.clone(),
                    )?;

                    if let Some(new_stream) = new_stream {
                        pulse::connect_stream(ml.clone(), new_stream.clone(), &cli.device)?;

                        if streams.add(new_stream) {
                            if autostart == slimproto::proto::AutoStart::Auto {
                                ml.borrow_mut().lock();
                                if streams.uncork() {
                                    info!("Sending track started");
                                    if let Ok(status) = status.read() {
                                        let msg =
                                            status.make_status_message(StatusCode::TrackStarted);
                                        slim_tx_in.send(msg).ok();
                                    }
                                }
                                ml.borrow_mut().unlock();
                            }
                        }
                    }
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
    status: Arc<RwLock<StatusData>>,
    slim_tx_in: Sender<ClientMessage>,
    streams: &mut stream::StreamQueue,
    stream_in: Sender<PlayerMsg>,
    ml: Rc<RefCell<Mainloop>>,
) {
    match msg {
        PlayerMsg::EndOfDecode => {
            ml.borrow_mut().lock();
            if streams.drain(stream_in) {
                if let Ok(status) = status.read() {
                    info!("Decoder ready for new stream");
                    let msg = status.make_status_message(StatusCode::DecoderReady);
                    slim_tx_in.send(msg).ok();
                }
            }
            ml.borrow_mut().unlock();
        }
        PlayerMsg::Drained => {
            if streams.is_draining() {
                info!("End of track");
                if let Some(old_stream) = streams.shift() {
                    ml.borrow_mut().lock();
                    old_stream.borrow_mut().disconnect().ok();
                    if streams.uncork() {
                        info!("Sending track started");
                        if let Ok(status) = status.read() {
                            let msg = status.make_status_message(StatusCode::TrackStarted);
                            slim_tx_in.send(msg).ok();
                        }
                    }
                    ml.borrow_mut().unlock();
                }
            }
        }
        PlayerMsg::Pause => {
            ml.borrow_mut().lock();
            streams.cork();
            ml.borrow_mut().unlock();
        }
        PlayerMsg::Unpause => {
            ml.borrow_mut().lock();
            streams.uncork();
            ml.borrow_mut().unlock();
        }
    }
}

fn print_output_devices(ml: Rc<RefCell<Mainloop>>, cx: Rc<RefCell<Context>>) {
    println!("Output devices:");
    let count = Arc::new(AtomicCell::new(0usize));
    let count_ref = count.clone();
    ml.borrow_mut().lock();
    let op = cx.borrow().introspect().get_sink_info_list(move |list| {
        if let libpulse_binding::callbacks::ListResult::Item(item) = list {
            if let Some(name) = &item.name {
                count_ref.fetch_add(1);
                println!("{}: {}", count_ref.load(), name);
            }
        }
    });
    ml.borrow_mut().unlock();

    while op.get_state() == pa::operation::State::Running {
        std::thread::sleep(Duration::from_millis(10));
    }
    print!("Found {} device", count.load());
    if count.load() != 1 {
        print!("s");
    }
    println!();
}
