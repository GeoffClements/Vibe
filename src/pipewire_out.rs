// src/pipewire_out.rs
// Pipewire audio output implementation
//
// Required system dependencies:
// - Pipewire development libraries (libpipewire-0.3-dev on Debian-based systems)
// - SPA development libraries (libspa-0.2-dev on Debian-based systems)
// - clang development libraries (libclang-dev on Debian-based systems)

use std::time::Duration;

use crossbeam::channel::{bounded, Sender};
use pipewire::{context::ContextRc, core::CoreRc, thread_loop::ThreadLoopRc, types::ObjectType};

use crate::{audio_out::AudioOutput, decode::Decoder, message::PlayerMsg, StreamParams};

pub struct PipewireAudioOutput {
    mainloop: ThreadLoopRc,
    _context: ContextRc,
    core: CoreRc,
}

impl PipewireAudioOutput {
    pub fn try_new() -> anyhow::Result<Self> {
        let mainloop = unsafe { ThreadLoopRc::new(None, None) }?;
        let context = ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        Ok(Self {
            mainloop,
            _context: context,
            core,
        })
    }
}

impl AudioOutput for PipewireAudioOutput {
    fn enqueue_new_stream(
        &mut self,
        _decoder: Decoder,
        _stream_in: Sender<PlayerMsg>,
        _stream_params: StreamParams,
        _device: &Option<String>,
    ) {
        todo!()
    }

    fn flush(&mut self) {
        todo!()
    }

    fn get_dur(&self) -> Duration {
        todo!()
    }

    fn pause(&mut self) -> bool {
        todo!()
    }

    fn shift(&mut self) {
        todo!()
    }

    fn stop(&mut self) {
        todo!()
    }

    fn unpause(&mut self) -> bool {
        todo!()
    }

    fn get_output_device_names(&self) -> anyhow::Result<Vec<(String, Option<String>)>> {
        let registry = self.core.get_registry()?;

        let mut ret = Vec::new();
        let (s, r) = bounded(1);

        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Node {
                    if let Some(props) = global.props {
                        if props.get("media.class") == Some("Audio/Sink") {
                            let device_name = props.get("node.name").unwrap_or_default().to_owned();
                            let device_desc = props.get("node.description").map(|s| s.to_owned());
                            let _ = s.send((device_name, device_desc));
                        }
                    }
                }
            })
            .register();

        self.mainloop.start();
        while let Ok(item) = r.recv_timeout(Duration::from_millis(250)) {
            ret.push(item);
        }
        self.mainloop.stop();

        Ok(ret)
    }
}
