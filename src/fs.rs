use crate::{
    cmds::{DirectoryListingResponse, FsEntry},
    device::Badge,
    stream::Stream,
};
use buf_redux::Buffer;
use fuse::{FileAttr, FileType, Filesystem};
use libc::{EAGAIN, EIO, ENOENT, ENOSYS};
use log::{debug, error, info};
use nix::unistd::{getegid, geteuid};
use std::{
    cell::RefCell,
    ops::Add,
    sync::Arc,
    time::{Duration, Instant},
};
use time::Timespec;
use tokio::runtime::Runtime;

// ! WARNING: Garbage ahead. Beware of the shitty code.

type Node = Arc<RefCell<Ino>>;

pub struct AppFS<'a> {
    app: Arc<Badge>,
    io: &'a Stream,
    nodes: Vec<Node>,
    rt: Arc<RefCell<Runtime>>,
}

const TTL: Timespec = Timespec { sec: 10, nsec: 0 }; // 10 seconds
const CREATE_TIME: Timespec = Timespec {
    sec: 1381237736,
    nsec: 0,
}; // 2013-10-08 08:56

fn default_attr() -> FileAttr {
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    FileAttr {
        ino: 0,
        size: 0,
        blocks: 0,
        atime: CREATE_TIME,
        mtime: CREATE_TIME,
        ctime: CREATE_TIME,
        crtime: CREATE_TIME,
        kind: FileType::Directory,
        perm: 0o644,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        flags: 0,
    }
}

#[derive(Debug)]
enum InoData {
    File { contents: Option<Vec<u8>> },
    Directory { children: Option<Vec<Node>> },
    Serial { pending_data: Buffer },
    Run,
}

#[derive(Debug)]
struct Ino {
    ino: u64,
    path: String,
    name: String,
    last_update: Instant,
    data: InoData,
}

impl Ino {
    pub fn dir<P: Into<String>>(path: P, ino: u64) -> Ino {
        Ino {
            ino,
            path: path.into(),
            name: String::new(),
            data: InoData::Directory { children: None },
            last_update: Instant::now(),
        }
    }

