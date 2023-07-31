use std::{
    net::{Ipv4Addr, SocketAddrV4},
    str::FromStr,
};

use anyhow;
use clap::Parser;
use slimproto::proto::SLIM_PORT;

mod proto;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(short, name = "SERVER[:PORT]", value_parser = cli_server_parser, help = "Connect to the specified server, otherwise use autodiscovery")]
    server: Option<SocketAddrV4>,

    #[arg(short, default_value = "vibe", help = "Set the player name")]
    name: String,
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

    proto::run(cli.server);

    Ok(())
}
