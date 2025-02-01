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
    channel::{bounded, Select},
};

use audio_out::AudioOutput;
use log::info;
use message::{process_slim_msg, process_stream_msg};
use simple_logger::SimpleLogger;
use slimproto::{
    proto::{ClientMessage, SLIM_PORT},
    status::StatusData,
};

mod audio_out;
mod decode;
mod message;
#[cfg(feature = "notify")]
mod notify;
mod proto;
#[cfg(feature = "pulse")]
mod pulse_out;
#[cfg(feature = "rodio")]
mod rodio_out;

#[derive(Parser)]
#[command(name = "Vibe", author, version, about, long_about = None)]
struct Cli {
    #[arg(
        short,
        name = "SERVER[:PORT]",
        value_parser = cli_server_parser,
        help = "Connect to the specified server, otherwise use autodiscovery")]
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

    #[cfg(all(feature = "pulse", feature = "rodio"))]
    #[arg(long, short = 'a', default_value = "pulse", value_parser = PossibleValuesParser::new([
        "pulse", "rodio" ]),
        help = "Which audio system to use"
    )]
    system: String,

    #[cfg(feature = "notify")]
    #[arg(long, short = 'q', help = "Do not use desktop notifications")]
    quiet: bool,

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
    #[cfg(feature = "pulse")]
    skip: Arc<AtomicCell<Duration>>,
    output_threshold: Duration,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    SimpleLogger::new()
        .with_colors(true)
        .with_level(cli.loglevel)
        .init()?;

    #[cfg(all(feature = "pulse", feature = "rodio"))]
    let output_system = cli.system.as_str();
    #[cfg(all(feature = "pulse", not(feature = "rodio")))]
    let output_system = "pulse";
    #[cfg(all(not(feature = "pulse"), feature = "rodio"))]
    let output_system = "rodio";
    let mut output = AudioOutput::try_new(
        output_system,
        #[cfg(feature = "rodio")]
        &cli.device,
    )?;

    // List the output devices and terminate
    if cli.list {
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

    loop {
        let name = {
            let name = match hostname::get().map(|s| s.into_string()) {
                Ok(Ok(hostname)) => cli.name.clone() + &format!("@{hostname}"),
                _ => cli.name.clone(),
            };
            Arc::new(RwLock::new(name))
        };

        // Start the slim protocol threads
        let status = Arc::new(Mutex::new(StatusData::default()));
        let start_time = Instant::now();
        let mut server_default_ip = *cli.server.unwrap_or(SocketAddrV4::new(0.into(), 0)).ip();
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
                        #[cfg(feature = "notify")]
                        &cli.quiet,
                    );
                }
                _ => {}
            }
        }
    }
}
