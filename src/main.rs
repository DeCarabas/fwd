// TODO: An actual proper UI.

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() == 2 && &args[1] == "--server" {
        fwd::run_server().await;
    } else if args.len() == 3 && args[1] == "browse" {
        fwd::browse_url(&args[2]).await;
    } else {
        if args.len() < 2 {
            eprintln!("Usage: fwd <server>");
            std::process::exit(1);
        }

        fwd::run_client(&args[1]).await;
    }
}
