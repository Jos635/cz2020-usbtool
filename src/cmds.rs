use buf_redux::Buffer;
use log::{debug, trace, warn};
use std::{convert::TryInto, error::Error, ffi::CString, io::Write};

#[derive(Debug, Clone)]
pub enum Command {
    CreateDir {
        path: String,
    },
    /// Don't include trailing slash
    FetchDir {
        path: String,
    },

    CreateFile {
        path: String,
    },
    FetchFile {
        path: String,
    },
    CopyFile {
        from: String,
        to: String,
    },
    MoveFile {
        from: String,
        to: String,
    },
    WriteFile {
        path: String,
        data: Vec<u8>,
    },

    /// Don't include /flash prefix
    RunFile {
        path: String,
    },

    DeletePath {
        path: String,
    },
    SerialIn {
        data: Vec<u8>,
    },
    Heartbeat,
}

fn str_to_null_terminated_buf<S: AsRef<str>>(s: S) -> Vec<u8> {
    CString::new(s.as_ref())
        .unwrap()
        .as_bytes_with_nul()
        .try_into()
        .unwrap()
}

impl Command {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Command::CreateDir { path }
            | Command::FetchDir { path }
            | Command::CreateFile { path }
            | Command::FetchFile { path }
            | Command::RunFile { path }
            | Command::DeletePath { path } => str_to_null_terminated_buf(path),

            Command::CopyFile { from, to } | Command::MoveFile { from, to } => {
                let mut v = Vec::new();
                v.write(CString::new(from.as_str()).unwrap().as_bytes_with_nul())
                    .unwrap();
                v.write(CString::new(to.as_str()).unwrap().as_bytes_with_nul())
                    .unwrap();

                v
            }
            Command::WriteFile { path, data } => {
                let mut v = str_to_null_terminated_buf(path);
                v.write(data).unwrap();

                v
            }
            Command::SerialIn { data } => data.clone(),
            Command::Heartbeat => str_to_null_terminated_buf("beat"),
        }
    }

    pub fn command(&self) -> u16 {
        match self {
            Command::CreateDir { path: _ } => 4102,
            Command::FetchDir { path: _ } => 4096,
            Command::CreateFile { path: _ } => 4098,
            Command::FetchFile { path: _ } => 4097,
            Command::CopyFile { from: _, to: _ } => 4100,
            Command::MoveFile { from: _, to: _ } => 4101,
            Command::WriteFile { path: _, data: _ } => 4098,
            Command::RunFile { path: _ } => 0,
            Command::DeletePath { path: _ } => 4099,
            Command::SerialIn { data: _ } => 2,
            Command::Heartbeat => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub enum FsEntry {
    File(String),
    Directory(String),
}

impl FsEntry {
    pub fn name(&self) -> &str {
        match self {
            FsEntry::File(name) | FsEntry::Directory(name) => name,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DirectoryListingResponse {
    Found {
        requested: String,
        entries: Vec<FsEntry>,
    },
    DirectoryNotFound,
}

#[derive(Debug, Clone)]
pub enum ResponseData {
    Log {
        text: String,
    },
    DirectoryListing(DirectoryListingResponse),

    /// If you request the contents of a non-existant file, you will get "Can\'t open file" back as contents
    FileContents(Vec<u8>),
    Ok,
    Error,
    Timeout,
    Unknown,
}

pub struct Response {
    pub message_id: u32,
    pub data: ResponseData,
}

impl Response {
    pub fn try_read(input: &mut Buffer) -> Result<Option<Response>, Box<dyn Error>> {
        loop {
            if input.len() < 12 {
                return Ok(None);
            }

            let check = &input.buf()[6..8];
            if check == [0xde, 0xad] {
                break;
            } else {
                warn!("Invalid magic numbers in header: {:?}!", check);
                input.consume(1);
            }
        }

        let len = u32::from_le_bytes(input.buf()[2..6].try_into().unwrap()) as usize;
        if input.len() < 12 + len {
            debug!("Waiting on {}+12 input bytes", len);
            return Ok(None);
        }

        let command = u16::from_le_bytes(input.buf()[0..2].try_into().unwrap());
        let message_id = u32::from_le_bytes(input.buf()[8..12].try_into().unwrap());
        let data = &input.buf()[12..12 + len];
        let data_str = data.iter().map(|b| *b as char).collect::<String>();

        trace!(
            "Received response: command={}, message_id={}, len={}, data={:?}, data_str={:?}",
            command,
            message_id,
            len,
            data,
            data_str
        );

        let data = match command {
            3 => ResponseData::Log { text: data_str },
            4096 => ResponseData::DirectoryListing(match data_str.as_str() {
                "Directory_not_found" => DirectoryListingResponse::DirectoryNotFound,
                _ => {
                    let mut split = data_str.split('\n');
                    DirectoryListingResponse::Found {
                        requested: split.next().unwrap().to_owned(),
                        entries: split
                            .map(|x| match x.chars().next() {
                                Some('f') => FsEntry::File(x[1..].to_owned()),
                                Some('d') => FsEntry::Directory(x[1..].to_owned()),
                                other => panic!("Unexpected type: {:?}", other),
                            })
                            .collect(),
                    }
                }
            }),
            4097 => ResponseData::FileContents(data.into()),
            0 | 1 | 2 | 4098 | 4099 | 4100 | 4101 | 4102 => {
                if data == [111, 107, 0] {
                    ResponseData::Ok
                } else {
                    ResponseData::Error
                }
            }
            _ => ResponseData::Unknown,
        };

        debug!("{:?}", data);
        input.consume(12 + len);

        return Ok(Some(Response { message_id, data }));
    }
}
