use anyhow::Result;
use procfs::process::FDTarget;
use std::collections::HashMap;

use crate::message::PortDesc;

pub fn get_entries(send_anonymous: bool) -> Result<HashMap<u16, PortDesc>> {
    let all_procs = procfs::process::all_processes()?;

    // build up a map between socket inodes and process stat info. Ignore any
    // error we encounter as it probably means we have no access to that
    // process or something.
    let mut map: HashMap<u64, String> = HashMap::new();
    for process in all_procs.flatten() {
        if !process.is_alive() {
            continue; // Ignore zombies.
        }

        if let (Ok(fds), Ok(cmd)) = (process.fd(), process.cmdline()) {
            for fd in fds.flatten() {
                if let FDTarget::Socket(inode) = fd.target {
                    map.insert(inode, cmd.join(" "));
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
            // If the process is not one that we can identify, then we return
            // the port but leave the description empty so that it can be
            // identified by the client as "anonymous".
            let desc = if let Some(cmd) = map.get(&tcp_entry.inode) {
                cmd.clone()
            } else {
                String::new()
            };

            if send_anonymous || !desc.is_empty() {
                h.insert(
                    tcp_entry.local_address.port(),
                    PortDesc {
                        port: tcp_entry.local_address.port(),
                        desc,
                    },
                );
            }
        }
    }

    Ok(h)
}
