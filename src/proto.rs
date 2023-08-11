use std::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, RwLock},
};

use crossbeam::channel::{Receiver, Sender};
use log::{error, info};
use slimproto::{
    self, discovery::discover, proto::Server, Capabilities, Capability, ClientMessage, FramedReader,
    FramedWriter, ServerMessage,
};

pub fn run(
    server_addr: Option<SocketAddrV4>,
    name: Arc<RwLock<String>>,
    slim_rx_in: Sender<ServerMessage>,
    slim_tx_out: Receiver<ClientMessage>,
) {
    std::thread::spawn(move || {
        let mut server = match server_addr {
            Some(sock) => Server::from(sock),
            None => match discover(None) {
                Ok(Some(server)) => server,
                _ => unreachable!(),
            },
        };

        slim_rx_in
            .send(ServerMessage::Serv {
                ip_address: Ipv4Addr::from(*server.socket.ip()),
                sync_group_id: None,
            })
            .ok();

        let mut syncgroupid = String::new();
        // Outer loop to reconnect to a different server and
        // update server details when a Serv message is received
        loop {
            let mut caps = Capabilities::default();
            if let Ok(name) = name.read() {
                caps.add_name(&name);
            }
            caps.add(Capability::Maxsamplerate(192000));
            if syncgroupid.len() > 0 {
                info!("Joining sync group: {syncgroupid}");
                caps.add(Capability::Syncgroupid(syncgroupid.to_owned()));
            }
            caps.add(Capability::Pcm);
            caps.add(Capability::Mp3);
            caps.add(Capability::Aac);
            caps.add(Capability::Alc);
            caps.add(Capability::Ogg);
            caps.add(Capability::Flc);

            // Connect to the server
            let (mut rx, mut tx) = match server.clone().prepare(caps).connect() {
                Ok((rx, tx)) => (rx, tx),
                Err(_) => {
                    error!("Error connecting to server");
                    return;
                }
            };

            // Start write thread
            // Continues until connection is dropped
            let slim_tx_out_r = slim_tx_out.clone();
            std::thread::spawn(move || {
                while let Ok(msg) = slim_tx_out_r.recv() {
                    // println!("{:?}", msg);
                    if tx.framed_write(msg).is_err() {
                        return;
                    }
                }
            });

            // Inner read loop
            while let Ok(msg) = rx.framed_read() {
                // println!("{:?}", msg);
                match msg {
                    // Request to change to another server
                    ServerMessage::Serv {
                        ip_address: ip,
                        sync_group_id: sgid,
                    } => {
                        if let Some(ref sgid) = sgid {
                            syncgroupid = sgid.to_owned();
                        }

                        server = (ip, sgid).into();
                        // Now inform the main thread
                        slim_rx_in
                            .send(ServerMessage::Serv {
                                ip_address: ip,
                                sync_group_id: None,
                            })
                            .ok();
                        break;
                    }
                    _ => {
                        slim_rx_in.send(msg).ok();
                    }
                }
            }
        }
    });
}
