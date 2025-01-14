use std::{
    cell::RefCell,
    ops::Deref,
    rc::Rc,
    time::Duration,
};

use anyhow::anyhow;
use crossbeam::channel::{bounded, Sender};
use libpulse_binding::{
    callbacks::ListResult,
    context::{Context, FlagSet as CxFlagSet, State},
    def::BufferAttr,
    error::PAErr,
    mainloop::threaded::Mainloop,
    operation::Operation,
    sample::Spec,
    stream::{FlagSet as SmFlagSet, SeekMode},
    volume::ChannelVolumes,
};
use log::warn;

use crate::{
    decode::{AudioFormat, Decoder, DecoderError},
    PlayerMsg, StreamParams,
};

const MIN_AUDIO_BUFFER_SIZE: usize = 8 * 1024;

#[derive(Clone)]
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

    fn into_inner(self) -> Rc<RefCell<libpulse_binding::stream::Stream>> {
        self.inner
    }

    fn set_write_callback(&mut self, callback: Box<dyn FnMut(usize) + 'static>) {
        (*self.inner)
            .borrow_mut()
            .set_write_callback(Some(callback));
    }

    fn set_underflow_callback(&mut self, callback: Option<Box<dyn FnMut() + 'static>>) {
        (*self.inner).borrow_mut().set_underflow_callback(callback)
    }

    fn disconnect(&mut self) -> Result<(), PAErr> {
        (*self.inner).borrow_mut().disconnect()
    }

    fn set_state_callback(&mut self, callback: Option<Box<dyn FnMut() + 'static>>) {
        (*self.inner).borrow_mut().set_state_callback(callback)
    }

    fn connect_playback(
        &mut self,
        dev: Option<&str>,
        attr: Option<&BufferAttr>,
        flags: SmFlagSet,
        volume: Option<&ChannelVolumes>,
        sync_stream: Option<&mut libpulse_binding::stream::Stream>,
    ) -> Result<(), PAErr> {
        (*self.inner)
            .borrow_mut()
            .connect_playback(dev, attr, flags, volume, sync_stream)
    }

    fn get_state(&self) -> libpulse_binding::stream::State {
        (*self.inner).borrow_mut().get_state()
    }

    fn play(&mut self) {
        let op = (*self.inner).borrow_mut().uncork(None);
        self.do_op(op);
    }

    fn pause(&mut self) {
        (*self.inner).borrow_mut().cork(None);
    }

    fn unpause(&mut self) {
        self.play();
    }

    fn get_pos(&self) -> Duration {
        let micros = match (*self.inner).borrow().get_time() {
            Ok(Some(micros)) => micros,
            _ => libpulse_binding::time::MicroSeconds(0),
        };

        Duration::from_micros(micros.0)
    }

    fn do_op(&self, op: Operation<dyn FnMut(bool)>) {
        std::thread::spawn(move || {
            while op.get_state() == libpulse_binding::operation::State::Running {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }
}

pub struct AudioOutput {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<Context>>,
    playing: Option<Stream>,
    next_up: Option<Stream>,
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

    pub fn enqueue_new_stream(
        &mut self,
        mut decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        stream_params: StreamParams,
        device: &Option<String>,
    ) {
        // Create an audio buffer to hold raw u8 samples
        let buf_size = {
            let num_samps = decoder.dur_to_samples(stream_params.output_threshold) as usize;
            if num_samps < MIN_AUDIO_BUFFER_SIZE {
                MIN_AUDIO_BUFFER_SIZE
            } else {
                num_samps
            }
        };

        let mut audio_buf = Vec::with_capacity(buf_size);

        // Prefill audio buffer to threshold
        loop {
            match decoder.fill_raw_buffer(&mut audio_buf, None, stream_params.volume.clone()) {
                Ok(()) => {}

                Err(DecoderError::EndOfDecode) => {
                    stream_in.send(PlayerMsg::EndOfDecode).ok();
                }

                Err(DecoderError::Unhandled) => {
                    warn!("Unhandled format");
                    stream_in.send(PlayerMsg::NotSupported).ok();
                    return;
                }

                Err(DecoderError::StreamError(e)) => {
                    warn!("Error reading data stream: {}", e);
                    stream_in.send(PlayerMsg::NotSupported).ok();
                    return;
                }

                Err(DecoderError::Retry) => {
                    continue;
                }
            }
            break;
        }

        (*self.mainloop).borrow_mut().lock();
        let mut stream = match Stream::new(self.context.clone(), &decoder) {
            Some(stream) => stream,
            None => {
                stream_in.send(PlayerMsg::NotSupported).ok();
                return;
            }
        };
        (*self.mainloop).borrow_mut().unlock();

        // Add callback to pa_stream to feed music
        let stream_in_r1 = stream_in.clone();
        let stream_in_r2 = stream_in.clone();
        let mut draining = false;
        let drained = Rc::new(RefCell::new(false));
        let drained2 = drained.clone();
        let sm_ref = Rc::downgrade(&stream.clone().into_inner());
        let mut start_flag = true;

        (*self.mainloop).borrow_mut().lock();
        stream.set_write_callback(Box::new(move |len| {
            if *drained.borrow() {
                return;
            }

            if start_flag {
                stream_in_r1.send(PlayerMsg::TrackStarted).ok();
                start_flag = false;
            }

            loop {
                match decoder.fill_raw_buffer(
                    &mut audio_buf,
                    Some(len),
                    stream_params.volume.clone(),
                ) {
                    Ok(()) => {}

                    Err(DecoderError::EndOfDecode) => {
                        if !draining {
                            stream_in_r1.send(PlayerMsg::EndOfDecode).ok();
                            draining = true;
                        }
                    }

                    Err(DecoderError::Unhandled) => {
                        warn!("Unhandled format");
                        stream_in_r1.send(PlayerMsg::NotSupported).ok();
                        draining = true;
                    }

                    Err(DecoderError::StreamError(e)) => {
                        warn!("Error reading data stream: {}", e);
                        stream_in_r1.send(PlayerMsg::NotSupported).ok();
                        draining = true;
                    }

                    Err(DecoderError::Retry) => {
                        continue;
                    }
                }
                break;
            }

            if audio_buf.len() > 0 {
                let buf_len = if audio_buf.len() < len {
                    audio_buf.len()
                } else {
                    len
                };

                let offset = decoder.dur_to_samples(stream_params.skip.take()) as i64;

                if let Some(sm) = sm_ref.upgrade() {
                    unsafe {
                        (*sm.as_ptr())
                            .write_copy(
                                &audio_buf.drain(..buf_len).collect::<Vec<u8>>(),
                                offset,
                                SeekMode::Relative,
                            )
                            .ok();
                    }
                }
            }

            if draining && audio_buf.len() == 0 {
                *drained.borrow_mut() = true;
            }
        }));

        // Add callback to detect end of track
        stream.set_underflow_callback(Some(Box::new(move || {
            if *drained2.borrow() {
                stream_in_r2.send(PlayerMsg::Drained).ok();
            }
        })));
        (*self.mainloop).borrow_mut().unlock();

        // Connect playback stream
        if self.connect_stream(stream.clone(), device).is_err() {
            return;
        }

        stream_in.send(PlayerMsg::StreamEstablished).ok();
        self.enqueue(stream, stream_params.autostart, stream_in.clone());
    }

    fn connect_stream(
        &mut self,
        mut stream: Stream,
        device: &Option<String>,
    ) -> anyhow::Result<()> {
        (*self.mainloop).borrow_mut().lock();

        // Stream state change callback
        {
            let ml_ref = self.mainloop.clone();
            let sm_ref = self.context.clone();
            stream.set_state_callback(Some(Box::new(move || {
                let state = unsafe { (*sm_ref.as_ptr()).get_state() };
                match state {
                    State::Ready | State::Failed | State::Terminated => unsafe {
                        (*ml_ref.as_ptr()).signal(false);
                    },
                    _ => {}
                }
            })));
        }

        let flags =
            SmFlagSet::START_CORKED | SmFlagSet::AUTO_TIMING_UPDATE | SmFlagSet::INTERPOLATE_TIMING;

        stream.connect_playback(device.as_deref(), None, flags, None, None)?;

        // Wait for stream to be ready
        loop {
            match stream.get_state() {
                libpulse_binding::stream::State::Ready => {
                    break;
                }
                libpulse_binding::stream::State::Failed
                | libpulse_binding::stream::State::Terminated => {
                    (*self.mainloop).borrow_mut().unlock();
                    (*self.mainloop).borrow_mut().stop();
                    return Err(anyhow!(libpulse_binding::error::PAErr(
                        libpulse_binding::error::Code::ConnectionTerminated as i32,
                    )));
                }
                _ => {
                    (*self.mainloop).borrow_mut().wait();
                }
            }
        }

        stream.set_state_callback(None);
        (*self.mainloop).borrow_mut().unlock();

        Ok(())
    }

    fn play(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            stream.play();
            (*self.mainloop).borrow_mut().unlock();
            true
        } else {
            false
        }
    }

    fn enqueue(
        &mut self,
        stream: Stream,
        autostart: slimproto::proto::AutoStart,
        _stream_in: Sender<PlayerMsg>,
    ) {
        if self.playing.is_some() {
            self.next_up = Some(stream);
        } else {
            self.playing = Some(stream);
            if autostart == slimproto::proto::AutoStart::Auto {
                self.play();
            }
        }
    }

    pub fn unpause(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            stream.unpause();
            (*self.mainloop).borrow_mut().unlock();
            true
        } else {
            false
        }
    }

    pub fn pause(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            stream.pause();
            (*self.mainloop).borrow_mut().unlock();
            true
        } else {
            false
        }
    }

    pub fn stop(&mut self) {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            stream.disconnect().ok();
            (*self.mainloop).borrow_mut().unlock();
        }
        self.next_up = None;
        self.playing = None;
    }

    pub fn flush(&mut self) {
        self.stop();
    }

    pub fn shift(&mut self) {
        let old_stream = self.playing.take();
        self.playing = self.next_up.take();

        if let Some(old_stream) = old_stream {
            if let Some(pa_stream) = Rc::into_inner(old_stream.into_inner()) {
                let mut pa_stream = pa_stream.into_inner();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(1));
                    pa_stream.disconnect().ok();
                });
            };
        }
    }

    pub fn get_dur(&self) -> Duration {
        match self.playing {
            Some(ref stream) => stream.get_pos(),
            None => Duration::ZERO,
        }
    }

    pub fn get_output_device_names(&self) -> anyhow::Result<Vec<String>> {
        let mut ret = Vec::new();
        let (s, r) = bounded(1);
    
        (*self.mainloop).borrow_mut().lock();
        let _op = (*self.context)
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
        (*self.mainloop).borrow_mut().unlock();
    
        while let Some(name) = r.recv()? {
            ret.push(name);
        }
    
        Ok(ret)
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        (*self.context).borrow_mut().disconnect();
    }
}
