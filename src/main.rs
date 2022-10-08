// TODO: An actual proper UI.

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: fwd <server>");
        std::process::exit(1);
    }

    let remote = &args[1];
    if remote == "--server" {
        fwd::run_server().await;
    } else {
        fwd::run_client(remote).await;
    }
}
