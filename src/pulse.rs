use std::{
    borrow::BorrowMut,
    cell::RefCell,
    io::Write,
    net::{Ipv4Addr, TcpStream},
    ops::Deref,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::anyhow;

use crossbeam::{
    atomic::AtomicCell,
    channel::{bounded, Sender},
};
use libpulse_binding::{
    self as pa,
    callbacks::ListResult,
    context::{Context, FlagSet as CxFlagSet},
    error::PAErr,
    mainloop::threaded::Mainloop,
    sample::Spec,
    stream::{FlagSet as SmFlagSet, Stream},
};
use log::{error, info};
use slimproto::{
    status::{StatusCode, StatusData},
    ClientMessage,
};

use crate::{
    decode::{Decoder, DecoderError},
    PlayerMsg,
};

pub struct AudioOutput {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<Context>>,
    playing: Option<Rc<RefCell<Stream>>>,
    next_up: Option<Rc<RefCell<Stream>>>,
}

impl AudioOutput {
    pub fn try_new() -> Result<Self, PAErr> {
        let ml = Rc::new(RefCell::new(
            Mainloop::new().ok_or(pa::error::Code::ConnectionRefused)?,
        ));

        let cx = Rc::new(RefCell::new(
            Context::new((*ml).borrow_mut().deref(), "Vibe")
                .ok_or(pa::error::Code::ConnectionRefused)?,
        ));

        // Context state change callback
        {
            let ml_ref = ml.clone();
            let cx_ref = cx.clone();
            (*cx)
                .borrow_mut()
                .set_state_callback(Some(Box::new(move || {
                    let state = unsafe { (*cx_ref.as_ptr()).get_state() };
                    match state {
                        pa::context::State::Ready
                        | pa::context::State::Terminated
                        | pa::context::State::Failed => unsafe {
                            (*ml_ref.as_ptr()).signal(false);
                        },
                        _ => {}
                    }
                })))
        }

        (*cx).borrow_mut().connect(None, CxFlagSet::NOFLAGS, None)?;
        (*ml).borrow_mut().lock();
        (*ml).borrow_mut().start()?;

        // Wait for context to be ready
        loop {
            match cx.borrow().get_state() {
                pa::context::State::Ready => {
                    break;
                }
                pa::context::State::Failed | pa::context::State::Terminated => {
                    (*ml).borrow_mut().unlock();
                    (*ml).borrow_mut().stop();
                    return Err(pa::error::PAErr(
                        pa::error::Code::ConnectionTerminated as i32,
                    ));
                }
                _ => (*ml).borrow_mut().wait(),
            }
        }

        (*cx).borrow_mut().set_state_callback(None);
        (*ml).borrow_mut().unlock();

        Ok(AudioOutput {
            mainloop: ml,
            context: cx,
            playing: None,
            next_up: None,
        })
    }

    pub fn make_stream(
        &mut self,
        server_ip: Ipv4Addr,
        default_ip: &Ipv4Addr,
        server_port: u16,
        http_headers: String,
        status: Arc<Mutex<StatusData>>,
        slim_tx: Sender<ClientMessage>,
        stream_in: Sender<PlayerMsg>,
        threshold: u32,
        format: slimproto::proto::Format,
        pcmsamplerate: slimproto::proto::PcmSampleRate,
        pcmchannels: slimproto::proto::PcmChannels,
        skip: Arc<AtomicCell<Duration>>,
        volume: Arc<Mutex<Vec<f32>>>,
        output_threshold: Duration,
    ) -> anyhow::Result<()> {
        // The LMS sends an ip of 0, 0, 0, 0 when it wants us to default to it
        let ip = if server_ip.is_unspecified() {
            *default_ip
        } else {
            server_ip
        };

        let mut data_stream = TcpStream::connect((ip, server_port))?;
        data_stream.write(http_headers.as_bytes())?;
        data_stream.flush()?;

        if let Ok(mut status) = status.lock() {
            info!("Sending stream connected");
            let msg = status.make_status_message(StatusCode::Connect);
            slim_tx.send(msg).ok();
        }

        if let Some(mut decoder) = Decoder::try_new(
            data_stream,
            status.clone(),
            slim_tx.clone(),
            threshold,
            format,
            pcmsamplerate,
            pcmchannels,
        )? {
            // Work with a sample format of F32
            // TODO: use data native format
            let sample_format = pa::sample::Format::FLOAT32NE;

            // Create a spec for the pa stream
            let spec = Spec {
                format: sample_format,
                rate: decoder.sample_rate(),
                channels: decoder.channels(),
            };

            // Create a pulseaudio stream
            let cx = self.context.clone();
            let pa_stream = Rc::new(RefCell::new(
                match Stream::new(&mut (*cx).borrow_mut(), "Music", &spec, None) {
                    Some(stream) => stream,
                    None => {
                        if let Ok(mut status) = status.lock() {
                            let msg = status.make_status_message(StatusCode::NotSupported);
                            slim_tx.send(msg).ok();
                        }
                        return Err(anyhow!("Failed to make stream"));
                    }
                },
            ));

            // Create an audio buffer to hold raw u8 samples
            let threshold_samples = (output_threshold.as_millis()
                * decoder.channels() as u128
                * decoder.sample_rate() as u128
                / 1000)
                * 4;
            let mut audio_buf = Vec::with_capacity(threshold_samples as usize);

            // Prefill audio buffer to threshold
            match decoder.fill_buf(&mut audio_buf, threshold_samples as usize, volume.clone()) {
                Ok(()) => {}
                Err(DecoderError::EndOfDecode) => {
                    stream_in.send(PlayerMsg::EndOfDecode).ok();
                }
                Err(DecoderError::StreamError(e)) => {
                    error!("Error reading data stream: {}", e);
                    if let Ok(mut status) = status.lock() {
                        let msg = status.make_status_message(StatusCode::NotSupported);
                        slim_tx.send(msg).ok();
                    }
                    return Err(e.into());
                }
            }

            // Add callback to pa_stream to feed music
            let status_ref = status.clone();
            let sm_ref = Rc::downgrade(&pa_stream);
            (*pa_stream)
                .borrow_mut()
                .set_write_callback(Some(Box::new(move |len| {
                    match decoder.fill_buf(&mut audio_buf, len, volume.clone()) {
                        Ok(()) => {}
                        Err(DecoderError::EndOfDecode) => {
                            stream_in.send(PlayerMsg::EndOfDecode).ok();
                        }
                        Err(DecoderError::StreamError(e)) => {
                            error!("Error reading data stream: {}", e);
                            if let Ok(mut status) = status.lock() {
                                let msg = status.make_status_message(StatusCode::NotSupported);
                                slim_tx.send(msg).ok();
                            }
                            return;
                        }
                    }

                    let buf_len = if audio_buf.len() < len {
                        audio_buf.len()
                    } else {
                        len
                    };

                    let offset = match skip.take() {
                        dur if dur.is_zero() => 0i64,
                        dur => {
                            let samples = dur.as_millis() as f64 * spec.rate as f64 / 1000.0;
                            samples.round() as i64 * spec.channels as i64 * 4
                        }
                    };

                    if let Some(sm) = sm_ref.upgrade() {
                        unsafe {
                            (*sm.as_ptr())
                                .write_copy(
                                    &audio_buf.drain(..buf_len).collect::<Vec<u8>>(),
                                    offset,
                                    pa::stream::SeekMode::Relative,
                                )
                                .ok();
                            (*sm.as_ptr()).update_timing_info(None);

                            if let Ok(Some(stream_time)) = (*sm.as_ptr()).get_time() {
                                if let Ok(mut status) = status_ref.lock() {
                                    status
                                        .set_elapsed_milli_seconds(stream_time.as_millis() as u32);
                                    status.set_elapsed_seconds(stream_time.as_secs() as u32);
                                    status.set_output_buffer_size(audio_buf.capacity() as u32);
                                    status.set_output_buffer_fullness(audio_buf.len() as u32);
                                };
                            }
                        };
                    }
                })));
            self.next_up = Some(pa_stream);
            return Ok(());
        }
        Err(anyhow::Error::new(DecoderError::StreamError(
            symphonia::core::errors::Error::DecodeError("Unable to decode"),
        )))
    }

    fn connect_stream(&mut self, device: &Option<String>) -> Result<(), PAErr> {
        (*self.mainloop).borrow_mut().lock();

        // Stream state change callback
        {
            let ml_ref = self.mainloop.clone();
            let sm_ref = sm.clone();
            sm.borrow_mut().set_state_callback(Some(Box::new(move || {
                let state = unsafe { (*sm_ref.as_ptr()).get_state() };
                match state {
                    pa::stream::State::Ready
                    | pa::stream::State::Failed
                    | pa::stream::State::Terminated => unsafe {
                        (*ml_ref.as_ptr()).signal(false);
                    },
                    _ => {}
                }
            })));
        }

        let flags = SmFlagSet::START_CORKED;

        sm.borrow_mut()
            .connect_playback(device.as_deref(), None, flags, None, None)?;

        // Wait for stream to be ready
        loop {
            match sm.borrow_mut().get_state() {
                pa::stream::State::Ready => {
                    break;
                }
                pa::stream::State::Failed | pa::stream::State::Terminated => {
                    ml.borrow_mut().unlock();
                    ml.borrow_mut().stop();
                    return Err(pa::error::PAErr(
                        pa::error::Code::ConnectionTerminated as i32,
                    ));
                }
                _ => {
                    ml.borrow_mut().wait();
                }
            }
        }

        sm.borrow_mut().set_state_callback(None);
        ml.borrow_mut().unlock();

        Ok(())
    }
    pub fn play_next(&mut self) {
        self.playing = self.next_up.take();
        if let Some(stream) = self.playing {}
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        (*self.context).borrow_mut().disconnect();
    }
}

pub fn get_output_device_names() -> anyhow::Result<Vec<String>> {
    let mut output = AudioOutput::try_new()?;
    let mut ret = Vec::new();
    let (s, r) = bounded(1);

    (*output.mainloop).borrow_mut().lock();
    let _op = (*output.context)
        .borrow_mut()
        .introspect()
        .get_sink_info_list(move |listresult| match listresult {
            ListResult::Item(item) => {
                s.send(item.name.as_ref().map(|n| n.to_string())).ok();
            }
            ListResult::End | ListResult::Error => {
                s.send(None).ok();
            }
        });
    (*output.mainloop).borrow_mut().unlock();

    while let Some(name) = r.recv()? {
        ret.push(name);
    }

    Ok(ret)
}
// pub fn setup() -> Result<(Rc<RefCell<Mainloop>>, Rc<RefCell<Context>>), PAErr> {
//     let ml = Rc::new(RefCell::new(
//         Mainloop::new().ok_or(pa::error::Code::ConnectionRefused)?,
//     ));

//     let cx = Rc::new(RefCell::new(
//         Context::new(ml.borrow_mut().deref(), "Vibe").ok_or(pa::error::Code::ConnectionRefused)?,
//     ));

//     // Context state change callback
//     {
//         let ml_ref = ml.clone();
//         let cx_ref = cx.clone();
//         cx.borrow_mut().set_state_callback(Some(Box::new(move || {
//             let state = unsafe { (*cx_ref.as_ptr()).get_state() };
//             match state {
//                 pa::context::State::Ready
//                 | pa::context::State::Terminated
//                 | pa::context::State::Failed => unsafe {
//                     (*ml_ref.as_ptr()).signal(false);
//                 },
//                 _ => {}
//             }
//         })))
//     }

//     cx.borrow_mut().connect(None, CxFlagSet::NOFLAGS, None)?;
//     ml.borrow_mut().lock();
//     ml.borrow_mut().start()?;

//     // Wait for context to be ready
//     loop {
//         match cx.borrow().get_state() {
//             pa::context::State::Ready => {
//                 break;
//             }
//             pa::context::State::Failed | pa::context::State::Terminated => {
//                 ml.borrow_mut().unlock();
//                 ml.borrow_mut().stop();
//                 return Err(pa::error::PAErr(
//                     pa::error::Code::ConnectionTerminated as i32,
//                 ));
//             }
//             _ => ml.borrow_mut().wait(),
//         }
//     }

//     cx.borrow_mut().set_state_callback(None);
//     ml.borrow_mut().unlock();

//     Ok((ml, cx))
// }
