use anyhow::{bail, Result};
use std::collections::HashMap;
use toml::Value;

#[derive(Debug, Clone)]
pub struct PortConfig {
    pub enabled: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    auto: bool,
    ports: HashMap<u16, PortConfig>,
}

impl ServerConfig {
    pub fn contains_key(&self, port: u16) -> bool {
        self.ports.contains_key(&port)
    }

    pub fn get(&self, port: u16) -> PortConfig {
        match self.ports.get(&port) {
            None => PortConfig { enabled: self.auto, description: None },
            Some(c) => c.clone(),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    auto: bool,
    servers: HashMap<String, ServerConfig>,
}

impl Config {
    pub fn get(&self, remote: &str) -> ServerConfig {
        match self.servers.get(remote) {
            Some(cfg) => cfg.clone(),
            None => ServerConfig { auto: self.auto, ports: HashMap::new() },
        }
    }
}

pub fn load_config() -> Result<Config> {
    use std::io::ErrorKind;

    let mut home = match home::home_dir() {
        Some(h) => h,
        None => return Ok(default()),
    };
    home.push(".fwd");

    let contents = match std::fs::read_to_string(home) {
        Ok(contents) => contents,
        Err(e) => match e.kind() {
            ErrorKind::NotFound => return Ok(default()),
            _ => return Err(e.into()),
        },
    };

    Ok(parse_config(&contents.parse::<Value>()?)?)
}

fn default() -> Config {
    Config { auto: true, servers: HashMap::new() }
}

fn parse_config(value: &Value) -> Result<Config> {
    match value {
        Value::Table(table) => Ok({
            let auto = match table.get("auto") {
                None => true,
                Some(Value::Boolean(v)) => *v,
                Some(v) => bail!("expected a true or false, got {:?}", v),
            };
            Config {
                auto,
                servers: get_servers(&table, auto)?,
            }
        }),
        _ => bail!("top level must be a table"),
    }
}

fn get_servers(
    table: &toml::value::Table,
    auto: bool,
) -> Result<HashMap<String, ServerConfig>> {
    match table.get("servers") {
        None => Ok(HashMap::new()),
        Some(Value::Table(table)) => Ok({
            let mut servers = HashMap::new();
            for (k, v) in table {
                servers.insert(k.clone(), get_server(v, auto)?);
            }
            servers
        }),
        v => bail!("expected a table in the servers key, got {:?}", v),
    }
}

fn get_server(value: &Value, auto: bool) -> Result<ServerConfig> {
    match value {
        Value::Table(table) => Ok(ServerConfig {
            auto: match table.get("auto") {
                None => auto, // Default to global default
                Some(Value::Boolean(v)) => *v,
                Some(v) => bail!("expected true or false, got {:?}", v),
            },
            ports: get_ports(table)?,
        }),
        value => bail!("expected a table, got {:?}", value),
    }
}

fn get_ports(table: &toml::value::Table) -> Result<HashMap<u16, PortConfig>> {
    match table.get("ports") {
        None => Ok(HashMap::new()),
        Some(Value::Table(table)) => Ok({
            let mut ports = HashMap::new();
            for (k,v) in table {
                let port:u16 = k.parse()?;
                let config = match v {
                    Value::Boolean(enabled) => PortConfig{enabled:*enabled, description:None},
                    Value::Table(table) => PortConfig{
                        enabled: match table.get("enabled") {
                            Some(Value::Boolean(enabled)) => *enabled,
                            _ => bail!("not implemented"),
                        },
                        description: match table.get("description") {
                            Some(Value::String(desc)) => Some(desc.clone()),
                            Some(v) => bail!("expect a string description, got {:?}", v),
                            None => None,
                        },
                    },
                    _ => bail!("expected either a boolean (enabled) or a table for a port config, got {:?}", v),
                };
                ports.insert(port, config);
            }
            ports
        }),
        Some(Value::Array(array)) => Ok({
            let mut ports = HashMap::new();
            for v in array {
                ports.insert(get_port_number(v)?, PortConfig{enabled:true, description:None});
            }
            ports
        }),
        Some(v) => bail!("ports must be either a table of '<port> = ...' or an array of ports, got {:?}", v),
    }
}

fn get_port_number(v: &Value) -> Result<u16> {
    let port: u16 = match v {
        Value::Integer(i) => (*i).try_into()?,
        v => bail!("port must be a small number, got {:?}", v),
    };
    Ok(port)
}
