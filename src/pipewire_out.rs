use std::time::Duration;

use crossbeam::channel::{bounded, Sender};
use pipewire::{
    context::{self, ContextBox},
    core::CoreBox,
    thread_loop::ThreadLoopBox,
    types::ObjectType,
};

use crate::{audio_out::AudioOutput, decode::Decoder, message::PlayerMsg, StreamParams};

pub struct PipewireAudioOutput {
    mainloop: ThreadLoopBox,
    // context: ContextBox<'l>,
    // core: CoreBox<'l>,
}

impl PipewireAudioOutput {
    pub fn try_new() -> anyhow::Result<Self> {
        let mainloop = unsafe { ThreadLoopBox::new(None, None) }?;
        // let context = ContextBox::new(mainloop.loop_(), None)?;
        // let core = context.connect(None)?;

        Ok(Self {
            mainloop: mainloop,
            // context: context,
            // core: core,
        })
    }
}

impl AudioOutput for PipewireAudioOutput {
    fn enqueue_new_stream(
        &mut self,
        decoder: Decoder,
        stream_in: Sender<PlayerMsg>,
        stream_params: StreamParams,
        device: &Option<String>,
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
        let context = ContextBox::new(self.mainloop.loop_(), None)?;
        let core = context.connect(None)?;
        let registry = core.get_registry()?;

        let mut ret = Vec::new();
        let (s, r) = bounded(1);

        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Node {
                    if let Some(props) = global.props {
                        if props.get("media.class") == Some("Audio/Sink") {
                            // if media_class.starts_with("Audio/Sink") {
                            // for key in props.keys() {
                            //     println!("    {} = {:?}", key, props.get(key));
                            // }
                            // println!("----");
                            let device_name = props.get("node.name").unwrap_or_default().to_owned();
                            let device_desc = props.get("node.description").map(|s| s.to_owned());
                            let _ = s.send((device_name, device_desc));
                            // }
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
