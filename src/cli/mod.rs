pub mod fix;
pub mod serve;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "frontend-analyzer-provider",
    about = "Konveyor external provider for JS/TS/JSX/TSX and CSS/SCSS analysis",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Apply fixes based on Konveyor analysis output.
    Fix(fix::FixOpts),

    /// Start as a Konveyor gRPC external provider.
    Serve(serve::ServeOpts),
}
