use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "mem8",
    version,
    about = "Persistent memory for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over stdio.
    Serve {
        /// Serve over HTTP on this address instead of stdio, e.g. 127.0.0.1:8080.
        ///
        /// Requires MEM8_TOKEN, and TLS for any address other than loopback.
        /// In this mode every tool call must name its `project`: the server
        /// cannot infer one, because its working directory is its own rather
        /// than the caller's.
        #[arg(long, value_name = "ADDR")]
        http: Option<std::net::SocketAddr>,

        /// PEM certificate for TLS.
        #[arg(long, value_name = "PATH", requires = "tls_key")]
        tls_cert: Option<PathBuf>,

        /// PEM private key for TLS.
        #[arg(long, value_name = "PATH", requires = "tls_cert")]
        tls_key: Option<PathBuf>,

        /// Permit a non-loopback bind without TLS.
        ///
        /// The bearer token is then sent in plaintext and can be read by
        /// anything on the network path. Only for a trusted private network.
        #[arg(long)]
        insecure: bool,
    },
    /// Write all memories to a markdown file.
    Export { path: PathBuf },
    /// Read memories from a markdown file.
    Import { path: PathBuf },
    /// Embed memories that have none, so semantic search can find them.
    ///
    /// Needed once after enabling semantic search on an existing database, and
    /// after importing. Safe to re-run; it only touches memories with no
    /// embedding.
    Reindex,
}

/// Run the server, over stdio or HTTP.
///
/// stdio is the default and needs no configuration. HTTP requires a token and,
/// unless the bind is loopback, TLS — both checked before the socket opens, so
/// a misconfigured server fails to start rather than running insecurely.
async fn serve(
    http: Option<std::net::SocketAddr>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    insecure: bool,
) -> Result<(), String> {
    let Some(addr) = http else {
        // Flags that only mean something for HTTP must not be silently ignored
        // -- someone passing --tls-cert believes they are getting TLS.
        if tls_cert.is_some() || insecure {
            return Err(
                "--tls-cert, --tls-key, and --insecure apply only to --http. \
                 Without --http, mem8 serves over stdio, which is a private pipe \
                 to the process that spawned it and needs no transport security."
                    .into(),
            );
        }
        return mem8::mcp::serve_stdio().await.map_err(|e| e.to_string());
    };

    #[cfg(not(feature = "http"))]
    {
        let _ = (addr, tls_key);
        Err("this build of mem8 cannot serve over HTTP. Rebuild with \
             `cargo install --path . --features http`."
            .to_string())
    }

    #[cfg(feature = "http")]
    {
        use mem8::http::{auth, serve_http, Tls};

        // Before anything binds: no token means no server, rather than a
        // server anyone can read.
        let token = auth::token_from_env()?;

        let tls = match (tls_cert, tls_key) {
            (Some(cert), Some(key)) => Tls::Enabled { cert, key },
            // clap's `requires` makes one-without-the-other unreachable.
            _ => Tls::Disabled {
                insecure_override: insecure,
            },
        };

        serve_http(addr, tls, token)
            .await
            .map_err(|e| e.to_string())
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result: Result<(), String> = match cli.command {
        Command::Serve {
            http,
            tls_cert,
            tls_key,
            insecure,
        } => serve(http, tls_cert, tls_key, insecure).await,
        Command::Export { path } => match mem8::cli::export(&path).await {
            Ok(n) => {
                println!("Exported {n} memories to {}", path.display());
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        Command::Import { path } => match mem8::cli::import(&path).await {
            Ok(n) => {
                println!("Imported {n} memories from {}", path.display());
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        Command::Reindex => match mem8::cli::reindex().await {
            Ok(0) => {
                println!("Nothing to do: every memory already has an embedding.");
                Ok(())
            }
            Ok(n) => {
                println!("Embedded {n} memories.");
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
    };

    if let Err(message) = result {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}
