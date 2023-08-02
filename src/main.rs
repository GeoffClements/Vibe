use std::{
    cell::RefCell,
    io::Write,
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
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
    select,
};
use libpulse_binding as pa;
use log::{error, info, warn};
use pa::{
    context::Context,
    mainloop::threaded::Mainloop,
    sample::Spec,
    stream::Stream,
    volume::{ChannelVolumes, VolumeDB},
};
use simple_logger::SimpleLogger;
use slimproto::{
    buffer::SlimBuffer,
    proto::{ClientMessage, PcmChannels, PcmSampleRate, ServerMessage, SLIM_PORT},
    status::{StatusCode, StatusData},
};
use symphonia::core::{
    audio::RawSampleBuffer,
    codecs::DecoderOptions,
    formats::FormatOptions,
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::Hint,
};

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

enum PlayerMsg {
    EndOfDecode,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    SimpleLogger::new()
        .with_colors(true)
        .with_level(cli.loglevel)
        .init()?;

    // Create a pulse audio threaded main loop and context
    let (ml, mut cx) = pulse::setup()?;

    // List the output devices and terminate
    if cli.list {
        println!("Output devices:");
        let count = Arc::new(AtomicCell::new(0usize));
        let count_ref = count.clone();
        let op = cx.introspect().get_sink_info_list(move |list| {
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
    let status = Arc::new(RwLock::new(StatusData::default()));
    let mut server_default_ip = *cli.server.unwrap_or(SocketAddrV4::new(0.into(), 0)).ip();
    let name = Arc::new(RwLock::new(cli.name));
    let (slim_tx_in, slim_tx_out) = bounded(1);
    let (slim_rx_in, slim_rx_out) = bounded(1);
    proto::run(
        cli.server,
        name.clone(),
        slim_rx_in.clone(),
        slim_tx_out.clone(),
    );

    // let gain = AtomicCell::new(1.0f32);
    let mut volume = ChannelVolumes::default();
    volume.set_len(2);

    // let mut stream = None;
    // let mut next_stream = None;
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
                    &mut volume,
                    status.clone(),
                    stream_in.clone(),
                )?;
            }
            op if op.index() == stream_idx => {
                let msg = op.recv(&stream_out)?;
                process_stream_msg(msg);
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
    volume: &mut ChannelVolumes,
    status: Arc<RwLock<StatusData>>,
    stream_in: Sender<PlayerMsg>,
) -> anyhow::Result<()> {
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
            info!("Setting volume to ({l},{r})");
            {
                let vl = volume.get_mut();
                vl[0] = VolumeDB(20.0 * l).into();
                vl[1] = VolumeDB(20.0 * r).into();
            }
            // TODO: now set volume for the current stream
        }
        ServerMessage::Status(ts) => {
            info!("Received status tick from server with timestamp {:#?}", ts);
            if let Ok(mut status) = status.write() {
                status.set_timestamp(ts);
                let msg = status.make_status_message(StatusCode::Timer);
                info!("Sending status update");
                slim_tx_in.send(msg).ok();
            }
        }
        ServerMessage::Stream {
            http_headers,
            server_ip,
            server_port,
            threshold,
            format,
            pcmsamplerate,
            pcmchannels,
            ..
        } => {
            if let Some(http_headers) = http_headers {
                let num_crlf = http_headers.matches("\r\n").count();

                if num_crlf > 0 {
                    if let Ok(mut status) = status.write() {
                        status.add_crlf(num_crlf as u8);
                    }

                    // let new_stream = make_stream(
                    //     server_ip,
                    //     &server_default_ip,
                    //     server_port,
                    //     http_headers,
                    //     status.clone(),
                    //     slim_tx_in.clone(),
                    //     stream_in.clone(),
                    //     threshold,
                    //     format,
                    //     pcmsamplerate,
                    //     pcmchannels,
                    //     &mut cx,
                    // )?;

                    // if stream.is_some() {
                    //     next_stream = new_stream;
                    // } else {
                    //     stream = new_stream;
                    // }
                }
            }
        }
        cmd => {
            warn!("Unimplemented command: {:?}", cmd);
        }
    }

    Ok(())
}

fn process_stream_msg(msg: PlayerMsg) {
    todo!();
}

