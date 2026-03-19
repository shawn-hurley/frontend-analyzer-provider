//! gRPC server transport setup.

use crate::proto::provider_code_location_service_server::ProviderCodeLocationServiceServer;
use crate::proto::provider_service_server::ProviderServiceServer;
use crate::service::FrontendProvider;
use std::sync::Arc;
use tonic::transport::Server;

/// Start the gRPC server on a TCP port.
pub async fn serve_tcp(provider: Arc<FrontendProvider>, port: u16) -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{}", port).parse()?;
    tracing::info!("Frontend provider listening on {}", addr);

    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(crate::proto::FILE_DESCRIPTOR_SET)
        .build_v1alpha()?;

    Server::builder()
        .add_service(ProviderServiceServer::from_arc(provider.clone()))
        .add_service(ProviderCodeLocationServiceServer::from_arc(provider))
        .add_service(reflection)
        .serve(addr)
        .await?;

    Ok(())
}

/// Start the gRPC server on a Unix domain socket.
#[cfg(unix)]
pub async fn serve_unix(provider: Arc<FrontendProvider>, socket_path: &str) -> anyhow::Result<()> {
    use tokio::net::UnixListener;
    use tokio_stream::wrappers::UnixListenerStream;

    // Remove existing socket file
    let _ = std::fs::remove_file(socket_path);

    let uds = UnixListener::bind(socket_path)?;
    let uds_stream = UnixListenerStream::new(uds);
    tracing::info!("Frontend provider listening on unix://{}", socket_path);

    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(crate::proto::FILE_DESCRIPTOR_SET)
        .build_v1alpha()?;

    Server::builder()
        .add_service(ProviderServiceServer::from_arc(provider.clone()))
        .add_service(ProviderCodeLocationServiceServer::from_arc(provider))
        .add_service(reflection)
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}
