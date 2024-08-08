// TODO: An actual proper command line parsing
use indoc::indoc;

fn usage() {
    println!(indoc! {"
usage: fwd [options] (<server> | browse <url> | clip [<file>])

To connect a client to a server that has an `fwd` installed in its path, run
`fwd <server>` on the client, where <server> is the name of the server to
connect to.

On a server that already has a client connected to it you can use `fwd browse
<url>` to open `<url>` in the default browser of the client.

On a server that already has a client connected to it you can use `fwd clip -`
to read stdin and send it to the clipboard of the client, or `fwd clip <file>`
to send the the contents of `file`.

Options:
    --version    Print the version of fwd and exit
    --sudo, -s   Run the server side of fwd with `sudo`. This allows the
                 client to forward ports that are open by processes being
                 run under other accounts (e.g., docker containers being
                 run as root), but requires sudo access on the server and
                 *might* end up forwarding ports that you do not want
                 forwarded (e.g., port 22 for sshd, or port 53 for systemd.)
    --log-filter FILTER
                 Set remote server's log level. Default is `warn`. Supports
                 all of Rust's env_logger filter syntax, e.g.
                 `--log-filter=fwd::trace`.
    "});
}

#[derive(Debug)]
enum Args {
    Help,
    Version,
    Server,
    Client(String, bool, String),
    Browse(String),
    Clip(String),
    Error,
}

fn parse_args(args: Vec<String>) -> Args {
    let mut server = None;
    let mut sudo = None;
    let mut log_filter = None;
    let mut rest = Vec::new();

    let mut arg_iter = args.into_iter().skip(1);
    while let Some(arg) = arg_iter.next() {
        if arg == "--help" || arg == "-?" || arg == "-h" {
            return Args::Help;
        } else if arg == "--version" {
            return Args::Version;
        } else if arg == "--server" {
            server = Some(true)
        } else if arg == "--sudo" || arg == "-s" {
            sudo = Some(true)
        } else if arg.starts_with("--log-filter") {
            if arg.contains('=') {
                log_filter = Some(arg.split('=').nth(1).unwrap().to_owned());
            } else if let Some(arg) = arg_iter.next() {
                log_filter = Some(arg);
            } else {
                return Args::Error;
            }
        } else {
            rest.push(arg)
        }
    }

    if server.unwrap_or(false) {
        if rest.is_empty() && sudo.is_none() {
            Args::Server
        } else {
            Args::Error
        }
    } else if rest.is_empty() {
        Args::Error
    } else if rest[0] == "browse" {
        if rest.len() == 2 {
            Args::Browse(rest[1].to_string())
        } else if rest.len() == 1 {
            Args::Client(
                rest[0].to_string(),
                sudo.unwrap_or(false),
                log_filter.unwrap_or("warn".to_owned()),
            )
        } else {
            Args::Error
        }
    } else if rest[0] == "clip" {
        if rest.len() == 1 {
            Args::Client(
                rest[0].to_string(),
                sudo.unwrap_or(false),
                log_filter.unwrap_or("warn".to_owned()),
            )
        } else if rest.len() == 2 {
            Args::Clip(rest[1].to_string())
        } else {
            Args::Error
        }
    } else if rest.len() == 1 {
        Args::Client(
            rest[0].to_string(),
            sudo.unwrap_or(false),
            log_filter.unwrap_or("warn".to_owned()),
        )
    } else {
        Args::Error
    }
}

async fn browse_url(url: &str) {
    if let Err(e) = fwd::browse_url(url).await {
        eprintln!("Unable to open {url}");
        eprintln!("{:#}", e);
        std::process::exit(1);
    }
}

async fn clip_file(file: String) {
    if let Err(e) = fwd::clip_file(&file).await {
        eprintln!("Unable to copy to the clipboard");
        eprintln!("{:#}", e);
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
            println!("fwd {} (rev {}{})", fwd::VERSION, fwd::REV, fwd::DIRTY);
        }
        Args::Server => {
            fwd::run_server().await;
        }
        Args::Browse(url) => {
            browse_url(&url).await;
        }
        Args::Clip(file) => {
            clip_file(file).await;
        }
        Args::Client(server, sudo, log_filter) => {
            fwd::run_client(&server, sudo, &log_filter).await;
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
        let mut vec: Vec<String> = x.iter().map(|a| a.to_string()).collect();
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
        assert_arg_parse!(&["clip", "a.txt", "b.txt"], Args::Error);
        assert_arg_parse!(&["a", "b"], Args::Error);
        assert_arg_parse!(&["--server", "something"], Args::Error);
        assert_arg_parse!(&["--server", "--sudo"], Args::Error);
        assert_arg_parse!(&["--server", "-s"], Args::Error);
    }

    #[test]
    fn client() {
        assert_arg_parse!(&["foo.com"], Args::Client(_, false, _));
        assert_arg_parse!(&["a"], Args::Client(_, false, _));
        assert_arg_parse!(&["browse"], Args::Client(_, false, _));
        assert_arg_parse!(&["clip"], Args::Client(_, false, _));
        assert_arg_parse!(&["foo.com", "--sudo"], Args::Client(_, true, _));
        assert_arg_parse!(&["a", "-s"], Args::Client(_, true, _));
        assert_arg_parse!(&["-s", "browse"], Args::Client(_, true, _));
        assert_arg_parse!(&["-s", "clip"], Args::Client(_, true, _));

        assert_client_parse(&["a"], "a", false, "warn");
        assert_client_parse(&["a", "--log-filter", "info"], "a", false, "info");
        assert_client_parse(&["a", "--log-filter=info"], "a", false, "info");
        assert_client_parse(
            &["a", "--sudo", "--log-filter=info"],
            "a",
            true,
            "info",
        );
    }

    fn assert_client_parse(
        x: &[&str],
        server: &str,
        sudo: bool,
        log_filter: &str,
    ) {
        let args = parse_args(args(x));
        assert_matches!(args, Args::Client(_, _, _));
        if let Args::Client(s, sdo, lf) = args {
            assert_eq!(s, server);
            assert_eq!(sdo, sudo);
            assert_eq!(lf, log_filter);
        }
    }

    #[test]
    fn server() {
        assert_arg_parse!(&["--server"], Args::Server);
    }

    #[test]
    fn clip() {
        assert_arg_parse!(&["clip", "garbage"], Args::Clip(_));
        assert_arg_parse!(&["clip", "-"], Args::Clip(_));
    }

    #[test]
    fn browse() {
        assert_arg_parse!(&["browse", "google.com"], Args::Browse(_));
    }
}
