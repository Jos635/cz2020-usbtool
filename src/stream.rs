use buf_redux::Buffer;
use std::sync::Mutex;

pub struct Stream {
    data: Mutex<Buffer>,
}

impl Stream {
    pub fn new() -> Stream {
        Stream {
            data: Mutex::new(Buffer::new()),
        }
    }

    pub fn read(&self, buf: &mut [u8]) -> usize {
        let mut data = self.data.lock().unwrap();
        data.copy_to_slice(buf)
    }

    pub fn write(&self, buf: &[u8]) {
        let mut data = self.data.lock().unwrap();
        data.push_bytes(buf);
    }
}