fn make_stream(
    server_ip: Ipv4Addr,
    default_ip: &Ipv4Addr,
    server_port: u16,
    http_headers: String,
    status: Arc<RwLock<StatusData>>,
    slim_tx: Sender<ClientMessage>,
    stream_in: Sender<PlayerMsg>,
    threshold: u32,
    format: slimproto::proto::Format,
    pcmsamplerate: slimproto::proto::PcmSampleRate,
    pcmchannels: slimproto::proto::PcmChannels,
    cx: &mut Context,
) -> anyhow::Result<Option<Stream>> {
    
    // The LMS sends an ip of 0, 0, 0, 0 when it wants us to default to it
    let ip = if server_ip.is_unspecified() {
        *default_ip
    } else {
        server_ip
    };

    let mut data_stream = TcpStream::connect((ip, server_port))?;
    data_stream.write(http_headers.as_bytes())?;
    data_stream.flush()?;

    if let Ok(status) = status.read() {
        let msg = status.make_status_message(StatusCode::Connect);
        slim_tx.send(msg).ok();
    }

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(SlimBuffer::with_capacity(
            threshold as usize * 1024,
            data_stream,
            status.clone(),
        ))),
        Default::default(),
    );

    // Create a hint to help the format registry guess what format reader is appropriate.
    let mut hint = Hint::new();
    hint.mime_type({
        match format {
            slimproto::proto::Format::Pcm => "audio/x-adpcm",
            slimproto::proto::Format::Mp3 => "audio/mpeg3",
            slimproto::proto::Format::Aac => "audio/aac",
            slimproto::proto::Format::Ogg => "audio/ogg",
            slimproto::proto::Format::Flac => "audio/flac",
            _ => "",
        }
    });

    let mut probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(probed) => probed,
        Err(_) => {
            if let Ok(status) = status.read() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx.send(msg).ok();
            }
            return Ok(None);
        }
    };

    let track = match probed.format.default_track() {
        Some(track) => track,
        None => {
            if let Ok(status) = status.read() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx.send(msg).ok();
            }
            return Ok(None);
        }
    };

    if let Ok(status) = status.read() {
        let msg = status.make_status_message(StatusCode::StreamEstablished);
        slim_tx.send(msg).ok();
    }

    // Work with a sample format of F32
    let sample_format = pa::sample::Format::FLOAT32NE;

    let sample_rate = match pcmsamplerate {
        PcmSampleRate::Rate(rate) => rate,
        PcmSampleRate::SelfDescribing => track.codec_params.sample_rate.unwrap_or(44100),
    };

    let channels = match pcmchannels {
        PcmChannels::Mono => 1u8,
        PcmChannels::Stereo => 2,
        PcmChannels::SelfDescribing => match track.codec_params.channel_layout {
            Some(symphonia::core::audio::Layout::Mono) => 1,
            Some(symphonia::core::audio::Layout::Stereo) => 2,
            None => match track.codec_params.channels {
                Some(channels) => channels.count() as u8,
                _ => 2,
            },
            _ => 2,
        },
    };

    // Create a spec for the pa stream
    let spec = Spec {
        format: sample_format,
        rate: sample_rate,
        channels,
    };

    // Create a pulseaudio stream
    let pa_stream = Rc::new(RefCell::new(match Stream::new(cx, "Music", &spec, None) {
        Some(stream) => stream,
        None => {
            if let Ok(status) = status.read() {
                let msg = status.make_status_message(StatusCode::NotSupported);
                slim_tx.send(msg).ok();
            }
            return Ok(None);
        }
    }));

    // if let Ok(status) = status.read() {
    //     let msg = status.make_status_message(StatusCode::TrackStarted);
    //     slim_tx.send(msg).ok();
    // }

    // Create a decoder for the track.
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut audio_buf = Vec::with_capacity(8 * 1024);

    // Add callback to pa_stream to feed music
    let status_ref = status.clone();
    let sm_ref = pa_stream.clone();
    pa_stream
        .borrow_mut()
        .set_write_callback(Some(Box::new(move |len| {
            while audio_buf.len() < len {
                let packet = match probed.format.next_packet() {
                    Ok(packet) => packet,
                    Err(symphonia::core::errors::Error::IoError(err))
                        if err.kind() == std::io::ErrorKind::UnexpectedEof
                            && err.to_string() == "end of stream" =>
                    {
                        stream_in.send(PlayerMsg::EndOfDecode).ok();
                        break;
                    }
                    Err(e) => {
                        error!("Error reading data stream: {}", e);
                        break;
                    }
                };

                let decoded = match decoder.decode(&packet) {
                    Ok(decoded) => decoded,
                    Err(_) => break,
                };

                if decoded.frames() == 0 {
                    break;
                }

                let mut raw_buf =
                    RawSampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());

                raw_buf.copy_interleaved_ref(decoded);
                audio_buf.extend_from_slice(raw_buf.as_bytes());
            }

            // Check for end of decoded data
            if audio_buf.len() < len {
                let sm_ref = sm_ref.clone();
                let status = status.clone();
                let slim_tx = slim_tx.clone();
                unsafe {
                    (*sm_ref.as_ptr()).drain(Some(Box::new(move |success| {
                        if success {
                            (*sm_ref.as_ptr()).disconnect().ok();
                            if let Ok(status) = status.read() {
                                let msg = status.make_status_message(StatusCode::DecoderReady);
                                slim_tx.send(msg).ok();
                            }
                        }
                    })));
                }
                return;
            }

            unsafe {
                (*sm_ref.as_ptr())
                    .write_copy(
                        &audio_buf.drain(..len).collect::<Vec<u8>>(),
                        0,
                        pa::stream::SeekMode::Relative,
                    )
                    .ok()
            };

            if let Ok(Some(stream_time)) = unsafe { (*sm_ref.as_ptr()).get_time() } {
                if let Ok(mut status) = status_ref.write() {
                    status.set_elapsed_milli_seconds(stream_time.as_millis() as u32);
                    status.set_elapsed_seconds(stream_time.as_secs() as u32);
                    status.set_output_buffer_size(audio_buf.capacity() as u32);
                    status.set_output_buffer_fullness(audio_buf.len() as u32);
                };
            }
        })));

    let pa_stream = Rc::into_inner(pa_stream).unwrap().into_inner();
    // pulse::connect_stream(&ml, &pa_stream, autostart).ok();

    Ok(Some(pa_stream))
}
