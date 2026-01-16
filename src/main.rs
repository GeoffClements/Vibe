use std::{
    net::{Ipv4Addr, SocketAddrV4, ToSocketAddrs},
    sync::{Arc, LazyLock, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::Context;
use cfg_if::cfg_if;
use clap::{
    builder::{PossibleValuesParser, TypedValueParser},
    Parser,
};
use crossbeam::{
    atomic::AtomicCell,
    channel::{bounded, Select},
};

use log::info;
use message::{process_slim_msg, process_stream_msg};
use simple_logger::SimpleLogger;
use slimproto::{
    proto::{ClientMessage, SLIM_PORT},
    status::{StatusCode, StatusData},
};

mod audio_out;
mod decode;
mod message;
#[cfg(feature = "notify")]
mod notify;
#[cfg(feature = "pipewire")]
mod pipewire_out;
mod proto;
#[cfg(feature = "pulse")]
mod pulse_out;
#[cfg(feature = "rodio")]
mod rodio_out;
mod startup;

#[derive(Parser)]
#[command(name = "Vibe", author, version, about, long_about = None)]
struct Cli {
    #[arg(
        short,
        long,
        name = "SERVER[:PORT]",
        // value_parser = cli_server_parser,
        help = "Connect to the specified server, otherwise use autodiscovery")]
    server: Option<String>,

    #[arg(
        short = 'o',
        long,
        name = "OUTPUT_DEVICE",
        help = "Output device [default: System default device]"
    )]
    device: Option<String>,

    #[arg(short, long, help = "List output devices")]
    list: bool,

    #[arg(short, long, default_value = "Vibe", help = "Set the player name")]
    name: String,

    #[cfg(any(
        all(feature = "pulse", feature = "rodio"),
        all(feature = "pulse", feature = "pipewire"),
        all(feature = "rodio", feature = "pipewire")
    ))]
    #[arg(long, short = 'a', default_value_t = cli_default_system(), value_parser = cli_system_list(),
        help = "Which audio system to use"
    )]
    system: String,

    #[cfg(feature = "notify")]
    #[arg(long, short = 'q', help = "Do not use desktop notifications")]
    quiet: bool,

    #[arg(long, help = "Create a systemd user service file")]
    create_service: bool,

    #[arg(long,
        default_value = "off",
        value_parser = PossibleValuesParser::new(["trace", "debug", "error", "warn", "info", "off"])
            .map(|s| s.parse::<log::LevelFilter>().unwrap()),
        help = "Set highest log level")]
    loglevel: log::LevelFilter,
}

fn cli_server_parser(value: &str) -> anyhow::Result<SocketAddrV4> {
    // Try parsing as SocketAddrV4 directly (ip:port or host:port)
    if let Ok(addr) = value.parse::<SocketAddrV4>() {
        return Ok(addr);
    }

    // Try parsing as Ipv4Addr (ip only, no port)
    if let Ok(ip) = value.parse::<Ipv4Addr>() {
        return Ok(SocketAddrV4::new(ip, SLIM_PORT));
    }

    // Try parsing as host[:port]
    let mut parts = value.rsplitn(2, ':');
    let last = parts.next();
    let first = parts.next();

    let (host, port) = match (first, last) {
        (Some(host), Some(port_str)) if port_str.chars().all(|c| c.is_ascii_digit()) => {
            let port = port_str.parse::<u16>().unwrap_or(SLIM_PORT);
            (host, port)
        }
        (Some(_), Some(_)) => (value, SLIM_PORT),
        (None, Some(host)) => (host, SLIM_PORT),
        _ => (value, SLIM_PORT),
    };

    // Use ToSocketAddrs to resolve host
    let addrs = (host, port).to_socket_addrs()?;
    for addr in addrs {
        if let std::net::SocketAddr::V4(addr_v4) = addr {
            return Ok(addr_v4);
        }
    }

    Err(anyhow::anyhow!("Could not resolve server address"))
}

fn cli_default_system() -> String {
    cfg_if! {
        if #[cfg(feature = "pulse")] {
            "pulse".to_string()
        } else if #[cfg(feature = "pipewire")] {
            "pipewire".to_string()
        } else {
            "rodio".to_string()
        }
    }
}

#[allow(unused)]
fn cli_system_list() -> PossibleValuesParser {
    cfg_if! {
        if #[cfg(all(feature = "pulse", feature = "pipewire", feature = "rodio"))] {
            PossibleValuesParser::new(["pulse", "pipewire", "rodio"])
        } else if #[cfg(all(feature = "pulse", feature = "pipewire"))] {
            PossibleValuesParser::new(["pulse", "pipewire"])
        } else if #[cfg(all(feature = "pulse", feature = "rodio"))] {
            PossibleValuesParser::new(["pulse", "rodio"])
        } else if #[cfg(all(feature = "rodio", feature = "pipewire"))] {
            PossibleValuesParser::new(["pipewire", "rodio"])
        } else {
            PossibleValuesParser::new([""])
        }
    }
}

