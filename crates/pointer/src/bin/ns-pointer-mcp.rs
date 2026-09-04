//! `ns-pointer-mcp` — the MCP server, on stdio, proxying to an agent.
//!
//! Launched by an MCP client as a subprocess, which is why the MCP hop needs
//! no authentication: it is a pipe to its own parent. The one hop that *is*
//! network-exposed is the agent connection below, and that one is
//! authenticated (`docs/pointer-protocol.md` §4).
//!
//! ```text
//! NS_POINTER_ADDR=192.168.1.40:7373 NS_POINTER_TOKEN=… ns-pointer-mcp
//! ```

use nspointer::client::RemotePointer;
use nspointer::mcp::McpServer;
use nspointer::Session;
use tokio::io::BufReader;
use tokio::net::TcpStream;

#[tokio::main]
async fn main() {
    let addr = std::env::var("NS_POINTER_ADDR").unwrap_or_else(|_| "127.0.0.1:7373".into());
    let Ok(token) = std::env::var("NS_POINTER_TOKEN") else {
        // Failing loudly beats starting a server that cannot do anything: an
        // MCP client would otherwise show eight tools that all error.
        eprintln!("NS_POINTER_TOKEN is not set. There is no unauthenticated mode.");
        std::process::exit(2);
    };

    let stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot reach the pointer agent at {addr}: {e}");
            std::process::exit(1);
        }
    };
    // Nagle batches small writes, and every message here is a small write on
    // a latency-sensitive path.
    let _ = stream.set_nodelay(true);
    let (r, w) = stream.into_split();

    let pointer = match RemotePointer::connect(BufReader::new(r), w, &token).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agent at {addr} refused the connection: {e}");
            std::process::exit(1);
        }
    };
    let session = match Session::open(pointer).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not read the screen layout from {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "ns-pointer-mcp: {} screen(s) on {addr}",
        session.screens().screens.len()
    );

    let server = McpServer::new(session);
    if let Err(e) = server.serve(tokio::io::stdin(), tokio::io::stdout()).await {
        eprintln!("mcp: {e}");
        std::process::exit(1);
    }
}
