use crate::cmds::{Command, DirectoryListingResponse, Response, ResponseData};
use buf_redux::Buffer;
use log::{debug, info, trace, warn};
use rusb::{Context, DeviceHandle, UsbContext};
use std::{
    collections::HashMap,
    error::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Poll, Waker},
    time::{Duration, Instant},
};
use std::{future::Future, io::Write};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum LibUsbError {
    #[error("No device found")]
    NoDeviceFound,
}

pub struct Device {
    handle: DeviceHandle<Context>,
}

impl Device {
    pub fn new(context: &Context) -> Result<Device, LibUsbError> {
        for device in context.devices().unwrap().iter() {
            let device_desc = device.device_descriptor().unwrap();

            debug!(
                "Bus {:03} Device {:03} ID {:04x}:{:04x}",
                device.bus_number(),
                device.address(),
                device_desc.vendor_id(),
                device_desc.product_id()
            );

            if device_desc.vendor_id() == 0xcafe && device_desc.product_id() == 0x4011 {
                trace!("Found badge!");

                let mut handle = device.open().unwrap();
                handle.reset().unwrap();

                return Ok(Device { handle });
            }
        }

        Err(LibUsbError::NoDeviceFound)
    }
}

impl Device {
    fn send(&self, data: &[u8]) -> Result<(), Box<dyn Error>> {
        let timeout = Duration::from_secs(10000);
        debug!("Sending bytes {:?}", data);
        let mut total_sent = 0;

        loop {
            let sent = self.handle.write_bulk(3, &data[total_sent..], timeout)?;
            total_sent += sent;

            if total_sent >= data.len() {
                break;
            }
        }

        Ok(())
    }

    fn receive(&self, data: &mut [u8]) -> Result<usize, Box<dyn Error>> {
        Ok(
            match self.handle.read_bulk(131, data, Duration::from_secs(15)) {
                Ok(len) => len,
                Err(rusb::Error::Timeout) => 0,
                other => other?,
            },
        )
    }

    fn reset(&self) -> Result<(), Box<dyn Error>> {
        info!("Resetting USB device");
        // self.handle.reset()?;

        Ok(())
    }
}

struct BadgeData {
    wakers: HashMap<u32, Arc<Mutex<BadgeRequestData>>>,
    last_message_id: u32,
}

pub struct Badge {
    device: Device,
    abort: AtomicBool,
    data: Mutex<BadgeData>,
}

pub struct BadgeRequestData {
    response: Option<Response>,
    waker: Option<Waker>,
    at: Instant,
}

pub struct BadgeRequest {
    data: Arc<Mutex<BadgeRequestData>>,
}

