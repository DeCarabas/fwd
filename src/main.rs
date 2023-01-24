// TODO: An actual proper command line parsing
use indoc::indoc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn usage() {
    println!(indoc! {"
usage: fwd [--version] (<server> | browse <url>)

To connect a client to a server that has an `fwd` installed in its path, run
`fwd <server>` on the client, where <server> is the name of the server to
connect to.

On a server that already has a client connected to it you can use `fwd browse
<url>` to open `<url>` in the default browser of the client.
    "});
}

#[derive(Debug)]
enum Args {
    Help,
    Version,
    Server,
    Client(String),
    Browse(String),
    Error,
}

fn parse_args(args: Vec<String>) -> Args {
    // Look for help; allow it to come anywhere because sometimes you just
    // want to jam it on the end of an existing command line.
    for arg in &args {
        if arg == "--help" || arg == "-?" || arg == "-h" {
            return Args::Help;
        }
    }

    // No help, parse for reals.
    if args.len() >= 2 && args[1] == "--version" {
        Args::Version
    } else if args.len() == 2 && &args[1] == "--server" {
        Args::Server
    } else if args.len() == 3 && args[1] == "browse" {
        Args::Browse(args[2].to_string())
    } else {
        if args.len() != 2 {
            Args::Error
        } else {
            Args::Client(args[1].to_string())
        }
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
            fwd::browse_url(&url).await;
        }
        Args::Client(server) => {
            fwd::run_client(&server).await;
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
    }

    #[test]
    fn client() {
        assert_arg_parse!(&["foo.com"], Args::Client(_));
        assert_arg_parse!(&["a"], Args::Client(_));
        assert_arg_parse!(&["browse"], Args::Client(_));
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
