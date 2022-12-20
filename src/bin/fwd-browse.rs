// TODO: An actual proper UI.

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: fwd-browse <url>");
        std::process::exit(1);
    }

    fwd::browse_url(&args[1]).await;
}
