use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mem8", version, about = "Persistent memory for AI coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over stdio.
    Serve,
    /// Write all memories to a markdown file.
    Export { path: PathBuf },
    /// Read memories from a markdown file.
    Import { path: PathBuf },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result: Result<(), String> = match cli.command {
        Command::Serve => mem8::mcp::serve_stdio().await.map_err(|e| e.to_string()),
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
    };

    if let Err(message) = result {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}
