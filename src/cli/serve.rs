use anyhow::Result;
use clap::Args;
use std::sync::Arc;

#[derive(Args)]
pub struct ServeOpts {
    /// TCP port to listen on.
    #[arg(short, long)]
    pub port: Option<u16>,

    /// Unix socket path to listen on.
    #[arg(short, long)]
    pub socket: Option<String>,
}

pub async fn run(opts: ServeOpts) -> Result<()> {
    let provider = Arc::new(frontend_grpc::service::FrontendProvider::new(1));

    if let Some(port) = opts.port {
        frontend_grpc::server::serve_tcp(provider, port).await?;
    } else if let Some(socket) = opts.socket {
        #[cfg(unix)]
        {
            frontend_grpc::server::serve_unix(provider, &socket).await?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("Unix sockets are not supported on this platform. Use --port instead.");
        }
    } else {
        // Default to port 9090
        frontend_grpc::server::serve_tcp(provider, 9090).await?;
    }

    Ok(())
}