    pub fn ensure_data<'a>(&mut self, appfs: &mut AppFS) {
        let path = self.path.clone();
        match &mut self.data {
            InoData::File { contents } => {
                if contents.is_some() && self.last_update > Instant::now() - Duration::from_secs(30)
                {
                    // Cache file contents for 30 seconds
                    return;
                }

                println!("Loading info for {:?}", path);
                *contents = Some(
                    appfs
                        .rt
                        .borrow_mut()
                        .block_on(async { appfs.app.fetch_file(path).await.unwrap() }),
                );
                self.last_update = Instant::now();
            }
            InoData::Directory { children } => {
                if children.is_some() && self.last_update > Instant::now() - Duration::from_secs(15)
                {
                    // Cache directory listings for 15 seconds
                    return;
                }

                println!("Loading info for {:?}", path);
                if let DirectoryListingResponse::Found {
                    requested: _,
                    entries,
                } = appfs
                    .rt
                    .borrow_mut()
                    .block_on(async { appfs.app.fetch_dir(path).await.unwrap() })
                {
                    let mut v = Vec::new();
                    for entry in entries.iter() {
                        let child_ino = appfs.nodes.len() as u64;
                        let ino_entry = Arc::new(RefCell::new(Ino {
                            data: match entry {
                                FsEntry::File(_) => InoData::File { contents: None },
                                FsEntry::Directory(_) => InoData::Directory { children: None },
                            },
                            path: if self.path == "/" {
                                format!("/{}", entry.name())
                            } else {
                                format!("{}/{}", &self.path, entry.name())
                            },
                            name: entry.name().to_owned(),
                            ino: child_ino,
                            last_update: Instant::now(),
                        }));

                        appfs.nodes.push(ino_entry.clone());
                        v.push(ino_entry);
                    }

                    *children = Some(v);
                    self.last_update = Instant::now();
                    println!("{:?}", children);
                } else {
                    *children = None;
                }
            }
            InoData::Serial { pending_data } => {
                let mut buf = [0u8; 4096];
                let len = appfs.io.read(&mut buf);
                pending_data.push_bytes(&buf[0..len]);
            }
            InoData::Run => {}
        }
    }

    pub fn attr(&self) -> FileAttr {
        match &self.data {
            InoData::File { contents } => FileAttr {
                ino: self.ino,
                kind: FileType::RegularFile,
                nlink: 1,
                size: contents.as_ref().map(|x| x.len() as u64).unwrap_or(0),
                blocks: contents.as_ref().map(|x| x.len() as u64).unwrap_or(0) / 4096,
                ..default_attr()
            },
            InoData::Directory { children } => FileAttr {
                ino: self.ino,
                kind: FileType::Directory,
                perm: 0o755,
                nlink: children.as_ref().map(|x| x.len()).unwrap_or(0) as u32 + 1,
                ..default_attr()
            },
            InoData::Serial { pending_data: _ } => FileAttr {
                ino: self.ino,
                kind: FileType::RegularFile,
                nlink: 1,
                // Fake file size to make sure minicom and/or tail -f keep reading even though we're not returning full output
                size: 0xffffffff,
                ..default_attr()
            },
            InoData::Run => FileAttr {
                ino: self.ino,
                kind: FileType::RegularFile,
                nlink: 1,
                ..default_attr()
            },
        }
    }

    pub fn read(&mut self, offset: usize, size: usize, reply: fuse::ReplyData, _appfs: &mut AppFS) {
        match &mut self.data {
            InoData::File {
                contents: Some(contents),
            } => {
                let start = offset as usize;
                let end = (start + size as usize).min(contents.len());
                reply.data(&contents[start..end])
            }
            InoData::File { contents: _ } => {
                panic!("Called read() on an unloaded file node");
            }
            InoData::Directory { children: _ } => {
                error!("Trying to read from a directory");
                reply.error(EIO);
            }
            InoData::Serial { pending_data } => {
                let mut buf = vec![0u8; size];
                let len = pending_data.copy_to_slice(&mut buf);
                debug!(
                    "Read bytes from serial input: {:?}",
                    std::str::from_utf8(&buf[0..len])
                );
                if len == 0 {
                    reply.error(EAGAIN);
                } else {
                    reply.data(&buf[0..len]);
                }
            }
            InoData::Run => reply.data(&[]),
        }
    }

    pub fn write(&mut self, offset: usize, data: &[u8], appfs: &mut AppFS) -> Option<usize> {
        match &mut self.data {
            InoData::File {
                contents: Some(contents),
            } => {
                let start = offset as usize;
                let size = contents.len();
                let end = start + data.len();

                let mut new_data = contents.clone();

                new_data.resize(end.max(size), 0);
                new_data[start..end].copy_from_slice(data);

                let path = self.path.clone();

                match appfs
                    .rt
                    .borrow_mut()
                    .block_on(async { appfs.app.write_file(&path, &new_data).await })
                {
                    Ok(_) => {
                        *contents = new_data;
                        Some(data.len())
                    }
                    Err(e) => {
                        error!("Error writing file: {}", e);
                        None
                    }
                }
            }
            InoData::File { contents: _ } => {
                panic!("Called read() on an unloaded file node");
            }
            InoData::Directory { children: _ } => {
                error!("Trying to read from a directory");
                None
            }
            InoData::Serial { pending_data: _ } => match appfs
                .rt
                .borrow_mut()
                .block_on(async { appfs.app.serial_in(&data).await })
            {
                Ok(_) => Some(data.len()),
                Err(e) => {
                    error!("Error writing to serial: {}", e);
                    None
                }
            },
            InoData::Run => match appfs.rt.borrow_mut().block_on(async {
                appfs
                    .app
                    .run_file(String::from_utf8(data.into()).unwrap().trim_end())
                    .await
            }) {
                Ok(_) => Some(data.len()),
                Err(e) => {
                    error!("Error running app: {}", e);
                    None
                }
            },
        }
    }
}