impl Future for BadgeRequest {
    type Output = ResponseData;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut data = self.data.lock().unwrap();
        if let Some(response) = &data.response {
            Poll::Ready(response.data.clone())
        } else {
            data.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[derive(Error, Debug)]
pub enum BadgeError {
    #[error("Invalid response received: {:?}", .0)]
    InvalidResponse(ResponseData),

    #[error("Execution of the command failed")]
    CommandFailed,
}

impl Badge {
    pub fn new(device: Device) -> Badge {
        Badge {
            device,
            abort: AtomicBool::new(false),
            data: Mutex::new(BadgeData {
                wakers: HashMap::new(),
                last_message_id: 0,
            }),
        }
    }

    pub fn close(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    fn send(&self, message_id: u32, command: Command) -> Result<(), Box<dyn Error>> {
        trace!("Requesting {:?} with message id {}", command, message_id);

        let bytes = command.to_bytes();
        let size = bytes.len() as u32;
        let mut packet = Vec::new();
        packet.write(&command.command().to_le_bytes())?;
        packet.write(&size.to_le_bytes())?;
        packet.write(&[0xde, 0xad])?;
        packet.write(&message_id.to_le_bytes())?;
        packet.write(&bytes)?;

        self.device.send(&packet)?;

        Ok(())
    }

    pub fn cmd_once(&self, command: Command) -> Result<BadgeRequest, Box<dyn Error>> {
        let mut data = self.data.lock().unwrap();
        data.last_message_id += 1;
        let message_id = data.last_message_id;
        trace!("Requesting {:?} with message id {}", command, message_id);
        let request_data = Arc::new(Mutex::new(BadgeRequestData {
            waker: None,
            response: None,
            at: Instant::now(),
        }));
        data.wakers.insert(message_id, request_data.clone());

        self.send(message_id, command)?;

        Ok(BadgeRequest { data: request_data })
    }

    pub async fn cmd(&self, command: Command) -> Result<ResponseData, Box<dyn Error>> {
        let mut i: i32 = 0;
        loop {
            trace!("Attempt {}", i);
            let result = self.cmd_once(command.clone())?;
            if i > 1 {
                std::thread::sleep(Duration::from_millis(500));
                // Send some serial input to wake up the device
                self.cmd_once(Command::SerialIn {
                    data: "\r\n\r\n\r\n\r\n".as_bytes().into(),
                })?
                .await;
            }
            let result = result.await;

            if let ResponseData::Timeout = result {
                i += 1;
                if i % 3 == 0 {
                    self.device.reset().unwrap();
                }

                continue;
            } else {
                return Ok(result);
            }
        }
    }

    pub async fn fetch_dir<S: Into<String>>(
        &self,
        dir: S,
    ) -> Result<DirectoryListingResponse, Box<dyn Error>> {
        let response = self.cmd(Command::FetchDir { path: dir.into() }).await?;
        if let ResponseData::DirectoryListing(listing) = response {
            Ok(listing)
        } else {
            Err(BadgeError::InvalidResponse(response))?
        }
    }

    pub async fn fetch_file<S: Into<String>>(&self, file: S) -> Result<Vec<u8>, Box<dyn Error>> {
        let response = self.cmd(Command::FetchFile { path: file.into() }).await?;
        if let ResponseData::FileContents(data) = response {
            Ok(data)
        } else {
            Err(BadgeError::InvalidResponse(response))?
        }
    }

    pub async fn ensure_ok(&self, cmd: Command) -> Result<(), Box<dyn Error>> {
        let response = self.cmd(cmd).await?;
        if let ResponseData::Ok = response {
            Ok(())
        } else if let ResponseData::Error = response {
            Err(BadgeError::CommandFailed)?
        } else {
            Err(BadgeError::InvalidResponse(response))?
        }
    }

    pub async fn create_dir<S: Into<String>>(&self, path: S) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::CreateDir { path: path.into() })
            .await
    }

    pub async fn create_file<S: Into<String>>(&self, path: S) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::CreateFile { path: path.into() })
            .await
    }

    pub async fn copy_file<S1: Into<String>, S2: Into<String>>(
        &self,
        from: S1,
        to: S2,
    ) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::CopyFile {
            from: from.into(),
            to: to.into(),
        })
        .await
    }

    pub async fn move_file<S1: Into<String>, S2: Into<String>>(
        &self,
        from: S1,
        to: S2,
    ) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::MoveFile {
            from: from.into(),
            to: to.into(),
        })
        .await
    }

    pub async fn write_file<S: Into<String>, B: AsRef<[u8]>>(
        &self,
        path: S,
        data: B,
    ) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::WriteFile {
            path: path.into(),
            data: data.as_ref().into(),
        })
        .await
    }

    pub async fn run_file<S: Into<String>>(&self, path: S) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::RunFile { path: path.into() }).await
    }

    pub async fn delete_path<S: Into<String>>(&self, path: S) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::DeletePath { path: path.into() })
            .await
    }

    pub async fn serial_in<S: AsRef<[u8]>>(&self, data: S) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::SerialIn {
            data: data.as_ref().into(),
        })
        .await
    }

    pub async fn heartbeat(&self) -> Result<(), Box<dyn Error>> {
        self.ensure_ok(Command::Heartbeat).await
    }

    pub fn run<F: Fn(String)>(self: Arc<Self>, stdout: F) {
        crossbeam::scope(|scope| {
            let me = self.clone();
            let t = scope.spawn(move |_| {
                while !me.abort.load(Ordering::Relaxed) {
                    me.send(0, Command::Heartbeat).unwrap();
                    std::thread::sleep(Duration::from_millis(250));
                }
            });

            let mut input = Buffer::new_ringbuf();
            let mut buf = [0u8; 256];
            while !self.abort.load(Ordering::Relaxed) {
                let device = &self.device;
                match device.receive(&mut buf) {
                    Ok(len) => {
                        self.data.lock().unwrap().wakers.retain(|_, value| {
                            let mut waker = value.lock().unwrap();

                            if waker.at < Instant::now() - Duration::from_secs(10) {
                                waker.response = Some(Response {
                                    message_id: 0,
                                    data: ResponseData::Timeout,
                                });
                                if let Some(waker) = waker.waker.take() {
                                    waker.wake();
                                }

                                false
                            } else {
                                true
                            }
                        });

                        trace!("Received {} bytes: {:?}", len, &buf[0..len]);
                        input.push_bytes(&buf[0..len]);

                        while let Some(response) = Response::try_read(&mut input).unwrap() {
                            let mut data = self.data.lock().unwrap();
                            if let Some(waker) = data.wakers.remove(&response.message_id) {
                                let mut waker = waker.lock().unwrap();
                                waker.response = Some(response);
                                if let Some(waker) = waker.waker.take() {
                                    waker.wake();
                                }
                            } else if let Response {
                                data: ResponseData::Log { text },
                                message_id: 0,
                            } = response
                            {
                                stdout(text);
                            } else {
                                warn!("Unhandled message: {:?}", response.data);
                            }
                        }

                        if input.len() > 0 {
                            warn!("Leftover input bytes: {}", input.len());
                            trace!("Leftover bytes: {:?}", input.buf())
                        }
                    }
                    Err(e) => {
                        println!("Error: {}", e);
                        break;
                    }
                }
            }

            t.join().unwrap();
        })
        .unwrap();
    }
}
