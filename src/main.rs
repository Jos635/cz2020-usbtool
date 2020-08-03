use cmds::{DirectoryListingResponse, FsEntry};
use crossbeam::scope;
use device::{Badge, Device};
use fs::AppFS;
use log::{info, warn};
use std::{
    error::Error,
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use stream::Stream;
use structopt::StructOpt;
use termios::{tcsetattr, Termios, ECHO, ICANON, TCSANOW};
use tokio::runtime::Runtime;

mod cmds;
mod device;
mod fs;
mod stream;

#[derive(StructOpt, Clone)]
#[structopt(
    name = "cz2020-usbtool",
    about = "Communicate with the CampZone 2020 badge without using Chrome."
)]
enum Args {
    #[structopt(about = "Lists all files available on the badge one-by-one")]
    Tree,

    #[structopt(about = "Lists all files in the specified directory")]
    Ls { path: String },

    #[structopt(about = "Fetches the specified file")]
    Get { path: String },

    #[structopt(about = "Writes stdin to the specified file")]
    Set { path: String },

    #[structopt(about = "Creates a new file")]
    CreateFile { path: String },

    #[structopt(about = "Creates a new directory")]
    CreateDir { path: String },

    #[structopt(about = "Deletes the specified path")]
    Rm { path: String },

    #[structopt(about = "Copies a file to another file")]
    Cp { from: String, to: String },

    #[structopt(about = "Moves a file from one location to another")]
    Mv {
        #[structopt(help = "The original file location")]
        from: String,

        #[structopt(about = "The new file location. The filename itself must be included.")]
        to: String,
    },

    #[structopt(about = "Runs an app")]
    Run {
        #[structopt(
            about = "The path to the __init__.py file. Don't prefix the path with /flash."
        )]
        path: String,
    },

    #[structopt(
        about = "Opens the serial connection for the Python shell on the badge. Input from standard in is written to the device."
    )]
    Shell,

    #[structopt(about = "Mounts the filesystem of the badge to a directory using libfuse")]
    Mount { path: String },
}

pub async fn tree(badge: &Badge) -> Result<(), Box<dyn Error>> {
    let mut stack = vec![
        ("".to_owned(), FsEntry::Directory("flash".to_owned())),
        ("".to_owned(), FsEntry::Directory("sd".to_owned())),
    ];

    while let Some((base, entry)) = stack.pop() {
        let new_base = format!("{}/{}", base, entry.name());
        println!("{}", new_base);
        match entry {
            FsEntry::Directory(_) => {
                let items = badge.fetch_dir(&new_base).await?;

                if let DirectoryListingResponse::Found {
                    requested: _,
                    entries,
                } = items
                {
                    stack.extend(entries.into_iter().map(|x| (new_base.clone(), x)));
                }
            }
            _ => {}
        }
    }

    Ok(())
}

static PRINT_STDOUT: AtomicBool = AtomicBool::new(false);

fn main() {
    env_logger::init();

    let context = rusb::Context::new().unwrap();
    let device = Device::new(&context).unwrap();

    let badge = Arc::new(Badge::new(device));
    let b2 = badge.clone();
    let b3 = badge.clone();
    let io = Stream::new();
    let ioref = &io;

    scope(|s| {
        let j = s.spawn(move |_| {
            b2.run(|text| {
                // replace().replace() to fix missing '\r's from some of the output, but not all
                ioref.write(text.replace("\r\n", "\n").replace("\n", "\r\n").as_bytes());

                if PRINT_STDOUT.load(Ordering::Relaxed) {
                    print!("{}", text);
                    std::io::stdout().flush().unwrap();
                }
            });
        });

        let args = Args::from_args();
        match args {
            Args::Mount { path } => {
                fuse::mount(AppFS::new(badge, &io), &path, &[]).unwrap();
            }
            args => {
                let mut rt = Runtime::new().unwrap();
                rt.block_on(async {
                    run(args, badge).await.unwrap();
                });
            }
        }

        info!("Terminating threads...");
        b3.close();
        j.join().unwrap();
    })
    .unwrap();
}

async fn run<'a>(args: Args, badge: Arc<Badge>) -> Result<(), Box<dyn Error>> {
    badge.heartbeat().await?;

    std::thread::sleep(Duration::from_millis(500));

    match args {
        Args::Ls { path } => {
            let entries = badge.fetch_dir(path).await?;
            if let DirectoryListingResponse::Found {
                requested: _,
                entries,
            } = entries
            {
                for entry in entries {
                    println!("{}", entry.name());
                }
            } else {
                println!("Unable to load directory");
            }
        }
        Args::Tree => tree(&badge).await?,
        Args::Get { path } => std::io::stdout().write_all(&badge.fetch_file(path).await?)?,
        Args::Set { path } => {
            let mut data = Vec::new();
            std::io::stdin().lock().read_to_end(&mut data)?;
            badge.write_file(path, data).await?;
        }
        Args::CreateFile { path } => badge.create_file(path).await?,
        Args::CreateDir { path } => badge.create_dir(path).await?,
        Args::Rm { path } => badge.delete_path(path).await?,
        Args::Cp { from, to } => badge.copy_file(from, to).await?,
        Args::Mv { from, to } => badge.move_file(from, to).await?,
        Args::Run { path } => {
            if path.starts_with("/flash") {
                warn!("You should use the run command without `/flash` prefix. I.e. instead of `run /flash/apps/synthesizer/__init__.py` do `run /apps/synthesizer/__init__.py`");
            }

            badge.run_file(path).await?
        }
        Args::Shell => {
            PRINT_STDOUT.store(true, Ordering::Relaxed);

            // Send a Control + C to terminate any previous command that might have been running
            badge.serial_in("\u{003}".as_bytes()).await?;

            let mut buf = [0u8; 1];
            let stdin = libc::STDIN_FILENO;

            let mut termios = Termios::from_fd(stdin).unwrap();
            // Make sure the terminal doesn't print keys and that we can read keys one-by-one
            termios.c_lflag &= !(ICANON | ECHO);
            tcsetattr(stdin, TCSANOW, &mut termios).unwrap();
            let mut reader = std::io::stdin();

            while let Ok(_) = reader.read_exact(&mut buf) {
                if buf[0] == '\n' as u8 {
                    badge.serial_in("\r\n".as_bytes()).await?;
                } else {
                    badge.serial_in(&buf).await?;
                }
            }
        }
        Args::Mount { path: _ } => unreachable!("Handled in main()"),
    }

    Ok(())
}
