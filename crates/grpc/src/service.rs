//! Konveyor ProviderService gRPC implementation.

use crate::proto::*;
use crate::proto::provider_service_server::ProviderService;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use std::pin::Pin;

/// The frontend analyzer provider.
pub struct FrontendProvider {
    pub config: Arc<Mutex<Option<Config>>>,
    pub project_root: Arc<Mutex<Option<PathBuf>>>,
    /// Number of context lines to include around code snippets.
    pub context_lines: usize,
}

impl FrontendProvider {
    pub fn new(context_lines: usize) -> Self {
        Self {
            config: Arc::new(Mutex::new(None)),
            project_root: Arc::new(Mutex::new(None)),
            context_lines,
        }
    }
}

type ProgressStream = Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;

#[tonic::async_trait]
impl ProviderService for FrontendProvider {
    async fn capabilities(
        &self,
        _request: Request<()>,
    ) -> Result<Response<CapabilitiesResponse>, Status> {
        let capabilities = vec![
            Capability {
                name: "referenced".into(),
                template_context: None,
            },
            Capability {
                name: "cssclass".into(),
                template_context: None,
            },
            Capability {
                name: "cssvar".into(),
                template_context: None,
            },
            Capability {
                name: "dependency".into(),
                template_context: None,
            },
        ];

        Ok(Response::new(CapabilitiesResponse { capabilities }))
    }

    async fn init(
        &self,
        request: Request<Config>,
    ) -> Result<Response<InitResponse>, Status> {
        let config = request.into_inner();
        let location = config.location.clone();

        tracing::info!("Initializing frontend provider with location: {}", location);

        let root = PathBuf::from(&location);
        if !root.exists() {
            return Ok(Response::new(InitResponse {
                error: format!("Location does not exist: {}", location),
                successful: false,
                id: 0,
                builtin_config: None,
            }));
        }

        *self.config.lock().map_err(|_| Status::internal("Config lock poisoned"))? = Some(config);
        *self.project_root.lock().map_err(|_| Status::internal("Project root lock poisoned"))? = Some(root);

        Ok(Response::new(InitResponse {
            error: String::new(),
            successful: true,
            id: 1,
            builtin_config: None,
        }))
    }

    async fn evaluate(
        &self,
        request: Request<EvaluateRequest>,
    ) -> Result<Response<EvaluateResponse>, Status> {
        let req = request.into_inner();
        let root = self
            .project_root
            .lock()
            .map_err(|_| Status::internal("Project root lock poisoned"))?
            .clone()
            .ok_or_else(|| Status::failed_precondition("Provider not initialized"))?;

        tracing::info!("Evaluate request: cap={}, condition_info={}", &req.cap, &req.condition_info);
        match crate::evaluate::evaluate_condition(&root, &req.cap, &req.condition_info) {
            Ok(response) => Ok(Response::new(EvaluateResponse {
                error: String::new(),
                successful: true,
                response: Some(response),
            })),
            Err(e) => Ok(Response::new(EvaluateResponse {
                error: e.to_string(),
                successful: false,
                response: None,
            })),
        }
    }

    async fn stop(
        &self,
        _request: Request<ServiceRequest>,
    ) -> Result<Response<()>, Status> {
        tracing::info!("Frontend provider stopping");
        Ok(Response::new(()))
    }

    async fn get_dependencies(
        &self,
        _request: Request<ServiceRequest>,
    ) -> Result<Response<DependencyResponse>, Status> {
        // TODO: implement dependency listing from package.json
        Ok(Response::new(DependencyResponse {
            successful: true,
            error: String::new(),
            file_dep: vec![],
        }))
    }

    async fn get_dependencies_dag(
        &self,
        _request: Request<ServiceRequest>,
    ) -> Result<Response<DependencyDagResponse>, Status> {
        Ok(Response::new(DependencyDagResponse {
            successful: true,
            error: String::new(),
            file_dag_dep: vec![],
        }))
    }

    async fn notify_file_changes(
        &self,
        _request: Request<NotifyFileChangesRequest>,
    ) -> Result<Response<NotifyFileChangesResponse>, Status> {
        Ok(Response::new(NotifyFileChangesResponse {
            error: String::new(),
        }))
    }

    async fn prepare(
        &self,
        _request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        Ok(Response::new(PrepareResponse {
            error: String::new(),
        }))
    }

    type StreamPrepareProgressStream = ProgressStream;

    async fn stream_prepare_progress(
        &self,
        _request: Request<PrepareProgressRequest>,
    ) -> Result<Response<Self::StreamPrepareProgressStream>, Status> {
        let stream = async_stream::stream! {
            yield Ok(ProgressEvent {
                r#type: 0,
                provider_name: "frontend".into(),
                files_processed: 0,
                total_files: 0,
            });
        };
        Ok(Response::new(Box::pin(stream)))
    }
}
