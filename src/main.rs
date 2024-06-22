// TODO: An actual proper command line parsing
use indoc::indoc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn usage() {
    println!(indoc! {"
usage: fwd [options] (<server> | browse <url> | clip [<file>])

To connect a client to a server that has an `fwd` installed in its path, run
`fwd <server>` on the client, where <server> is the name of the server to
connect to.

On a server that already has a client connected to it you can use `fwd browse
<url>` to open `<url>` in the default browser of the client.

Options:
    --version    Print the version of fwd and exit
    --sudo, -s   Run the server side of fwd with `sudo`. This allows the
                 client to forward ports that are open by processes being
                 run under other accounts (e.g., docker containers being
                 run as root), but requires sudo access on the server and
                 *might* end up forwarding ports that you do not want
                 forwarded (e.g., port 22 for sshd, or port 53 for systemd.)
    "});
}

#[derive(Debug)]
enum Args {
    Help,
    Version,
    Server,
    Client(String, bool),
    Browse(String),
    Error,
}

fn parse_args(args: Vec<String>) -> Args {
    let mut server = None;
    let mut sudo = None;
    let mut rest = Vec::new();

    for arg in args.into_iter().skip(1) {
        if arg == "--help" || arg == "-?" || arg == "-h" {
            return Args::Help;
        } else if arg == "--version" {
            return Args::Version;
        } else if arg == "--server" {
            server = Some(true)
        } else if arg == "--sudo" || arg == "-s" {
            sudo = Some(true)
        } else {
            rest.push(arg)
        }
    }

    if server.unwrap_or(false) {
        if rest.len() == 0 && sudo.is_none() {
            Args::Server
        } else {
            Args::Error
        }
    } else if rest.len() > 1 {
        if rest[0] == "browse" {
            if rest.len() == 2 {
                Args::Browse(rest[1].to_string())
            } else {
                Args::Error
            }
        } else {
            Args::Error
        }
    } else if rest.len() == 1 {
        Args::Client(rest[0].to_string(), sudo.unwrap_or(false))
    } else {
        Args::Error
    }
}

async fn browse_url(url: &str) {
    if let Err(e) = fwd::browse_url(&url).await {
        eprintln!("Unable to open {url}");
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() {
    match parse_args(std::env::args().collect()) {
        Args::Help => {
            usage();
        }
        Args::Version => {
            println!("fwd {VERSION}");
        }
        Args::Server => {
            fwd::run_server().await;
        }
        Args::Browse(url) => {
            browse_url(&url).await;
        }
        Args::Client(server, sudo) => {
            fwd::run_client(&server, sudo).await;
        }
        Args::Error => {
            usage();
            std::process::exit(1);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    // Goldarn it.
    fn args(x: &[&str]) -> Vec<String> {
        let mut vec: Vec<String> =
            x.into_iter().map(|a| a.to_string()).collect();
        vec.insert(0, "fwd".to_string());
        vec
    }

    macro_rules! assert_arg_parse {
        ( $x:expr, $p:pat ) => {
            assert_matches!(parse_args(args($x)), $p)
        };
    }

    #[test]
    fn help() {
        assert_arg_parse!(&["--help"], Args::Help);
        assert_arg_parse!(&["browse", "--help"], Args::Help);
        assert_arg_parse!(&["foo.com", "--help"], Args::Help);
        assert_arg_parse!(&["--help", "foo.com"], Args::Help);
        assert_arg_parse!(&["browse", "-?"], Args::Help);
        assert_arg_parse!(&["foo.com", "-h"], Args::Help);
    }

    #[test]
    fn errors() {
        assert_arg_parse!(&[], Args::Error);
        assert_arg_parse!(&["browse", "google.com", "what"], Args::Error);
        assert_arg_parse!(&["a", "b"], Args::Error);
        assert_arg_parse!(&["--server", "something"], Args::Error);
        assert_arg_parse!(&["--server", "--sudo"], Args::Error);
        assert_arg_parse!(&["--server", "-s"], Args::Error);
    }

    #[test]
    fn client() {
        assert_arg_parse!(&["foo.com"], Args::Client(_, false));
        assert_arg_parse!(&["a"], Args::Client(_, false));
        assert_arg_parse!(&["browse"], Args::Client(_, false));
        assert_arg_parse!(&["foo.com", "--sudo"], Args::Client(_, true));
        assert_arg_parse!(&["a", "-s"], Args::Client(_, true));
        assert_arg_parse!(&["-s", "browse"], Args::Client(_, true));
    }

    #[test]
    fn server() {
        assert_arg_parse!(&["--server"], Args::Server);
    }

    #[test]
    fn browse() {
        assert_arg_parse!(&["browse", "google.com"], Args::Browse(_));
    }
}