impl<'a> AppFS<'a> {
    pub fn new(badge: Arc<Badge>, io: &'a Stream) -> AppFS<'a> {
        let flash = Arc::new(RefCell::new(Ino {
            ino: 2,
            last_update: Instant::now(),
            name: "flash".to_owned(),
            path: "/flash".to_owned(),
            data: InoData::Directory { children: None },
        }));
        let sdcard = Arc::new(RefCell::new(Ino {
            ino: 3,
            last_update: Instant::now(),
            name: "sdcard".to_owned(),
            path: "/sdcard".to_owned(),
            data: InoData::Directory { children: None },
        }));

        let serial = Arc::new(RefCell::new(Ino {
            ino: 4,
            last_update: Instant::now(),
            name: "serial".to_owned(),
            path: "/serial".to_owned(),
            data: InoData::Serial {
                pending_data: Buffer::new(),
            },
        }));

        let run = Arc::new(RefCell::new(Ino {
            ino: 5,
            last_update: Instant::now(),
            name: "run".to_owned(),
            path: "/run".to_owned(),
            data: InoData::Run,
        }));

        AppFS {
            app: badge,
            io,
            nodes: vec![
                Arc::new(RefCell::new(Ino::dir("ERROR", 1))),
                Arc::new(RefCell::new(Ino {
                    ino: 1,
                    last_update: Instant::now().add(Duration::from_secs(0xffff_ffff)),
                    name: "".to_owned(),
                    path: "/".to_owned(),
                    data: InoData::Directory {
                        children: Some(vec![
                            flash.clone(),
                            sdcard.clone(),
                            serial.clone(),
                            run.clone(),
                        ]),
                    },
                })),
                flash,
                sdcard,
                serial,
                run,
            ],
            rt: Arc::new(RefCell::new(Runtime::new().unwrap())),
        }
    }
}

impl<'a> Filesystem for AppFS<'a> {
    fn lookup(
        &mut self,
        _req: &fuse::Request,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: fuse::ReplyEntry,
    ) {
        info!("lookup({}, {:?})", parent, name);
        if let Some(entry) = self.nodes.get(parent as usize) {
            let entry = entry.clone();
            let entry = entry.borrow();
            match &entry.data {
                InoData::Directory {
                    children: Some(children),
                } => {
                    if let Some(child) = children
                        .iter()
                        .filter(|n| n.borrow().name.as_str() == name)
                        .next()
                    {
                        child.borrow_mut().ensure_data(self);
                        let child = child.borrow();
                        let result = child.attr();
                        debug!("Attr result: {:?}", result);
                        reply.entry(&TTL, &result, 0);
                    } else {
                        debug!("ENOENT: Node not found in children");
                        reply.error(ENOENT);
                    }
                }
                InoData::Directory { children: None } => {
                    panic!("Tried to lookup file in directory which was not loaded.");
                }
                _ => {
                    error!("Tried to load children of a non-directory");
                    reply.error(ENOENT);
                }
            }
        } else {
            debug!("ENOENT: Unknown ino");
            reply.error(ENOENT);
        }
    }

    fn forget(&mut self, _req: &fuse::Request, _ino: u64, _nlookup: u64) {
        info!("forget()");
    }

    fn getattr(&mut self, _req: &fuse::Request, ino: u64, reply: fuse::ReplyAttr) {
        info!("getattr({})", ino);
        if let Some(entry) = self.nodes.get(ino as usize) {
            let entry = entry.clone();
            entry.borrow_mut().ensure_data(self);
            reply.attr(&TTL, &entry.borrow().attr());
        } else {
            reply.error(ENOENT);
        }
    }

