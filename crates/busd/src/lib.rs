//! `busd`: the message bus broker.
//!
//! Services and apps exchange requests and pub/sub messages through it over a Unix
//! socket, in the length-prefixed JSON frames `protocol::bus` defines. The broker
//! stamps every message with its sender, routes requests to the connection that
//! claimed the name they're addressed to, and passes everything through a
//! permission hook ([`router::Policy`]); busd's is [`policy::Grants`], which
//! follows the grants permd publishes.

pub mod policy;
pub mod router;
pub mod server;
pub mod testing;

pub use router::{AllowAll, Policy};
pub use server::serve;

/// Raise this process's open-descriptor limit to its hard limit (at most 65,536)
/// and return the new soft limit. The broker holds every connection plus the
/// descriptors in flight on them, and the usual default soft limit is 1,024.
pub fn raise_fd_limit() -> std::io::Result<u64> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit and setrlimit with a valid struct.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let want = lim.rlim_max.min(65_536);
        if lim.rlim_cur < want {
            lim.rlim_cur = want;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(lim.rlim_cur)
}
