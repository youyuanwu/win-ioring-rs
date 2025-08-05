// This provides safe wrappers for io_ring.

use std::{
    borrow::BorrowMut,
    cell::RefCell,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use futures::FutureExt;
use futures::channel::oneshot;
use windows::core::HRESULT;

use crate::{io_ring::IoRing, sys::AsyncEvent};

#[cfg(test)]
mod tests;

pub mod rt;

pub type BufResult = (windows::core::Result<()>, Vec<u8>);

pub struct Driver {
    io_ring: Handle,
    event: AsyncEvent,
    // receives notifications there are submissions pushed.
    submit_notify_rx: Rc<AsyncEvent>,
    shutdown_notify_rx: Rc<AsyncEvent>,
}

pub struct HandleInner {
    io_ring: IoRing,
    // signals there are submissions pushed.
    submit_notify_tx: Rc<AsyncEvent>,
    shutdown_notify_tx: Rc<AsyncEvent>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        // Manual close of the ring.
        self.io_ring.close().unwrap();
    }
}

impl HandleInner {
    pub fn as_io_ring(&self) -> &IoRing {
        &self.io_ring
    }
}

#[derive(Clone)]
pub struct Handle {
    inner: Rc<RefCell<HandleInner>>,
}

impl Handle {
    pub fn as_inner(&self) -> std::cell::RefMut<'_, HandleInner> {
        self.inner.as_ref().borrow_mut()
    }

    /// Triggers shutdown of the driver.
    /// User needs to ensure all operations are completed before calling this.
    pub fn shutdown(&self) {
        // Notify the driver to shutdown.
        self.as_inner().shutdown_notify_tx.signal().unwrap();
        // We never reset the event because we do not support restart.
    }
}

enum UserData {
    // None,
    Read {
        buffer: Vec<u8>,
        tx: oneshot::Sender<(Vec<u8>, HRESULT)>,
    },
}

impl Driver {
    pub fn new(mut io_ring: IoRing) -> Self {
        let event = crate::sys::AsyncEvent::new().expect("Failed to create AsyncEvent");
        io_ring
            .set_io_ring_completion_event(event.handle())
            .expect("Failed to set completion event");

        let submit_notify = Rc::new(AsyncEvent::new_manual_reset().unwrap());
        let shutdown_notify = Rc::new(AsyncEvent::new_manual_reset().unwrap());
        Self {
            io_ring: Handle {
                inner: Rc::new(RefCell::new(HandleInner {
                    io_ring,
                    submit_notify_tx: submit_notify.clone(),
                    shutdown_notify_tx: shutdown_notify.clone(),
                })),
            },
            event,
            submit_notify_rx: submit_notify,
            shutdown_notify_rx: shutdown_notify,
        }
    }

    pub fn handle(&self) -> Handle {
        self.io_ring.clone()
    }

    // Run the loop to process completions
    pub async fn drive(&self) {
        loop {
            futures::select! {
                // Wait for new submissions
                _ = self.submit_notify_rx.wait().fuse() => {
                    self.submit_notify_rx.reset().unwrap();
                    // Submit pending operations
                    let mut inner = self.io_ring.as_inner();
                    let ring = &mut inner.borrow_mut().io_ring;
                    ring.submit(0, 0).unwrap();
                }
                // Wait for completions
                _ = self.event.wait().fuse() => {
                    // Reset the event for next completion
                    self.event.reset().unwrap();
                    // process all completions.
                    let mut inner = self.io_ring.as_inner();
                    let ring = &mut inner.borrow_mut().io_ring;
                    while let Some(completion) = ring.pop_completion() {
                        let ctx = unsafe { Box::from_raw(completion.UserData as *mut UserData) };
                        match *ctx {
                            UserData::Read { buffer, tx } => {
                                if tx.send((buffer, completion.ResultCode)).is_err() {
                                    panic!("fail to send completion. Channel closed");
                                }
                            }
                            // _ => {}
                        }
                    }

                }
                _ = self.shutdown_notify_rx.wait().fuse() => {
                    // Shutdown signal received, break the loop
                    // TODO: user is responsible for ensure all operations are completed before shutdown.
                    break;
                }
            }
        }
    }
}

impl Handle {
    pub fn build_read_file(
        &mut self,
        file: &crate::file::File,
        mut buffer: Vec<u8>,
        num_of_bytes_to_read: u32,
        offset: u64,
    ) -> windows::core::Result<ReadFut> {
        let mut inner = self.inner.as_ref().borrow_mut();
        let (tx, rx) = futures::channel::oneshot::channel();

        let ring = &mut inner.io_ring;
        let ptr = buffer.as_mut_ptr() as *mut _;
        let user_data = Box::new(UserData::Read { buffer, tx });
        unsafe {
            ring.build_read_file(
                crate::io_ring::ops::ReadOp::builder()
                    .with_raw_handle(file.as_raw_handle())
                    .with_raw_data_address(ptr)
                    .with_num_of_bytes_to_read(num_of_bytes_to_read)
                    .with_offset(offset)
                    .with_user_data(Box::into_raw(user_data) as *mut _ as usize)
                    .build(),
            )
        }?;

        // Signal the driver there is a new entry.
        // Driver does not process it until the next await point.
        inner.submit_notify_tx.signal().unwrap();

        // signal submission ready.
        Ok(ReadFut::new(rx))
    }
}

enum ReadFutState {
    SubmitSignalled,
    Completed,
}

pub struct ReadFut {
    rx: oneshot::Receiver<(Vec<u8>, HRESULT)>,
    state: ReadFutState,
}

impl ReadFut {
    pub fn new(rx: oneshot::Receiver<(Vec<u8>, HRESULT)>) -> Self {
        Self {
            rx,
            state: ReadFutState::SubmitSignalled,
        }
    }
}

impl Future for ReadFut {
    type Output = BufResult;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.state {
            ReadFutState::SubmitSignalled => {
                use futures::ready;

                // Wait for driver to reply.
                let res = ready!(self.rx.poll_unpin(cx));
                match res {
                    Ok((buffer, hr)) => {
                        let ok = hr.ok();
                        self.state = ReadFutState::Completed;
                        Poll::Ready((ok, buffer))
                    }
                    Err(_) => {
                        panic!("Driver closed?");
                    }
                }
            }
            ReadFutState::Completed => {
                // Already completed.
                unreachable!("completed should not be polled again");
            }
        }
    }
}