    fn mknod(
        &mut self,
        _req: &fuse::Request,
        parent: u64,
        name: &std::ffi::OsStr,
        _mode: u32,
        _rdev: u32,
        reply: fuse::ReplyEntry,
    ) {
        info!("mknod({}, {})", parent, name.to_str().unwrap());
        if let Some(entry) = self.nodes.get(parent as usize) {
            let name = name.to_str().unwrap();
            let path = format!("{}/{}", entry.borrow().path, name);
            match &mut entry.clone().borrow_mut().data {
                InoData::Directory { children } => {
                    let new_node = Arc::new(RefCell::new(Ino {
                        ino: self.nodes.len() as u64,
                        path: path.clone(),
                        name: name.to_owned(),
                        data: InoData::File { contents: None },
                        last_update: Instant::now(),
                    }));

                    match self
                        .rt
                        .borrow_mut()
                        .block_on(async { self.app.create_file(path).await })
                    {
                        Ok(_) => {
                            if let Some(children) = children {
                                children.push(new_node.clone());
                            }

                            reply.entry(
                                &TTL,
                                &FileAttr {
                                    ino: new_node.borrow().ino,
                                    kind: FileType::RegularFile,
                                    nlink: 1,
                                    ..default_attr()
                                },
                                0,
                            );

                            self.nodes.push(new_node.clone());
                        }
                        Err(e) => {
                            error!("Error creating file: {}", e);
                            reply.error(EIO);
                        }
                    }
                }
                _ => {
                    error!("Tried to mknod on a non-directory");
                    reply.error(ENOENT)
                }
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn mkdir(
        &mut self,
        _req: &fuse::Request,
        parent: u64,
        name: &std::ffi::OsStr,
        _mode: u32,
        reply: fuse::ReplyEntry,
    ) {
        info!("mkdir({}, {})", parent, name.to_str().unwrap());
        if let Some(entry) = self.nodes.get(parent as usize) {
            let name = name.to_str().unwrap();
            let path = format!("{}/{}", entry.borrow().path, name);
            match &mut entry.clone().borrow_mut().data {
                InoData::Directory { children } => {
                    let new_node = Arc::new(RefCell::new(Ino {
                        ino: self.nodes.len() as u64,
                        path: path.clone(),
                        name: name.to_owned(),
                        last_update: Instant::now(),
                        data: InoData::Directory {
                            children: Some(Vec::new()),
                        },
                    }));

                    match self
                        .rt
                        .borrow_mut()
                        .block_on(async { self.app.create_dir(path).await })
                    {
                        Ok(_) => {
                            if let Some(children) = children {
                                children.push(new_node.clone());
                            }

                            reply.entry(&TTL, &new_node.borrow().attr(), 0);
                            self.nodes.push(new_node.clone());
                        }
                        Err(e) => {
                            error!("Error creating directory: {}", e);
                            reply.error(EIO);
                        }
                    }
                }
                _ => {
                    error!("mkdir on a non-directory");
                    reply.error(ENOENT);
                }
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn unlink(
        &mut self,
        _req: &fuse::Request,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: fuse::ReplyEmpty,
    ) {
        info!("unlink({}, {})", parent, name.to_str().unwrap());
        if let Some(entry) = self.nodes.get(parent as usize) {
            let path = format!("{}/{}", entry.borrow().path, name.to_str().unwrap());
            info!("Unlinking {}", path);
            match &mut entry.borrow_mut().data {
                InoData::Directory { children } => {
                    match self
                        .rt
                        .borrow_mut()
                        .block_on(async { self.app.delete_path(&path).await })
                    {
                        Ok(_) => {
                            if let Some(children) = children {
                                children.retain(|item| item.borrow().path != path);
                            }

                            reply.ok()
                        }
                        Err(e) => {
                            error!("Error deleting file: {}", e);
                            reply.error(EIO);
                        }
                    }
                }
                _ => {
                    error!("Tried to unlink a file inside a non-directory");
                    reply.error(ENOENT);
                }
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn rmdir(
        &mut self,
        _req: &fuse::Request,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: fuse::ReplyEmpty,
    ) {
        info!("rmdir({}, {})", parent, name.to_str().unwrap());
        if let Some(entry) = self.nodes.get(parent as usize) {
            let path = format!("{}/{}", entry.borrow().path, name.to_str().unwrap());
            match &mut entry.borrow_mut().data {
                InoData::Directory { children } => {
                    match self
                        .rt
                        .borrow_mut()
                        .block_on(async { self.app.delete_path(&path).await })
                    {
                        Ok(_) => {
                            if let Some(children) = children {
                                children.retain(|item| item.borrow().path != path);
                            }
                            reply.ok()
                        }
                        Err(e) => {
                            error!("Error deleting directory: {}", e);
                            reply.error(EIO);
                        }
                    }
                }
                _ => {
                    error!("rmdir on a non-directory");
                    reply.error(ENOENT);
                }
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn rename(
        &mut self,
        _req: &fuse::Request,
        parent: u64,
        name: &std::ffi::OsStr,
        newparent: u64,
        newname: &std::ffi::OsStr,
        reply: fuse::ReplyEmpty,
    ) {
        info!("rename({}, {})", parent, name.to_str().unwrap());
        if let (Some(from), Some(to)) = (
            self.nodes.get(parent as usize),
            self.nodes.get(newparent as usize),
        ) {
            let from_path = format!("{}/{}", from.borrow().path, name.to_str().unwrap());
            let to_path = format!("{}/{}", to.borrow().path, newname.to_str().unwrap());
            match (&mut from.borrow_mut().data, &mut to.borrow_mut().data) {
                (
                    InoData::Directory {
                        children: from_children,
                    },
                    InoData::Directory {
                        children: to_children,
                    },
                ) => {
                    match self
                        .rt
                        .borrow_mut()
                        .block_on(async { self.app.move_file(&from_path, &to_path).await })
                    {
                        Ok(_) => {
                            if let Some(from_children) = from_children {
                                if let Some(to_children) = to_children {
                                    let item = from_children
                                        .iter()
                                        .filter(|item| item.borrow().path == from_path)
                                        .next()
                                        .unwrap()
                                        .clone();
                                    item.borrow_mut().path = to_path.clone();
                                    item.borrow_mut().name = newname.to_str().unwrap().to_owned();
                                    to_children.push(item);
                                }

                                from_children.retain(|item| item.borrow().path != from_path);
                            }

                            reply.ok()
                        }
                        Err(e) => {
                            error!("Error deleting file: {}", e);
                            reply.error(EIO);
                        }
                    }
                }
                _ => {
                    error!("Rename where one of the parents isn't a directory");
                }
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn open(&mut self, _req: &fuse::Request, ino: u64, _flags: u32, reply: fuse::ReplyOpen) {
        info!("open()");
        if let Some(_) = self.nodes.get(ino as usize) {
            reply.opened(0, 0);
        } else {
            reply.error(ENOENT);
        }
    }

    fn read(
        &mut self,
        _req: &fuse::Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        reply: fuse::ReplyData,
    ) {
        info!("read({}, .., {}, {})", ino, offset, size);
        if let Some(entry) = self.nodes.get(ino as usize) {
            let entry = entry.clone();
            let mut entry = entry.borrow_mut();
            entry.ensure_data(self);
            entry.read(offset as usize, size as usize, reply, self);
        } else {
            reply.error(ENOENT);
        }
    }

    fn write(
        &mut self,
        _req: &fuse::Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _flags: u32,
        reply: fuse::ReplyWrite,
    ) {
        info!("write({}, {}, {:?})", ino, offset, data);
        if let Some(entry) = self.nodes.get(ino as usize) {
            let entry = entry.clone();
            let mut entry = entry.borrow_mut();
            entry.ensure_data(self);

            if let Some(size) = entry.write(offset as usize, data, self) {
                reply.written(size as u32);
            } else {
                error!("Error writing file!");
                reply.error(EIO);
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn flush(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuse::ReplyEmpty,
    ) {
        info!("flush()");
        reply.error(ENOSYS);
    }

    fn release(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
        reply: fuse::ReplyEmpty,
    ) {
        info!("release()");
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: fuse::ReplyEmpty,
    ) {
        info!("fsync()");
        reply.error(ENOSYS);
    }

    fn opendir(&mut self, _req: &fuse::Request, ino: u64, _flags: u32, reply: fuse::ReplyOpen) {
        info!(
            "opendir({} = {:?})",
            ino,
            self.nodes
                .get(ino as usize)
                .map(|n| n.borrow().path.clone())
                .unwrap_or("<unknown>".to_owned())
        );
        reply.opened(0, 0);
    }

    fn readdir(
        &mut self,
        _req: &fuse::Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: fuse::ReplyDirectory,
    ) {
        info!("readdir(.., {}, .., {})", ino, offset);
        if let Some(parent_entry) = self.nodes.get(ino as usize) {
            let parent_entry = parent_entry.borrow();
            match &parent_entry.data {
                InoData::Directory { children } => {
                    if let Some(children) = &children {
                        if offset < 1 {
                            reply.add(ino, 1, FileType::Directory, ".");
                        }
                        if offset < 2 {
                            reply.add(ino, 2, FileType::Directory, "..");
                        }

                        for (offset, entry) in children
                            .iter()
                            .enumerate()
                            .skip(offset.checked_sub(2).unwrap_or(0) as usize)
                            .map(|(x, e)| (x as i64 + 3, e))
                        {
                            let entry = entry.borrow();
                            debug!("Adding child {} to response", entry.path);
                            // ! TODO: Duplicate FileType mapping
                            if reply.add(
                                entry.ino,
                                offset,
                                match entry.data {
                                    InoData::File { contents: _ } => FileType::RegularFile,
                                    InoData::Directory { children: _ } => FileType::Directory,
                                    InoData::Serial { pending_data: _ } => FileType::RegularFile,
                                    InoData::Run => FileType::RegularFile,
                                },
                                &entry.name,
                            ) {
                                break;
                            }
                        }

                        reply.ok()
                    } else {
                        reply.error(ENOENT)
                    }
                }
                _ => {
                    error!("Tried to readdir() on a non-directory");
                    reply.error(ENOENT);
                }
            }
        }
    }

    fn releasedir(
        &mut self,
        _req: &fuse::Request,
        ino: u64,
        fh: u64,
        _flags: u32,
        reply: fuse::ReplyEmpty,
    ) {
        info!("releasedir({}, {})", ino, fh);
        reply.ok();
    }

    fn statfs(&mut self, _req: &fuse::Request, _ino: u64, reply: fuse::ReplyStatfs) {
        info!("statfs()");
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }

    fn setxattr(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _name: &std::ffi::OsStr,
        _value: &[u8],
        _flags: u32,
        _position: u32,
        reply: fuse::ReplyEmpty,
    ) {
        info!("setxattr()");
        reply.error(ENOSYS);
    }

    fn getxattr(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _name: &std::ffi::OsStr,
        _size: u32,
        reply: fuse::ReplyXattr,
    ) {
        info!("getxattr()");
        reply.error(ENOSYS);
    }

    fn listxattr(&mut self, _req: &fuse::Request, _ino: u64, _size: u32, reply: fuse::ReplyXattr) {
        info!("listxattr()");
        reply.error(ENOSYS);
    }

    fn removexattr(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _name: &std::ffi::OsStr,
        reply: fuse::ReplyEmpty,
    ) {
        info!("removexattr()");
        reply.error(ENOSYS);
    }

    fn access(&mut self, _req: &fuse::Request, _ino: u64, _mask: u32, reply: fuse::ReplyEmpty) {
        info!("access()");
        reply.error(ENOSYS);
    }

    fn create(
        &mut self,
        _req: &fuse::Request,
        _parent: u64,
        _name: &std::ffi::OsStr,
        _mode: u32,
        _flags: u32,
        reply: fuse::ReplyCreate,
    ) {
        info!("create()");
        reply.error(ENOSYS);
    }
    fn init(&mut self, _req: &fuse::Request) -> Result<(), libc::c_int> {
        Ok(())
    }

    fn destroy(&mut self, _req: &fuse::Request) {}

    fn setattr(
        &mut self,
        _req: &fuse::Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<Timespec>,
        _mtime: Option<Timespec>,
        _fh: Option<u64>,
        _crtime: Option<Timespec>,
        _chgtime: Option<Timespec>,
        _bkuptime: Option<Timespec>,
        _flags: Option<u32>,
        reply: fuse::ReplyAttr,
    ) {
        info!("setattr({}, .., size={:?})", ino, size);
        if let Some(node) = self.nodes.get(ino as usize) {
            let node = node.clone();
            let mut node = node.borrow_mut();
            let path = node.path.clone();
            node.ensure_data(self);
            match &mut node.data {
                InoData::File {
                    contents: Some(contents),
                } => {
                    if let Some(new_size) = size {
                        let result = self
                            .rt
                            .borrow_mut()
                            .block_on(async {
                                self.app
                                    .write_file(path, &contents[0..new_size as usize])
                                    .await
                            })
                            .map(|x| x);
                        match result {
                            Ok(_) => {
                                contents.resize(new_size as usize, 0);
                                drop(contents);
                                reply.attr(&TTL, &node.attr());
                            }
                            Err(e) => {
                                error!("Error deleting directory: {}", e);
                                reply.error(EIO);
                            }
                        }
                    } else {
                        reply.attr(&TTL, &node.attr());
                    }
                }
                InoData::File { contents: _ } => {
                    unreachable!();
                }
                InoData::Directory { children: _ } => {
                    info!("setattr on directory ignored");
                    reply.attr(&TTL, &node.attr());
                }
                InoData::Serial { pending_data: _ } => {
                    info!("setattr on serial ignored");
                    reply.attr(&TTL, &node.attr());
                }
                InoData::Run => {
                    info!("setattr on run ignored");
                    reply.attr(&TTL, &node.attr());
                }
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn readlink(&mut self, _req: &fuse::Request, _ino: u64, reply: fuse::ReplyData) {
        info!("readlink()");
        reply.error(ENOSYS);
    }

    fn symlink(
        &mut self,
        _req: &fuse::Request,
        _parent: u64,
        _name: &std::ffi::OsStr,
        _link: &std::path::Path,
        reply: fuse::ReplyEntry,
    ) {
        info!("symlink()");
        reply.error(ENOSYS);
    }

    fn link(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _newparent: u64,
        _newname: &std::ffi::OsStr,
        reply: fuse::ReplyEntry,
    ) {
        info!("link()");
        reply.error(ENOSYS);
    }

    fn fsyncdir(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: fuse::ReplyEmpty,
    ) {
        info!("fsyncdir()");
        reply.error(ENOSYS);
    }

    fn getlk(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _typ: u32,
        _pid: u32,
        reply: fuse::ReplyLock,
    ) {
        info!("getlk()");
        reply.error(ENOSYS);
    }

    fn setlk(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _typ: u32,
        _pid: u32,
        _sleep: bool,
        reply: fuse::ReplyEmpty,
    ) {
        info!("setlk()");
        reply.error(ENOSYS);
    }

    fn bmap(
        &mut self,
        _req: &fuse::Request,
        _ino: u64,
        _blocksize: u32,
        _idx: u64,
        reply: fuse::ReplyBmap,
    ) {
        info!("bmap()");
        reply.error(ENOSYS);
    }
}
