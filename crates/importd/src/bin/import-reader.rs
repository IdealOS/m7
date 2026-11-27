//! `import-reader`: reads music tags and picture sizes for importd, sandboxed.
//!
//! importd starts it with one end of a socket pair as its standard input and
//! sends it open files to read (see `importd::reader`). It opens nothing itself:
//! its sandbox lets it open no files and make no sockets, as a user that owns
//! nothing. It reads until importd closes the socket, then exits.

use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;

fn main() {
    // SAFETY: importd made standard input a socket for this process alone.
    let sock = unsafe { UnixStream::from_raw_fd(0) };
    let mut message = [0u8; 64];
    loop {
        let (n, fd) = match importd::reader::recv_fd(&sock, &mut message) {
            Ok(Some(got)) => got,
            Ok(None) => return,
            Err(e) => {
                eprintln!("import-reader: {e}");
                std::process::exit(1);
            }
        };
        let mut file = std::fs::File::from(fd);
        let (reply, artwork) = importd::reader::answer(&message[..n], &mut file);
        drop(file);
        if let Err(e) = importd::reader::write_reply(&sock, &reply, &artwork) {
            eprintln!("import-reader: {e}");
            std::process::exit(1);
        }
    }
}
