% fwd(1)
% John Doty <john@d0ty.me>
% August 2024

# NAME

fwd - Automatically forward connections to remote machines

# SYNOPSIS

**fwd** [OPTIONS] SERVER

**fwd** [OPTIONS] browse URL

**fwd** [OPTIONS] clip FILE

**fwd-browse** URL

# DESCRIPTION

**fwd** enumerates the listening ports the remote server and automatically listens for connections on the same ports on the local machine.
When **fwd** receives a connection on the local machine, it automatically forwards that connection to the remote machine.

**-s**, **-\-sudo**
:    Run the server side of fwd with `sudo`.
:    This allows the client to forward ports that are open by processes being run under other accounts (e.g., docker containers being run as root), but requires sudo access on the server and *might* end up forwarding ports that you do not want forwarded (e.g., port 22 for sshd, or port 53 for systemd.)

**-\-log-filter** **FILTER**
:    Set remote server's log level. Default is `warn`.
:    Supports all of Rust's env_logger filter syntax, e.g. `--log-filter=fwd::trace`.

**-\-version**
:    Print the version of fwd and exit.

# INTERACTIVE COMMANDS

Once **fwd** is connected, it displays an interactive list of the ports available on the remote server.

- Ports that **fwd** is listening on are displayed in the default terminal color.
- Ports that **fwd** is aware of but which are disabled are displayed in dark gray.
- Ports that **fwd** has tried to listen on but which have failed are displayed in red.
  Details on the error may be found in the log window.
  Disabling and re-enabling the port will cause **fwd** to try again.

The following commands are available while **fwd** is connected:

**Esc, q, Ctrl-C**
:    Exit **fwd**.

**?, h**
:    Display the help window.

**Up, k**
:    Select the previous port in the list.

**Down, j**
:    Select the next port in the list.

**Enter**
:    Attempt to browse to localhost on the specified port with the default browser.

**a**
:    Hide or show anonymous ports.
:    (See "identifying ports" below for more information on anonymous ports.)

**e**
:    Enable or disable the selected port.

**l**
:    Show or hide the log window.


# IDENTIFYING PORTS

**fwd** enumerates all of the ports that the remote server is listening on, and attempts to identify the process that is listening on each port.
It can identify ports in the following ways:

*docker*
:    **fwd** will attempt to find and connect to a docker engine on the remote machine.
:    If successful, it will list all of the forwarded ports, and identify each port as belonging to that docker container.

*procfs*
:    On Linux, the listening ports are found by reading procfs and mapping them back to process command lines.
:    **fwd** can only identify processes that the user it is connected as has permissions to read on the remote machine.

(Earlier methods take precedence over later methods.)

If **fwd** cannot identify the process that is listening on a given port, then the port is *anonymous*.
Anonymous ports are not enabled by default, but can be enabled manually, either with the UI or by configuration.

# OPENING BROWSERS
**fwd** can be used to open URLs in the default browser on the local machine.
Run **fwd browse URL** on the remote server to open the `URL` in the default browser on the local machine.

This only works if **fwd** is connected, and if the user running **fwd browse** is the same as the user that connected the **fwd** session.

The **fwd-browse** program acts as a wrapper around **fwd browse**, to be used with configurations that can't handle a browser being a program with an argument.

# CLIPBOARD
**fwd** can be used from the remote machine to place text on the clipboard of the local machine.
Run **fwd clip FILE** to copy the contents of the named file to the clipboard.
If **FILE** is **-**, this reads text from stdin instead.

# CONFIGURATION
**fwd** can be configured with a configuration file.

- On Windows, the config file will be in your roaming AppData folder.
  (e.g., *c:\\Users\\Winifred\\AppData\\Roaming\\fwd\\config\\config.toml*)
- On MacOS, the config file will be in *$HOME/Library/Application Support/fwd/config.toml*.
  (e.g., /Users/Margarie/Library/Application Support/fwd/config.toml)
- On XDG-ish systems (like Linux), the config file is in *~/.config/fwd/config.toml*.
  (e.g., */home/lynette/.config/fwd/config.toml*)

The following is an example of a *config.toml* file:

```
auto=true   # should `fwd` should enable identified ports (default true)

[servers.foo]       # Server-specific settings for foo
auto=true           # defaults to the global setting
ports=[1080, 1082]  # ports that are always present

[servers.bar.ports] # `ports` can also be a table with port numbers as keys
1080=true           # the values can be booleans (for enabled)...
1081="My program"   # or strings (for descriptions).

[servers.bar.ports.1082] # port values can also be tables
enabled=true
description="A humble python"
```

Ports that are specified in the configuration file will always be present in the list of ports for a given server, even if no process is listening on that port.

# TROUBLESHOOTING

Connections are made via the **ssh** command.
Your **ssh** must:

- Be on your path, so that **fwd** can find it to invoke it
- Be able to authenticate you to the remote server.
  (Interactive authentication is fine.)
- Understand the **-D** command line option, to operate as a SOCKS5 server
- Be able to start the **fwd** command on the remote server

A typical ssh invocation from **fwd** looks like:

```bash
ssh -T -D XXXX me@server FWD_LOG=warning FWD_SEND_ANONYMOUS=1 fwd --server
```

**fwd** only enumerates ports that are listening on loopback addresses (e.g., 127.0.0.1) or on all addresses (e.g., 0.0.0.0).
If it cannot find a particular port, check to make sure that the process listening on that port is accessible via localhost.

# SEE ALSO

ssh(1)
