//! ProviderCodeLocationService gRPC implementation.
//!
//! Provides code snippets for incidents so that kantra can populate the
//! `codeSnip` field in the output YAML.

use crate::proto::provider_code_location_service_server::ProviderCodeLocationService;
use crate::proto::{GetCodeSnipRequest, GetCodeSnipResponse};
use crate::service::FrontendProvider;
use std::fs::File;
use std::io::{BufRead, BufReader};
use tonic::{Request, Response, Status};
use url::Url;

#[tonic::async_trait]
impl ProviderCodeLocationService for FrontendProvider {
    async fn get_code_snip(
        &self,
        request: Request<GetCodeSnipRequest>,
    ) -> Result<Response<GetCodeSnipResponse>, Status> {
        let req = request.into_inner();

        let code_location = req
            .code_location
            .ok_or_else(|| Status::invalid_argument("no code location sent"))?;
        let start_position = code_location
            .start_position
            .ok_or_else(|| Status::invalid_argument("no start position sent"))?;
        let end_position = code_location
            .end_position
            .ok_or_else(|| Status::invalid_argument("no end position sent"))?;

        let file_uri = Url::parse(&req.uri).map_err(|e| {
            Status::invalid_argument(format!("invalid URI '{}': {}", req.uri, e))
        })?;

        let file_path = file_uri.to_file_path().map_err(|_| {
            Status::invalid_argument(format!("cannot convert URI to path: {}", req.uri))
        })?;

        let file = File::open(&file_path).map_err(|e| {
            Status::not_found(format!("cannot open file {}: {}", file_path.display(), e))
        })?;
        let reader = BufReader::new(file);

        let start_line = start_position.line as usize;
        let end_line = end_position.line as usize;

        let skip = start_line.saturating_sub(self.context_lines);
        let take = (end_line - start_line) + (2 * self.context_lines) + 1;

        let snip: String = reader
            .lines()
            .skip(skip)
            .take(take)
            .enumerate()
            .map(|(i, line)| {
                let line_num = skip + i;
                let content = line.unwrap_or_default();
                format!("{:>5}  {}\n", line_num + 1, content)
            })
            .collect();

        Ok(Response::new(GetCodeSnipResponse { snip }))
    }
}
