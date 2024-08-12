use anyhow::Result;
#[cfg_attr(not(target_os = "linux"), allow(unused))]
use log::error;
use log::warn;
use std::collections::HashMap;

use crate::message::PortDesc;

#[cfg(target_os = "linux")]
mod procfs;

#[cfg(unix)]
pub mod docker;

pub async fn get_entries(_send_anonymous: bool) -> Result<Vec<PortDesc>> {
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut attempts = 0;

    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut result: HashMap<u16, PortDesc> = HashMap::new();

    #[cfg(unix)]
    {
        attempts += 1;
        match docker::get_entries().await {
            Ok(m) => {
                for (p, d) in m {
                    result.entry(p).or_insert(d);
                }
            }
            Err(e) => error!("Error reading from docker: {e:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    {
        attempts += 1;
        match procfs::get_entries(_send_anonymous) {
            Ok(m) => {
                for (p, d) in m {
                    result.entry(p).or_insert(d);
                }
            }
            Err(e) => error!("Error reading procfs: {e:?}"),
        }
    }

    if attempts == 0 {
        warn!("Port scanning is not supported for this server");
    }

    Ok(result.into_values().collect())
}
