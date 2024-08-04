use anyhow::Result;
use log::{error, warn};
use std::collections::HashMap;

use crate::message::PortDesc;

#[cfg(target_os = "linux")]
mod procfs;

pub fn get_entries() -> Result<Vec<PortDesc>> {
    #[allow(unused)]
    let mut attempts = 0;

    let mut result: HashMap<u16, PortDesc> = HashMap::new();

    #[cfg(target_os = "linux")]
    {
        attempts += 1;
        match procfs::get_entries() {
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
