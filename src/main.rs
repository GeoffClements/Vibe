use std::{
    net::{Ipv4Addr, SocketAddrV4},
    str::FromStr,
    sync::{atomic::AtomicUsize, Arc, RwLock},
    time::Duration,
};

use anyhow;
use clap::{
    builder::{PossibleValuesParser, TypedValueParser},
    Parser,
};
use crossbeam::{atomic::AtomicCell, channel::bounded, select};
use log::{error, info, warn};
use simple_logger::SimpleLogger;
use slimproto::proto::{ClientMessage, ServerMessage, SLIM_PORT};

mod proto;
mod pulse;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(short, name = "SERVER[:PORT]", value_parser = cli_server_parser, help = "Connect to the specified server, otherwise use autodiscovery")]
    server: Option<SocketAddrV4>,

    #[arg(short, help = "List output devices")]
    list: bool,

    #[arg(short, default_value = "vibe", help = "Set the player name")]
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    SimpleLogger::new()
        .with_colors(true)
        .with_level(cli.loglevel)
        .init()?;

    // Create a pulse audio threaded main loop and context
    let (ml, cx) = pulse::setup()?;

    if cli.list {
        println!("Output devices:");
        let count = Arc::new(AtomicCell::new(0usize));
        let count_ref = count.clone();
        let op = cx.borrow().introspect().get_sink_info_list(move |list| {
            if let libpulse_binding::callbacks::ListResult::Item(item) = list {
                if let Some(name) = &item.name {
                    count_ref.fetch_add(1);
                    println!("{}: {}", count_ref.load(), name);
                }
            }
        });

        loop {
            std::thread::sleep(Duration::from_millis(10));
            if op.get_state() != libpulse_binding::operation::State::Running {
                println!("Found {} devices", count.load());
                break;
            }
        }
        return Ok(());
    }

    // Start the slim protocol threads
    let mut server_ip = *cli.server.unwrap_or(SocketAddrV4::new(0.into(), 0)).ip();
    let name = Arc::new(RwLock::new(cli.name));
    let (slim_tx_in, slim_tx_out) = bounded(1);
    let (slim_rx_in, slim_rx_out) = bounded(1);
    proto::run(
        cli.server,
        name.clone(),
        slim_rx_in.clone(),
        slim_tx_out.clone(),
    );

    let gain = AtomicCell::new(1.0f32);
    loop {
        select! {
            recv(slim_rx_out) -> msg => {
                match msg {
                    Ok(msg) => match msg {
                        ServerMessage::Serv { ip_address, .. } => {
                            info!("Switching to server at {ip_address}");
                            server_ip = ip_address;
                        },
                        ServerMessage::Queryname => {
                            log::info!("Name query from server");
                            if let Ok(name) = name.read() {
                                info!("Sending name: {name}");
                                slim_tx_in.send(ClientMessage::Name(name.to_owned())).ok();
                            }
                        },
                        ServerMessage::Setname(new_name) => {
                            if let Ok(mut name) = name.write() {
                                info!("Set name to {new_name}");
                                *name = new_name;
                            }
                        },
                        ServerMessage::Gain(l, r) => {
                            let ave_g = ((l + r) / 2.0).sqrt() as f32;
                            info!("Setting gain to {ave_g}");
                            gain.store(ave_g);
                        },
                        cmd @ _ => {
                            warn!("Unimplemented command: {:?}", cmd);
                        },
                    }
                    Err(e) => {
                        error!("Error: {}", e);
                    },
                }
            }
        }
    }
}
