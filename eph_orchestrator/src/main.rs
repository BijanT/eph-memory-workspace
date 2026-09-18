mod qmp;
mod vm_detection;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;

#[allow(dead_code)]
const EPH_MEM_DONATION_GRANULARITY: u64 = 256 * 1024 * 1024; // 256 MiB
const QMP_DIRECTORY: &str = "/tmp/ephmem/";

#[allow(dead_code)]
struct DonorVM {
    /* The path to the QMP socket */
    qmp_socket_path: PathBuf,
    /* The Unix stream for communicating with the donor VM */
    stream: LineStream<UnixStream>,
    /* The list of donatable regions available in the donor VM */
    donatable_regions: Vec<DonatableRegion>,
}

#[allow(dead_code)]
struct DonatableRegion {
    /* The total memory available in the memory region */
    size: u64,
    /* The amount of memory that has been donated from this region */
    donated: u64,
    /* The path to the QEMU memory backend device for this donatable region */
    path: String,
}

struct LineStream<T: Read + Write> {
    reader: BufReader<T>,
}

impl<T: Read + Write> LineStream<T> {
    pub fn new(stream: T) -> Self {
        Self {
            reader: BufReader::new(stream),
        }
    }

    pub fn send(&mut self, message: &str) -> std::io::Result<()> {
        self.reader.get_mut().write_all(message.as_bytes())?;
        if !message.ends_with('\n') {
            self.reader.get_mut().write_all(b"\n")?;
        }
        Ok(())
    }

    pub fn recv_line(&mut self) -> std::io::Result<String> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        Ok(line)
    }
}

fn main() {
    println!("Starting eph_orchestrator...");

    let donors = Mutex::new(Vec::new());

    thread::scope(|s| {
        s.spawn(|| {
            if let Err(e) = vm_detection::vm_detection_thread(&donors, QMP_DIRECTORY) {
                eprintln!("Error in VM detection thread: {}", e);
            }
        });
    });
}
