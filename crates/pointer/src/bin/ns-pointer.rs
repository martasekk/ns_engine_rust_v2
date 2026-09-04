//! `ns-pointer` — drive a machine through a running `ns-pointerd`.
//!
//! Runs beside the agent over loopback. See `nspointer::cli::USAGE`.

use nspointer::cli;
use nspointer::client::RemotePointer;
use nspointer::Session;
use tokio::io::BufReader;
use tokio::net::TcpStream;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print!("{}", cli::USAGE);
        return;
    }
    let cmd = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };

    let addr = std::env::var("NS_POINTER_ADDR").unwrap_or_else(|_| "127.0.0.1:7373".into());
    let Ok(token) = std::env::var("NS_POINTER_TOKEN") else {
        eprintln!("NS_POINTER_TOKEN is not set. There is no unauthenticated mode.");
        std::process::exit(2);
    };

    let stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("no agent at {addr}: {e}");
            eprintln!("start it on the target machine: ns-pointerd serve");
            std::process::exit(1);
        }
    };
    let _ = stream.set_nodelay(true);
    let (r, w) = stream.into_split();

    let pointer = match RemotePointer::connect(BufReader::new(r), w, &token).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agent refused the connection: {e}");
            std::process::exit(1);
        }
    };
    if !pointer.local_override() {
        eprintln!(
            "warning: this agent reports no local override — moving the physical \
             mouse will not interrupt anything sent from here."
        );
    }
    let session = match Session::open(pointer).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not read the screen layout: {e}");
            std::process::exit(1);
        }
    };
    match cli::run(&session, cmd).await {
        Ok(out) => println!("{out}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
