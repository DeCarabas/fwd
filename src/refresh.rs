use crate::message::PortDesc;
use procfs::process::FDTarget;
use std::collections::HashMap;

pub fn get_entries() -> procfs::ProcResult<Vec<PortDesc>> {
    let all_procs = procfs::process::all_processes()?;

    // build up a map between socket inodes and process stat info. Ignore any
    // error we encounter as it probably means we have no access to that
    // process or something.
    let mut map: HashMap<u64, String> = HashMap::new();
    for p in all_procs {
        if let Ok(process) = p {
            if !process.is_alive() {
                continue; // Ignore zombies.
            }

            if let (Ok(fds), Ok(cmd)) = (process.fd(), process.cmdline()) {
                for fd in fds {
                    if let Ok(fd) = fd {
                        if let FDTarget::Socket(inode) = fd.target {
                            map.insert(inode, cmd.join(" "));
                        }
                    }
                }
            }
        }
    }

    let mut h: HashMap<u16, PortDesc> = HashMap::new();

    // Go through all the listening IPv4 and IPv6 sockets and take the first
    // instance of listening on each port *if* the address is loopback or
    // unspecified. (TODO: Do we want this restriction really?)
    let tcp = procfs::net::tcp()?;
    let tcp6 = procfs::net::tcp6()?;
    for tcp_entry in tcp.into_iter().chain(tcp6) {
        if tcp_entry.state == procfs::net::TcpState::Listen
            && (tcp_entry.local_address.ip().is_loopback()
                || tcp_entry.local_address.ip().is_unspecified())
            && !h.contains_key(&tcp_entry.local_address.port())
        {
            if let Some(cmd) = map.get(&tcp_entry.inode) {
                h.insert(
                    tcp_entry.local_address.port(),
                    PortDesc {
                        port: tcp_entry.local_address.port(),
                        desc: cmd.clone(),
                    },
                );
            }
        }
    }

    Ok(h.into_values().collect())
}
