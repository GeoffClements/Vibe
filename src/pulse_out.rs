// src/pulse_out.rs
// PulseAudio audio output implementation
//
// Required system dependencies:
// - PulseAudio development libraries (libpulse-dev on Debian-based systems)

use std::{cell::RefCell, ops::Deref, rc::Rc, time::Duration};

use anyhow::anyhow;
use crossbeam::channel::{bounded, Sender};
use log::warn;
use pulse::{
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

use crate::{
    SKIP, StreamParams, audio_out::AudioOutput, decode::{Decoder, DecoderError}, message::PlayerMsg
};

const MIN_AUDIO_BUFFER_SIZE: usize = 8 * 1024;

#[derive(Clone)]
pub struct Stream {
    inner: Rc<RefCell<pulse::stream::Stream>>,
}

impl Stream {
    fn new(context: Rc<RefCell<Context>>, decoder: &Decoder) -> Option<Self> {
        let spec = Spec {
            // format: match decoder.format() {
            //     AudioFormat::I16 | AudioFormat::U16 => pulse::sample::Format::S16NE,
            //     AudioFormat::I32 | AudioFormat::U32 => pulse::sample::Format::S32NE,
            //     AudioFormat::F32 => pulse::sample::Format::F32le,
            // },
            format: pulse::sample::Format::F32le,
            rate: decoder.sample_rate(),
            channels: decoder.channels(),
        };

        // Create a pulseaudio stream
        let stream =
            pulse::stream::Stream::new(&mut (*context).borrow_mut(), "Music", &spec, None)?;

        Some(Self {
            inner: Rc::new(RefCell::new(stream)),
        })
    }

    fn into_inner(self) -> Rc<RefCell<pulse::stream::Stream>> {
        self.inner
    }

    fn set_write_callback(&mut self, callback: Box<dyn FnMut(usize) + 'static>) {
        (*self.inner)
            .borrow_mut()
            .set_write_callback(Some(callback));
    }

    fn unset_write_callback(&mut self) {
        (*self.inner).borrow_mut().set_write_callback(None);
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
        sync_stream: Option<&mut pulse::stream::Stream>,
    ) -> Result<(), PAErr> {
        (*self.inner)
            .borrow_mut()
            .connect_playback(dev, attr, flags, volume, sync_stream)
    }

    fn get_state(&self) -> pulse::stream::State {
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
            _ => pulse::time::MicroSeconds(0),
        };

        Duration::from_micros(micros.0)
    }

    fn do_op(&self, op: Operation<dyn FnMut(bool)>) {
        std::thread::spawn(move || {
            while op.get_state() == pulse::operation::State::Running {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }
}

pub struct PulseAudioOutput {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<Context>>,
    playing: Option<Stream>,
    next_up: Option<Stream>,
}

impl PulseAudioOutput {
    pub fn try_new() -> anyhow::Result<Self> {
        let mainloop = Rc::new(RefCell::new(
            Mainloop::new().ok_or(pulse::error::Code::ConnectionRefused)?,
        ));

        let context = Rc::new(RefCell::new(
            Context::new((*mainloop).borrow_mut().deref(), "Vibe")
                .ok_or(pulse::error::Code::ConnectionRefused)?,
        ));

        // Context state change callback
        {
            let mainloop_ref = mainloop.clone();
            let context_ref = context.clone();
            (*context)
                .borrow_mut()
                .set_state_callback(Some(Box::new(move || {
                    let state = unsafe { (*context_ref.as_ptr()).get_state() };
                    match state {
                        State::Ready | State::Terminated | State::Failed => unsafe {
                            (*mainloop_ref.as_ptr()).signal(false);
                        },
                        _ => {}
                    }
                })))
        }

        (*context)
            .borrow_mut()
            .connect(None, CxFlagSet::NOFLAGS, None)?;
        (*mainloop).borrow_mut().lock();
        (*mainloop).borrow_mut().start()?;

        // Wait for context to be ready
        loop {
            match context.borrow().get_state() {
                State::Ready => {
                    break;
                }
                State::Failed | State::Terminated => {
                    (*mainloop).borrow_mut().unlock();
                    (*mainloop).borrow_mut().stop();
                    return Err(anyhow!("Unable to connect with pulseaudio"));
                }
                _ => (*mainloop).borrow_mut().wait(),
            }
        }

        (*context).borrow_mut().set_state_callback(None);
        (*mainloop).borrow_mut().unlock();

        Ok(PulseAudioOutput {
            mainloop,
            context,
            playing: None,
            next_up: None,
        })
    }

    fn connect_stream(
        &mut self,
        mut stream: Stream,
        device: &Option<String>,
    ) -> anyhow::Result<()> {
        (*self.mainloop).borrow_mut().lock();

        // Stream state change callback
        {
            let mainloop_ref = self.mainloop.clone();
            let stream_ref = self.context.clone();
            stream.set_state_callback(Some(Box::new(move || {
                let state = unsafe { (*stream_ref.as_ptr()).get_state() };
                match state {
                    State::Ready | State::Failed | State::Terminated => unsafe {
                        (*mainloop_ref.as_ptr()).signal(false);
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
                pulse::stream::State::Ready => {
                    break;
                }
                pulse::stream::State::Failed | pulse::stream::State::Terminated => {
                    (*self.mainloop).borrow_mut().unlock();
                    (*self.mainloop).borrow_mut().stop();
                    return Err(anyhow!(pulse::error::PAErr(
                        pulse::error::Code::ConnectionTerminated as i32,
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
}

impl AudioOutput for PulseAudioOutput {
    fn enqueue_new_stream(
        &mut self,
        mut decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        stream_params: StreamParams,
        device: &Option<String>,
    ) {
        // Create an audio buffer to hold raw u8 samples
        let buf_size = {
            let num_samples = decoder.dur_to_samples(stream_params.output_threshold) as usize;
            if num_samples < MIN_AUDIO_BUFFER_SIZE {
                MIN_AUDIO_BUFFER_SIZE
            } else {
                num_samples
            }
        };

        let mut audio_buf = Vec::with_capacity(buf_size);

        // Prefill audio buffer to threshold
        loop {
            match decoder.fill_raw_buffer(&mut audio_buf, None) {
                Ok(()) => {}

                Err(DecoderError::EndOfDecode) => {
                    stream_in.send(PlayerMsg::EndOfDecode).ok();
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

        {
            let mut start_flag = true;
            let mut draining = false;
            let drained = Rc::new(RefCell::new(false));
            let stream_ref = Rc::downgrade(&stream.clone().into_inner());
            let drained_ref = drained.clone();
            let stream_in_ref = stream_in.clone();
            (*self.mainloop).borrow_mut().lock();
            stream.set_write_callback(Box::new(move |len| {
                if *drained_ref.borrow() {
                    return;
                }

                if start_flag {
                    stream_in_ref.send(PlayerMsg::TrackStarted).ok();
                    start_flag = false;
                }

                loop {
                    match decoder.fill_raw_buffer(&mut audio_buf, Some(len)) {
                        Ok(()) => {}

                        Err(DecoderError::EndOfDecode) => {
                            if !draining {
                                stream_in_ref.send(PlayerMsg::EndOfDecode).ok();
                                draining = true;
                            }
                        }

                        Err(DecoderError::StreamError(e)) => {
                            warn!("Error reading data stream: {}", e);
                            stream_in_ref.send(PlayerMsg::NotSupported).ok();
                            draining = true;
                        }

                        Err(DecoderError::Retry) => {
                            continue;
                        }
                    }
                    break;
                }

                if !audio_buf.is_empty() {
                    let buf_len = if audio_buf.len() < len {
                        audio_buf.len()
                    } else {
                        len
                    };

                    let offset = (decoder.dur_to_samples(SKIP.take())
                        * size_of::<f32>() as u64) as i64;

                    if let Some(stream) = stream_ref.upgrade() {
                        unsafe {
                            (*stream.as_ptr())
                                .write_copy(
                                    &audio_buf.drain(..buf_len).collect::<Vec<u8>>(),
                                    offset,
                                    SeekMode::Relative,
                                )
                                .ok();
                        }
                    }
                }

                if draining && audio_buf.is_empty() {
                    *drained_ref.borrow_mut() = true;
                }
            }));

            // Add callback to detect end of track
            let stream_in_ref = stream_in.clone();
            stream.set_underflow_callback(Some(Box::new(move || {
                if *drained.borrow() {
                    stream_in_ref.send(PlayerMsg::Drained).ok();
                }
            })));
            (*self.mainloop).borrow_mut().unlock();
        }

        // Connect playback stream
        if self.connect_stream(stream.clone(), device).is_err() {
            return;
        }

        stream_in.send(PlayerMsg::StreamEstablished).ok();
        self.enqueue(stream, stream_params.autostart, stream_in.clone());
    }

    fn unpause(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            stream.unpause();
            (*self.mainloop).borrow_mut().unlock();
            true
        } else {
            false
        }
    }

    fn pause(&mut self) -> bool {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            stream.pause();
            (*self.mainloop).borrow_mut().unlock();
            true
        } else {
            false
        }
    }

    fn stop(&mut self) {
        if let Some(ref mut stream) = self.playing {
            (*self.mainloop).borrow_mut().lock();
            stream.unset_write_callback();
            stream.disconnect().ok();
            (*self.mainloop).borrow_mut().unlock();
        }
        self.next_up = None;
        self.playing = None;
    }

    fn flush(&mut self) {
        self.stop();
    }

    fn shift(&mut self) {
        self.playing = self.next_up.take();
    }

    fn get_dur(&self) -> Duration {
        match self.playing {
            Some(ref stream) => stream.get_pos(),
            None => Duration::ZERO,
        }
    }

    fn get_output_device_names(&self) -> anyhow::Result<Vec<(String, Option<String>)>> {
        let mut ret = Vec::new();
        let (s, r) = bounded(1);

        (*self.mainloop).borrow_mut().lock();
        let _op = (*self.context)
            .borrow_mut()
            .introspect()
            .get_sink_info_list(move |list_result| match list_result {
                ListResult::Item(item) => {
                    let name = item.name.to_owned().unwrap_or_default().to_string();
                    let description = item.description.to_owned().map(|n| n.to_string());
                    s.send(Some((name, description))).ok();
                }
                ListResult::End | ListResult::Error => {
                    s.send(None).ok();
                }
            });
        (*self.mainloop).borrow_mut().unlock();

        while let Some(item) = r.recv()? {
            ret.push(item);
        }

        Ok(ret)
    }
}

impl Drop for PulseAudioOutput {
    fn drop(&mut self) {
        (*self.context).borrow_mut().disconnect();
    }
}