// Controls from/to the LMS
pub static VOLUME: LazyLock<Mutex<Vec<f32>>> = LazyLock::new(|| Mutex::new(vec![1.0, 1.0]));
pub static SKIP: LazyLock<AtomicCell<Duration>> = LazyLock::new(|| AtomicCell::new(Duration::ZERO));
pub static STATUS: LazyLock<Arc<Mutex<StatusData>>> =
    LazyLock::new(|| Arc::new(Mutex::new(StatusData::default())));

pub struct StreamParams {
    autostart: slimproto::proto::AutoStart,
    output_threshold: Duration,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    SimpleLogger::new()
        .with_colors(true)
        .with_level(cli.loglevel)
        .init()?;

    // Create a systemd unit file if requested
    if cli.create_service {
        if let Some(ref server) = cli.server {
            _ = cli_server_parser(server).context(format!("Server not found: {}", server))?
        }

        startup::create_systemd_unit(&cli.server, &cli.device)?;

        return Ok(());
    }

    let output_system = {
        cfg_if! {
            if #[cfg(any(
                all(feature = "pulse", feature = "rodio"),
                all(feature = "pulse", feature = "pipewire"),
                all(feature = "rodio", feature = "pipewire")
            ))] {
                cli.system
            } else {
                cli_default_system()
            }
        }
    };
    let mut output = None;

    // List the output devices and terminate
    if cli.list {
        if let Ok(output) = audio_out::make_audio_output(
            &output_system,
            #[cfg(feature = "rodio")]
            &cli.device,
        ) {
            println!("Output devices:");
            let names = output.get_output_device_names()?;
            names
                .iter()
                .enumerate()
                .for_each(|(i, (name, description))| {
                    println!("{}: {}", i, name);
                    if let Some(desc) = description {
                        println!("   {}", desc);
                    }
                });
            print!("Found {} device", names.len());
            if names.len() != 1 {
                print!("s");
            }
            println!();
            return Ok(());
        }
    }

    // If a server was specified, parse it now
    let cli_server = if let Some(ref server) = cli.server {
        Some(cli_server_parser(server).context(format!("Server not found: {}", server))?)
    } else {
        None
    };

    // Main loop - if we lose the server connection, we restart everything
    loop {
        let name = {
            let name = match hostname::get().map(|s| s.into_string()) {
                Ok(Ok(hostname)) => cli.name.clone() + &format!("@{hostname}"),
                _ => cli.name.clone(),
            };
            Arc::new(RwLock::new(name))
        };

        // Start the slim protocol threads
        // let status = Arc::new(Mutex::new(StatusData::default()));
        let start_time = Instant::now();
        let mut server_default_ip = *cli_server.unwrap_or(SocketAddrV4::new(0.into(), 0)).ip();
        let (slim_tx_in, slim_tx_out) = bounded(1);
        let (slim_rx_in, slim_rx_out) = bounded(1);
        proto::run(cli_server, slim_rx_in.clone(), slim_tx_out.clone());

        // let volume = Arc::new(Mutex::new(vec![1.0f32, 1.0]));
        let (stream_in, stream_out) = bounded(10);
        let mut select = Select::new();
        let slim_idx = select.recv(&slim_rx_out);
        let stream_idx = select.recv(&stream_out);

        loop {
            let timeout = if output.is_some() {
                Duration::from_secs(1)
            } else {
                Duration::from_secs(5)
            };

            match select.select_timeout(timeout) {
                Ok(op) if op.index() == slim_idx => match op.recv(&slim_rx_out)? {
                    Some(msg) => process_slim_msg(
                        &mut output,
                        msg,
                        &mut server_default_ip,
                        name.clone(),
                        slim_tx_in.clone(),
                        stream_in.clone(),
                        &start_time,
                        &output_system,
                        #[cfg(feature = "rodio")]
                        &cli.device,
                    )?,

                    None => {
                        info!("Lost contact with server, resetting");
                        slim_tx_in.send(ClientMessage::Bye(1)).ok();
                        if let Some(ref mut output) = output {
                            output.stop();
                        }
                        break;
                    }
                },

                Ok(op) if op.index() == stream_idx => {
                    let msg = op.recv(&stream_out)?;
                    process_stream_msg(
                        msg,
                        slim_tx_in.clone(),
                        &mut output,
                        stream_in.clone(),
                        &cli.device,
                        #[cfg(feature = "notify")]
                        &cli.quiet,
                    );
                }

                Ok(_) => {}

                Err(_) => {
                    let play_time = match output {
                        Some(ref output) => output.get_dur(),
                        None => Duration::ZERO,
                    };

                    if let Ok(mut status) = STATUS.lock() {
                        // info!("Sending status update - jiffies: {:?}", status.get_jiffies());
                        status.set_elapsed_milli_seconds(play_time.as_millis() as u32);
                        status.set_elapsed_seconds(play_time.as_secs() as u32);
                        // status.set_timestamp(ts);

                        let msg = status.make_status_message(StatusCode::Timer);
                        slim_tx_in.send(msg).ok();
                    }
                }
            }
        }
    }
}
