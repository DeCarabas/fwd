use crate::message::PortDesc;
use anyhow::Result;
#[cfg(target_os = "linux")]
use std::collections::HashMap;

#[cfg(not(target_os = "linux"))]
pub async fn get_entries() -> Result<Vec<PortDesc>> {
    use anyhow::bail;
    bail!("Not supported on this operating system");
}

#[cfg(target_os = "linux")]
pub async fn get_entries() -> Result<Vec<PortDesc>> {
    let start = std::time::Instant::now();
    use procfs::process::FDTarget;
    use std::collections::HashMap;

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
    log::trace!("procfs elapsed={:?}", start.elapsed());

    let mut h = if let Ok(ports) = find_docker_ports().await {
        ports
    } else {
        HashMap::new()
    };

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

    let vals = h.into_values().collect();
    log::trace!("total portscan elapsed={:?}", start.elapsed());
    Ok(vals)
}

#[cfg(target_os = "linux")]
async fn find_docker_ports(
) -> Result<HashMap<u16, PortDesc>, bollard::errors::Error> {
    use bollard::container::ListContainersOptions;
    use bollard::Docker;

    let start = std::time::Instant::now();
    let client = Docker::connect_with_defaults()?;
    log::trace!("docker connect elapsed={:?}", start.elapsed());

    let port_start = std::time::Instant::now();
    let mut port_to_name = HashMap::new();
    let opts: ListContainersOptions<String> =
        ListContainersOptions { all: false, ..Default::default() };
    for container in client.list_containers(Some(opts)).await? {
        let name = container
            .names
            .into_iter()
            .flatten()
            .next()
            .unwrap_or_else(|| "<unknown docker>".to_owned());
        for port in container.ports.iter().flatten() {
            if let Some(public_port) = port.public_port {
                let private_port = port.private_port;
                port_to_name.insert(
                    public_port,
                    PortDesc {
                        port: public_port,
                        desc: format!("{name} (docker->{private_port})"),
                    },
                );
            }
        }
    }
    log::trace!(
        "docker port elapsed={:?} total docker elapsed={:?}",
        port_start.elapsed(),
        start.elapsed()
    );
    Ok(port_to_name)
}
