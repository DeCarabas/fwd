// TODO: An actual proper UI.

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: fwd-browse <url>");
        std::process::exit(1);
    }

    let url = &args[1];
    if let Err(e) = fwd::browse_url(url).await {
        eprintln!("Unable to open {url}");
        eprintln!("{}", e);
        std::process::exit(1);
    }
}
