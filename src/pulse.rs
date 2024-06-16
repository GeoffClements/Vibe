use std::{
    cell::RefCell,
    ops::Deref,
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::anyhow;
use crossbeam::channel::{bounded, Sender};
use libpulse_binding::{
    callbacks::ListResult,
    context::{Context, FlagSet as CxFlagSet, State},
    error::PAErr,
    mainloop::threaded::Mainloop,
    operation::Operation,
    sample::Spec,
    stream::SeekMode,
    time::MicroSeconds,
};
use log::warn;
use slimproto::status::StatusData;

use crate::{
    decode::{AudioFormat, Decoder, DecoderError},
    PlayerMsg, StreamParams,
};

// type Callback = Box<dyn FnMut() + Send + Sync + 'static>;

pub struct Stream {
    inner: Rc<RefCell<libpulse_binding::stream::Stream>>,
}

impl Stream {
    fn new(cx: Rc<RefCell<Context>>, decoder: &Decoder) -> Option<Self> {
        let spec = Spec {
            format: match decoder.format() {
                AudioFormat::I16 | AudioFormat::U16 => libpulse_binding::sample::Format::S16NE,
                AudioFormat::I32 | AudioFormat::U32 => libpulse_binding::sample::Format::S32NE,
                AudioFormat::F32 => libpulse_binding::sample::Format::FLOAT32NE,
            },
            rate: decoder.sample_rate(),
            channels: decoder.channels(),
        };

        // Create a pulseaudio stream
        let stream = match libpulse_binding::stream::Stream::new(
            &mut (*cx).borrow_mut(),
            "Music",
            &spec,
            None,
        ) {
            Some(stream) => stream,
            None => {
                return None;
            }
        };

        Some(Self {
            inner: Rc::new(RefCell::new(stream)),
        })
    }

    fn set_write_callback(&mut self, callback: Box<dyn FnMut(usize) + 'static>) {
        (*self.inner)
            .borrow_mut()
            .set_write_callback(Some(callback));
    }

    fn write_copy(&mut self, data: &[u8], offset: i64, seek: SeekMode) -> Result<(), PAErr> {
        (*self.inner).borrow_mut().write_copy(data, offset, seek)
    }

    fn update_timing_info(
        &mut self,
        callback: Option<Box<dyn FnMut(bool) + 'static>>,
    ) -> Operation<dyn FnMut(bool)> {
        (*self.inner).borrow_mut().update_timing_info(callback)
    }

    fn get_time(&self) -> Result<Option<MicroSeconds>, PAErr> {
        (*self.inner).borrow_mut().get_time()
    }

    pub fn disconnect(&mut self) -> Result<(), PAErr> {
        (*self.inner).borrow_mut().disconnect()
    }

    fn play(&mut self) {
        (*self.inner).borrow_mut().uncork(None);
    }

    fn pause(&mut self) {
        (*self.inner).borrow_mut().cork(None);
    }

    fn unpause(&mut self) {
        self.play();
    }
}

pub struct AudioOutput {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<Context>>,
    playing: Option<Rc<RefCell<Stream>>>,
    next_up: Option<Rc<RefCell<Stream>>>,
}

impl AudioOutput {
    pub fn try_new() -> anyhow::Result<Self> {
        let ml = Rc::new(RefCell::new(
            Mainloop::new().ok_or(libpulse_binding::error::Code::ConnectionRefused)?,
        ));

        let cx = Rc::new(RefCell::new(
            Context::new((*ml).borrow_mut().deref(), "Vibe")
                .ok_or(libpulse_binding::error::Code::ConnectionRefused)?,
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
                        State::Ready | State::Terminated | State::Failed => unsafe {
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
                State::Ready => {
                    break;
                }
                State::Failed | State::Terminated => {
                    (*ml).borrow_mut().unlock();
                    (*ml).borrow_mut().stop();
                    return Err(anyhow!("Unable to connect with pulseaudio"));
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
        mut decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        status: Arc<Mutex<StatusData>>,
        stream_params: StreamParams,
    ) -> Option<(Rc<RefCell<Stream>>, slimproto::proto::AutoStart)> {
        // Create an audio buffer to hold raw u8 samples
        let threshold_samples = (stream_params.output_threshold.as_millis()
            * decoder.channels() as u128
            * decoder.sample_rate() as u128
            / 1000)
            * decoder.format().size_of() as u128;
        let mut audio_buf = Vec::with_capacity(threshold_samples as usize);

        // Prefill audio buffer to threshold
        match decoder.fill_buf(&mut audio_buf, 1024 as usize, stream_params.volume.clone()) {
            Ok(()) => {}
            Err(DecoderError::EndOfDecode) => {
                stream_in.send(PlayerMsg::EndOfDecode).ok();
            }
            Err(DecoderError::StreamError(e)) => {
                warn!("Error reading data stream: {}", e);
                stream_in.send(PlayerMsg::NotSupported).ok();
                return None;
            }
        }

        (*self.mainloop).borrow_mut().lock();
        let stream = Rc::new(RefCell::new(
            match Stream::new(self.context.clone(), &decoder) {
                Some(stream) => stream,
                None => {
                    stream_in.send(PlayerMsg::NotSupported).ok();
                    return None;
                }
            },
        ));
        (*self.mainloop).borrow_mut().unlock();

        // Add callback to pa_stream to feed music
        let stream_in_r = stream_in.clone();
        let mut draining = false;
        let mut drained = false;
        let sm_ref = Rc::downgrade(&stream);
        (*self.mainloop).borrow_mut().lock();
        (*stream)
            .borrow_mut()
            .set_write_callback(Box::new(move |len| {
                if !drained {
                    match decoder.fill_buf(
                        &mut audio_buf,
                        len as usize,
                        stream_params.volume.clone(),
                    ) {
                        Ok(()) => {}
                        Err(DecoderError::EndOfDecode) => {
                            if !draining {
                                stream_in_r.send(PlayerMsg::EndOfDecode).ok();
                                draining = true;
                            }
                        }
                        Err(DecoderError::StreamError(e)) => {
                            warn!("Error reading data stream: {}", e);
                            stream_in_r.send(PlayerMsg::NotSupported).ok();
                            return;
                        }
                    }

                    if audio_buf.len() > 0 {
                        let buf_len = if audio_buf.len() < len {
                            audio_buf.len()
                        } else {
                            len
                        };

                        let offset = match stream_params.skip.take() {
                            dur if dur.is_zero() => 0i64,
                            dur => {
                                let samples =
                                    dur.as_millis() as f64 * decoder.sample_rate() as f64 / 1000.0;
                                samples.round() as i64 * decoder.channels() as i64 * 4
                            }
                        };

                        if let Some(sm) = sm_ref.upgrade() {
                            unsafe {
                                (*sm.as_ptr())
                                    .write_copy(
                                        &audio_buf.drain(..buf_len).collect::<Vec<u8>>(),
                                        offset,
                                        SeekMode::Relative,
                                    )
                                    .ok();
                                (*sm.as_ptr()).update_timing_info(None);

                                if let Ok(Some(stream_time)) = (*sm.as_ptr()).get_time() {
                                    if let Ok(mut status) = status.lock() {
                                        status.set_elapsed_milli_seconds(
                                            stream_time.as_millis() as u32
                                        );
                                        status.set_elapsed_seconds(stream_time.as_secs() as u32);
                                        status.set_output_buffer_size(audio_buf.capacity() as u32);
                                        status.set_output_buffer_fullness(audio_buf.len() as u32);
                                    };
                                }
                            };
                        }
                    } else if draining {
                        stream_in_r.send(PlayerMsg::Drained).ok();
                        drained = true;
                    }
                }
            }));

        (*self.mainloop).borrow_mut().unlock();
        stream_in.send(PlayerMsg::StreamEstablished).ok();
        Some((stream, stream_params.autostart))
    }

    pub fn enqueue(
        &mut self,
        stream: Rc<RefCell<Stream>>,
        autostart: slimproto::proto::AutoStart,
        stream_in: Sender<PlayerMsg>,
    ) {
        if self.playing.is_some() {
            self.next_up = Some(stream);
        } else {
            self.playing = Some(stream);
            if autostart == slimproto::proto::AutoStart::Auto {
                stream_in.send(PlayerMsg::TrackStarted).ok();
                self.play();
            }
        }
    }

    pub fn play(&mut self) -> bool {
        if let Some(ref stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            (*stream.as_ref()).borrow_mut().play();
            (*self.mainloop).borrow_mut().unlock();
            true
        } else {
            false
        }
    }

    pub fn unpause(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            (*stream.as_ref()).borrow_mut().unpause();
            (*self.mainloop).borrow_mut().unlock();
            true
        } else {
            false
        }
    }

    pub fn pause(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            (*stream.as_ref()).borrow_mut().pause();
            (*self.mainloop).borrow_mut().unlock();
            true
        } else {
            false
        }
    }

    pub fn stop(&mut self) {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            (*stream.as_ref()).borrow_mut().disconnect().ok();
            (*self.mainloop).borrow_mut().unlock();
        }
        self.next_up = None;
        self.playing = None;
    }

    pub fn flush(&mut self) {
        self.stop();
    }

    pub fn shift(&mut self) -> Option<Rc<RefCell<Stream>>> {
        if let Some(ref old_stream) = self.playing {
            (*old_stream.as_ref()).borrow_mut().disconnect().ok();
        }
        let old_stream = self.playing.take();
        self.playing = self.next_up.take();
        old_stream
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        (*self.context).borrow_mut().disconnect();
    }
}

pub fn get_output_device_names() -> anyhow::Result<Vec<String>> {
    let output = AudioOutput::try_new()?;
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
