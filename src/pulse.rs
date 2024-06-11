use std::{cell::RefCell, ops::Deref, rc::Rc};

use crossbeam::channel::bounded;
use libpulse_binding::{
    self as pa,
    callbacks::ListResult,
    context::{Context, FlagSet as CxFlagSet},
    error::PAErr,
    mainloop::threaded::Mainloop,
    stream::{FlagSet as SmFlagSet, Stream},
};

pub struct Output {
    mainloop: Mainloop,
    context: Context,
}

impl Output {
    pub fn try_new() -> Result<Self, PAErr> {
        let ml = Rc::new(RefCell::new(
            Mainloop::new().ok_or(pa::error::Code::ConnectionRefused)?,
        ));

        let cx = Rc::new(RefCell::new(
            Context::new(ml.borrow_mut().deref(), "Vibe")
                .ok_or(pa::error::Code::ConnectionRefused)?,
        ));

        // Context state change callback
        {
            let ml_ref = ml.clone();
            let cx_ref = cx.clone();
            cx.borrow_mut().set_state_callback(Some(Box::new(move || {
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

        cx.borrow_mut().connect(None, CxFlagSet::NOFLAGS, None)?;
        ml.borrow_mut().lock();
        ml.borrow_mut().start()?;

        // Wait for context to be ready
        loop {
            match cx.borrow().get_state() {
                pa::context::State::Ready => {
                    break;
                }
                pa::context::State::Failed | pa::context::State::Terminated => {
                    ml.borrow_mut().unlock();
                    ml.borrow_mut().stop();
                    return Err(pa::error::PAErr(
                        pa::error::Code::ConnectionTerminated as i32,
                    ));
                }
                _ => ml.borrow_mut().wait(),
            }
        }

        cx.borrow_mut().set_state_callback(None);
        ml.borrow_mut().unlock();

        Ok(Output {
            mainloop: Rc::into_inner(ml).unwrap().into_inner(),
            context: Rc::into_inner(cx).unwrap().into_inner(),
        })
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.context.disconnect();
    }
}

pub fn setup() -> Result<(Rc<RefCell<Mainloop>>, Rc<RefCell<Context>>), PAErr> {
    let ml = Rc::new(RefCell::new(
        Mainloop::new().ok_or(pa::error::Code::ConnectionRefused)?,
    ));

    let cx = Rc::new(RefCell::new(
        Context::new(ml.borrow_mut().deref(), "Vibe").ok_or(pa::error::Code::ConnectionRefused)?,
    ));

    // Context state change callback
    {
        let ml_ref = ml.clone();
        let cx_ref = cx.clone();
        cx.borrow_mut().set_state_callback(Some(Box::new(move || {
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

    cx.borrow_mut().connect(None, CxFlagSet::NOFLAGS, None)?;
    ml.borrow_mut().lock();
    ml.borrow_mut().start()?;

    // Wait for context to be ready
    loop {
        match cx.borrow().get_state() {
            pa::context::State::Ready => {
                break;
            }
            pa::context::State::Failed | pa::context::State::Terminated => {
                ml.borrow_mut().unlock();
                ml.borrow_mut().stop();
                return Err(pa::error::PAErr(
                    pa::error::Code::ConnectionTerminated as i32,
                ));
            }
            _ => ml.borrow_mut().wait(),
        }
    }

    cx.borrow_mut().set_state_callback(None);
    ml.borrow_mut().unlock();

    Ok((ml, cx))
}

pub fn connect_stream(
    ml: Rc<RefCell<Mainloop>>,
    sm: Rc<RefCell<Stream>>,
    device: &Option<String>,
) -> Result<(), PAErr> {
    ml.borrow_mut().lock();

    // Stream state change callback
    {
        let ml_ref = ml.clone();
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

pub fn get_output_device_names() -> anyhow::Result<Vec<String>> {
    let mut output = Output::try_new()?;
    let mut ret = Vec::new();
    let (s, r) = bounded(1);

    output.mainloop.lock();
    let _op = output
        .context
        .introspect()
        .get_sink_info_list(move |listresult| match listresult {
            ListResult::Item(item) => {
                s.send(item.name.as_ref().map(|n| n.to_string())).ok();
            }
            ListResult::End | ListResult::Error => {
                s.send(None).ok();
            }
        });
    output.mainloop.unlock();

    while let Some(name) = r.recv()? {
        ret.push(name);
    }

    Ok(ret)
}
