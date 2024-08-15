use anyhow::{bail, Result};
use std::collections::hash_map;
use std::collections::HashMap;
use toml::value::{Table, Value};

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct PortConfig {
    pub enabled: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct ServerConfig {
    auto: bool,
    ports: HashMap<u16, PortConfig>,
}

impl ServerConfig {
    #[cfg(test)]
    pub fn default() -> ServerConfig {
        ServerConfig { auto: true, ports: HashMap::new() }
    }

    #[cfg(test)]
    pub fn set_auto(&mut self, auto: bool) {
        self.auto = auto;
    }

    #[cfg(test)]
    pub fn insert(&mut self, port: u16, config: PortConfig) {
        self.ports.insert(port, config);
    }

    pub fn auto(&self) -> bool {
        self.auto
    }

    pub fn iter(&self) -> hash_map::Iter<u16, PortConfig> {
        self.ports.iter()
    }

    pub fn contains_key(&self, port: u16) -> bool {
        self.ports.contains_key(&port)
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
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

    let Some(directories) = directories_next::ProjectDirs::from("", "", "fwd")
    else {
        return Ok(default());
    };

    let mut config_path = directories.config_dir().to_path_buf();
    config_path.push("config.toml");

    let contents = match std::fs::read_to_string(config_path) {
        Ok(contents) => contents,
        Err(e) => match e.kind() {
            ErrorKind::NotFound => return Ok(default()),
            _ => return Err(e.into()),
        },
    };

    parse_config(&contents.parse::<Value>()?)
}

fn default() -> Config {
    Config { auto: true, servers: HashMap::new() }
}

fn get_bool(table: &Table, key: &str, default: bool) -> Result<bool> {
    match table.get(key) {
        None => Ok(default),
        Some(Value::Boolean(v)) => Ok(*v),
        Some(v) => bail!("expected a true or false, got {v:?}"),
    }
}

fn parse_config(value: &Value) -> Result<Config> {
    let Value::Table(table) = value else {
        bail!("top level must be a table")
    };

    let auto = get_bool(table, "auto", true)?;
    let servers = match table.get("servers") {
        None => &Table::new(),
        Some(Value::Table(t)) => t,
        Some(v) => bail!("Expected a table in the servers key, got {v:?}"),
    };

    Ok(Config {
        auto,
        servers: parse_servers(servers, auto)?,
    })
}

fn parse_servers(
    table: &Table,
    auto: bool,
) -> Result<HashMap<String, ServerConfig>> {
    let mut servers = HashMap::new();
    for (k, v) in table {
        let Value::Table(table) = v else {
            bail!("expected a table for server {k}, got {v:?}");
        };

        servers.insert(k.clone(), parse_server(table, auto)?);
    }
    Ok(servers)
}

fn parse_server(table: &Table, auto: bool) -> Result<ServerConfig> {
    let auto = get_bool(table, "auto", auto)?;
    let ports = match table.get("ports") {
        None => HashMap::new(),
        Some(v) => parse_ports(v)?,
    };

    Ok(ServerConfig { auto, ports })
}

fn parse_ports(value: &Value) -> Result<HashMap<u16, PortConfig>> {
    match value {
        Value::Array(array) => {
            let mut ports = HashMap::new();
            for v in array {
                ports.insert(
                    get_port_number(v)?,
                    PortConfig { enabled: true, description: None },
                );
            }
            Ok(ports)
        }

        Value::Table(table) => {
            let mut ports = HashMap::new();
            for (k, v) in table {
                let port: u16 = k.parse()?;
                let config = parse_port_config(v)?;
                ports.insert(port, config);
            }
            Ok(ports)
        }

        _ => bail!("ports must be either an array or a table, got {value:?}"),
    }
}

fn parse_port_config(value: &Value) -> Result<PortConfig> {
    match value {
        Value::Boolean(enabled) => Ok(PortConfig{enabled:*enabled, description:None}),
        Value::String(description) => Ok(PortConfig{
            enabled: true,
            description: Some(description.clone()),
        }),
        Value::Table(table) => {
            let enabled = get_bool(table, "enabled", true)?;
            let description = match table.get("description") {
                Some(Value::String(desc)) => Some(desc.clone()),
                Some(v) => bail!("expect a string description, got {v:?}"),
                None => None,
            };
            Ok(PortConfig { enabled, description })
        },
        _ => bail!("expected either a boolean (enabled), a string (description), or a table for a port config, got {value:?}"),
    }
}

fn get_port_number(v: &Value) -> Result<u16> {
    let port: u16 = match v {
        Value::Integer(i) => (*i).try_into()?,
        v => bail!("port must be a small number, got {:?}", v),
    };
    Ok(port)
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;

    fn config_test(config: &str, expected: Config) {
        let config = config.parse::<Value>().expect("case not toml");
        let config = parse_config(&config).expect("unable to parse config");

        assert_eq!(expected, config);
    }

    fn config_error_test(config: &str) {
        let config = config.parse::<Value>().expect("case not toml");
        assert!(parse_config(&config).is_err());
    }

    #[test]
    fn empty() {
        config_test("", Config { auto: true, servers: HashMap::new() });
    }

    #[test]
    fn auto_false() {
        config_test(
            "
auto=false
",
            Config { auto: false, servers: HashMap::new() },
        );
    }

    #[test]
    fn auto_not_boolean() {
        config_error_test(
            "
auto='what is going on'
",
        );
    }

    #[test]
    fn servers_not_table() {
        config_error_test("servers=1234");
    }

    #[test]
    fn servers_default() {
        config_test("servers.foo={}", {
            let mut servers = HashMap::new();
            servers.insert(
                "foo".to_string(),
                ServerConfig { auto: true, ports: HashMap::new() },
            );
            Config { auto: true, servers }
        })
    }

    #[test]
    fn servers_auto_false() {
        config_test(
            "
[servers.foo]
auto=false
",
            {
                let mut servers = HashMap::new();
                servers.insert(
                    "foo".to_string(),
                    ServerConfig { auto: false, ports: HashMap::new() },
                );
                Config { auto: true, servers }
            },
        )
    }

    #[test]
    fn servers_auto_not_bool() {
        config_error_test(
            "
[servers.foo]
auto=1234
",
        )
    }

    #[test]
    fn servers_ports_list() {
        config_test(
            "
[servers.foo]
ports=[1,2,3]
",
            {
                let mut servers = HashMap::new();
                servers.insert(
                    "foo".to_string(),
                    ServerConfig {
                        auto: true,
                        ports: {
                            let mut ports = HashMap::new();
                            ports.insert(
                                1,
                                PortConfig { enabled: true, description: None },
                            );
                            ports.insert(
                                2,
                                PortConfig { enabled: true, description: None },
                            );
                            ports.insert(
                                3,
                                PortConfig { enabled: true, description: None },
                            );
                            ports
                        },
                    },
                );
                Config { auto: true, servers }
            },
        )
    }

    #[test]
    fn servers_ports_table_variations() {
        config_test(
            "
[servers.foo.ports]
1=true
2={enabled=false}
3=false
",
            {
                let mut servers = HashMap::new();
                servers.insert(
                    "foo".to_string(),
                    ServerConfig {
                        auto: true,
                        ports: {
                            let mut ports = HashMap::new();
                            ports.insert(
                                1,
                                PortConfig { enabled: true, description: None },
                            );
                            ports.insert(
                                2,
                                PortConfig {
                                    enabled: false,
                                    description: None,
                                },
                            );
                            ports.insert(
                                3,
                                PortConfig {
                                    enabled: false,
                                    description: None,
                                },
                            );
                            ports
                        },
                    },
                );
                Config { auto: true, servers }
            },
        )
    }

    #[test]
    fn servers_ports_table_descriptions() {
        config_test(
            "
[servers.foo.ports]
1={enabled=false}
2={description='humble'}
",
            {
                let mut servers = HashMap::new();
                servers.insert(
                    "foo".to_string(),
                    ServerConfig {
                        auto: true,
                        ports: {
                            let mut ports = HashMap::new();
                            ports.insert(
                                1,
                                PortConfig {
                                    enabled: false,
                                    description: None,
                                },
                            );
                            ports.insert(
                                2,
                                PortConfig {
                                    enabled: true,
                                    description: Some("humble".to_string()),
                                },
                            );
                            ports
                        },
                    },
                );
                Config { auto: true, servers }
            },
        )
    }

    #[test]
    fn servers_ports_raw_desc() {
        config_test(
            "
[servers.foo.ports]
1='humble'
",
            {
                let mut servers = HashMap::new();
                servers.insert(
                    "foo".to_string(),
                    ServerConfig {
                        auto: true,
                        ports: {
                            let mut ports = HashMap::new();
                            ports.insert(
                                1,
                                PortConfig {
                                    enabled: true,
                                    description: Some("humble".to_string()),
                                },
                            );
                            ports
                        },
                    },
                );
                Config { auto: true, servers }
            },
        )
    }

    #[test]
    fn servers_inherit_auto() {
        config_test(
            "
auto=false
servers.foo={}
",
            {
                let mut servers = HashMap::new();
                servers.insert(
                    "foo".to_string(),
                    ServerConfig { auto: false, ports: HashMap::new() },
                );
                Config { auto: false, servers }
            },
        )
    }
}
