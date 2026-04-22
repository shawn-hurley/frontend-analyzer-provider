//! Konveyor ProviderService gRPC implementation.

use crate::proto::provider_service_server::ProviderService;
use crate::proto::*;
use ast_index_react::types::ReactProjectIndex;
use std::collections::HashSet;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

/// The frontend analyzer provider.
pub struct FrontendProvider {
    pub config: Arc<Mutex<Option<Config>>>,
    pub project_root: Arc<Mutex<Option<PathBuf>>>,
    /// Pre-built React/JSX project index for cross-file queries.
    ///
    /// Built once during `Init` and shared across all `Evaluate` calls.
    /// Provides transparency info, JSX element spans/props, object
    /// properties, and the full symbol/import graph without re-parsing.
    pub react_index: Arc<Mutex<Option<ReactProjectIndex>>>,
    /// Number of context lines to include around code snippets.
    pub context_lines: usize,
}

impl FrontendProvider {
    pub fn new(context_lines: usize) -> Self {
        Self {
            config: Arc::new(Mutex::new(None)),
            project_root: Arc::new(Mutex::new(None)),
            react_index: Arc::new(Mutex::new(None)),
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

    async fn init(&self, request: Request<Config>) -> Result<Response<InitResponse>, Status> {
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

        // Install npm dependencies so that `npm ls` can resolve the full
        // dependency tree for the `GetDependencies` RPC. Without this,
        // dependency rules (e.g., "update @patternfly/react-core to v6")
        // cannot match because the resolved versions are unknown.
        let pkg_json = root.join("package.json");
        if pkg_json.exists() {
            tracing::info!("Running npm install in {}", location);
            let output = std::process::Command::new("npm")
                .arg("install")
                .arg("--ignore-scripts")
                .arg("--no-audit")
                .arg("--no-fund")
                .current_dir(&root)
                .output();

            match output {
                Ok(result) => {
                    if result.status.success() {
                        tracing::info!("npm install completed successfully");
                    } else {
                        let stderr = String::from_utf8_lossy(&result.stderr);
                        tracing::warn!(
                            "npm install failed (non-fatal): {}",
                            stderr.chars().take(500).collect::<String>()
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("npm install could not run (non-fatal): {}", e);
                }
            }
        }

        // Build the React/JSX project index.
        // This indexes all TS/JS/TSX/JSX files once, extracting:
        // - Symbol definitions and references
        // - Import graph (who imports what, resolved paths)
        // - Component transparency (children passthrough detection)
        // - JSX element spans and props
        // - Object literal properties (for spread prop resolution)
        tracing::info!("Building React project index for {}", root.display());
        let analyzer = ast_index_react::analyzer::ReactAnalyzer::new(&root);
        let index = ast_index::ProjectIndex::new(analyzer);
        let stats = index.build(&root);
        tracing::info!(
            "React index built: {} files scanned, {} cached, {} failed, {} unresolved modules",
            stats.files_scanned,
            stats.files_cached,
            stats.files_failed,
            stats.unresolved_modules.len(),
        );

        *self
            .config
            .lock()
            .map_err(|_| Status::internal("Config lock poisoned"))? = Some(config);
        *self
            .project_root
            .lock()
            .map_err(|_| Status::internal("Project root lock poisoned"))? = Some(root);
        *self
            .react_index
            .lock()
            .map_err(|_| Status::internal("React index lock poisoned"))? = Some(index);

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

        let index_guard = self
            .react_index
            .lock()
            .map_err(|_| Status::internal("React index lock poisoned"))?;
        let index_ref = index_guard.as_ref();

        tracing::info!(
            "Evaluate request: cap={}, condition_info={}",
            &req.cap,
            &req.condition_info
        );
        match crate::evaluate::evaluate_condition(&root, &req.cap, &req.condition_info, index_ref) {
            Ok(result) => {
                // Build an error summary if any files could not be parsed.
                let error = if result.parse_errors.is_empty() {
                    String::new()
                } else {
                    let file_list: Vec<String> = result
                        .parse_errors
                        .iter()
                        .map(|e| format!("{}: {}", e.file_path.display(), e.message))
                        .collect();
                    format!(
                        "{} file(s) could not be parsed and were skipped:\n{}",
                        result.parse_errors.len(),
                        file_list.join("\n")
                    )
                };

                Ok(Response::new(EvaluateResponse {
                    error,
                    successful: true,
                    response: Some(result.response),
                }))
            }
            Err(e) => Ok(Response::new(EvaluateResponse {
                error: e.to_string(),
                successful: false,
                response: None,
            })),
        }
    }

    async fn stop(&self, _request: Request<ServiceRequest>) -> Result<Response<()>, Status> {
        tracing::info!("Frontend provider stopping");
        Ok(Response::new(()))
    }

    async fn get_dependencies(
        &self,
        _request: Request<ServiceRequest>,
    ) -> Result<Response<DependencyResponse>, Status> {
        let root = self
            .project_root
            .lock()
            .map_err(|_| Status::internal("Project root lock poisoned"))?
            .clone()
            .ok_or_else(|| Status::failed_precondition("Provider not initialized"))?;

        // Parse declared dependencies from package.json files directly,
        // rather than using `npm ls` which returns resolved versions from
        // node_modules. Declared versions are what dependency rules should
        // match against — they're what the user controls and what needs
        // updating during a migration.
        //
        // Benefits over `npm ls`:
        //  - Returns declared versions, not resolved (correct for rule matching)
        //  - Correctly tags dependencies vs devDependencies vs peerDependencies
        //  - Supports npm workspaces
        //  - Works regardless of package manager (npm/yarn/pnpm)
        //  - Does not require npm install to have succeeded
        let pkg_paths = frontend_js_scanner::dependency::find_package_jsons(&root);

        let mut file_deps = Vec::new();
        let dep_sections = ["dependencies", "devDependencies", "peerDependencies"];

        for pkg_path in &pkg_paths {
            let file_uri = format!("file://{}", pkg_path.display());

            let content = match std::fs::read_to_string(pkg_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        path = %pkg_path.display(),
                        error = %e,
                        "Failed to read package.json, skipping"
                    );
                    continue;
                }
            };

            let pkg: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        path = %pkg_path.display(),
                        error = %e,
                        "Failed to parse package.json, skipping"
                    );
                    continue;
                }
            };

            let mut deps = Vec::new();

            for section in &dep_sections {
                if let Some(dep_map) = pkg.get(*section).and_then(|v| v.as_object()) {
                    for (name, version_val) in dep_map {
                        let raw_version = version_val.as_str().unwrap_or("0.0.0");

                        // Skip workspace protocol entries (e.g., "workspace:*")
                        if raw_version.starts_with("workspace:") {
                            continue;
                        }

                        // Strip range prefixes (^, ~, >=, etc.) to get the
                        // base semver for kantra's version comparison.
                        let version =
                            frontend_js_scanner::dependency::strip_npm_prefix(raw_version)
                                .to_string();

                        deps.push(Dependency {
                            name: name.clone(),
                            version,
                            classifier: String::new(),
                            r#type: section.to_string(),
                            resolved_identifier: String::new(),
                            file_uri_prefix: String::new(),
                            indirect: false,
                            extras: None,
                            labels: vec![],
                        });
                    }
                }
            }

            tracing::info!(
                path = %pkg_path.display(),
                count = deps.len(),
                "Parsed declared dependencies from package.json"
            );

            file_deps.push(FileDep {
                file_uri,
                list: Some(DependencyList { deps }),
            });
        }

        let direct_count: usize = file_deps
            .iter()
            .filter_map(|fd| fd.list.as_ref())
            .map(|l| l.deps.len())
            .sum();
        tracing::info!(
            "Parsed {} declared dependencies from {} package.json file(s)",
            direct_count,
            file_deps.len()
        );

        // ── Lockfile entries (indirect / transitive dependencies) ────
        //
        // Parse the project's lockfile(s) to surface ALL resolved packages,
        // including transitive dependencies not visible in package.json.
        // This lets kantra fire rules on transitive copies of packages
        // (e.g., a nested @patternfly/react-core@5.x brought in by
        // @patternfly/react-topology when the direct dep is already v6).
        //
        // Uses monorepo-aware discovery: checks root first (standard
        // workspace case), falls back to per-package.json lockfiles for
        // multi-project repos.
        let (lockfile_entries, lockfile_paths) =
            frontend_js_scanner::lockfile::parse_all_lockfiles(&root, &pkg_paths).unwrap_or_else(
                |e| {
                    tracing::warn!(
                        error = %e,
                        "Failed to parse lockfile(s), skipping indirect dependencies"
                    );
                    (Vec::new(), Vec::new())
                },
            );

        if !lockfile_entries.is_empty() {
            // Build dedup set from direct deps to avoid exact duplicates.
            // Direct deps use strip_npm_prefix (e.g., "5.4.1") while lockfile
            // entries have resolved versions (e.g., "5.4.14"), so most entries
            // won't collide. We keep entries with the same name but different
            // version — that's the key case (transitive v5 copy when direct is v6).
            let direct_set: HashSet<(String, String)> = file_deps
                .iter()
                .filter_map(|fd| fd.list.as_ref())
                .flat_map(|l| l.deps.iter())
                .map(|d| (d.name.clone(), d.version.clone()))
                .collect();

            // Build reverse index: for each package name, which lockfile
            // entries list it as a dependency. This lets us annotate each
            // indirect dep with its parent(s) via the `extras` field.
            //
            // Example: react-topology@5.2.1 has dependencies including
            // react-core@^5.1.1. The reverse index maps "react-core" →
            // ["@patternfly/react-topology@5.2.1"]. When we emit the
            // indirect dep for react-core@5.4.14, we attach
            // extras.resolvedFor = "@patternfly/react-topology@5.2.1"
            // so the fix engine knows which package to actually update.
            let mut reverse_deps: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for entry in &lockfile_entries {
                for dep_name in entry.dependencies.keys() {
                    reverse_deps
                        .entry(dep_name.clone())
                        .or_default()
                        .push(format!("{}@{}", entry.name, entry.version));
                }
            }

            let mut lockfile_deps = Vec::new();
            for entry in &lockfile_entries {
                if direct_set.contains(&(entry.name.clone(), entry.version.clone())) {
                    continue;
                }

                // Build extras with resolvedFor provenance
                let extras = {
                    let mut fields = std::collections::BTreeMap::new();

                    // Which package(s) depend on this entry
                    if let Some(parents) = reverse_deps.get(&entry.name) {
                        let parent_list = parents.join(", ");
                        fields.insert(
                            "resolvedFor".into(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(parent_list)),
                            },
                        );
                    }

                    if fields.is_empty() {
                        None
                    } else {
                        Some(prost_types::Struct { fields })
                    }
                };

                lockfile_deps.push(Dependency {
                    name: entry.name.clone(),
                    version: entry.version.clone(),
                    classifier: String::new(),
                    r#type: "lockfile".into(),
                    resolved_identifier: String::new(),
                    file_uri_prefix: String::new(),
                    indirect: true,
                    extras,
                    labels: vec![],
                });
            }

            if !lockfile_deps.is_empty() {
                let lockfile_uri = lockfile_paths
                    .first()
                    .map(|p| format!("file://{}", p.display()))
                    .unwrap_or_else(|| format!("file://{}/lockfile", root.display()));

                tracing::info!(
                    count = lockfile_deps.len(),
                    lockfiles = lockfile_paths.len(),
                    "Adding indirect dependencies from lockfile(s)"
                );

                file_deps.push(FileDep {
                    file_uri: lockfile_uri,
                    list: Some(DependencyList {
                        deps: lockfile_deps,
                    }),
                });
            }

            // ── Peer dependency constraints ──────────────────────────────
            //
            // For each direct dependency in the consumer's package.json, check
            // its lockfile entry for peerDependencies. Emit each peer dep
            // constraint as a separate dependency entry so kantra can detect
            // version incompatibilities.
            //
            // Example: @openshift/dynamic-plugin-sdk-utils@4.1.0 declares
            // peerDependencies: { "@patternfly/react-core": "^5.1.0" }.
            // We emit an entry for @patternfly/react-core at version 5.1.0
            // (the base of the constraint) with type "peerDependency" and
            // resolvedFor pointing to the parent package. This lets the
            // dep-update rule for @patternfly/react-core match it, creating
            // an incident that the fix engine can use to update the parent.
            let direct_dep_names: HashSet<String> = file_deps
                .iter()
                .filter_map(|fd| fd.list.as_ref())
                .flat_map(|l| l.deps.iter())
                .filter(|d| !d.indirect)
                .map(|d| d.name.clone())
                .collect();

            let mut peer_dep_entries = Vec::new();

            for entry in &lockfile_entries {
                // Only process lockfile entries for packages that are direct
                // dependencies of the consumer — we can only update what's
                // in their package.json.
                if !direct_dep_names.contains(&entry.name) {
                    continue;
                }

                // Skip entries with no peer dependencies
                if entry.peer_dependencies.is_empty() {
                    continue;
                }

                let parent_label = format!("{}@{}", entry.name, entry.version);

                for (peer_name, peer_constraint) in &entry.peer_dependencies {
                    // Skip self-references
                    if peer_name == &entry.name {
                        continue;
                    }

                    // Strip the constraint prefix (^, ~, >=, npm:, etc.) to get
                    // the base version for kantra comparison.
                    let base_version =
                        frontend_js_scanner::dependency::strip_npm_prefix(peer_constraint)
                            .to_string();

                    let extras = {
                        let mut fields = std::collections::BTreeMap::new();
                        fields.insert(
                            "resolvedFor".into(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(
                                    parent_label.clone(),
                                )),
                            },
                        );
                        fields.insert(
                            "constraintSource".into(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(
                                    "peerDependency".into(),
                                )),
                            },
                        );
                        Some(prost_types::Struct { fields })
                    };

                    peer_dep_entries.push(Dependency {
                        name: peer_name.clone(),
                        version: base_version,
                        classifier: String::new(),
                        r#type: "peerDependency".into(),
                        resolved_identifier: String::new(),
                        file_uri_prefix: String::new(),
                        indirect: true,
                        extras,
                        labels: vec![],
                    });
                }
            }

            if !peer_dep_entries.is_empty() {
                let lockfile_uri = lockfile_paths
                    .first()
                    .map(|p| format!("file://{}", p.display()))
                    .unwrap_or_else(|| format!("file://{}/lockfile", root.display()));

                tracing::info!(
                    count = peer_dep_entries.len(),
                    "Adding peer dependency constraint entries from lockfile"
                );

                file_deps.push(FileDep {
                    file_uri: lockfile_uri,
                    list: Some(DependencyList {
                        deps: peer_dep_entries,
                    }),
                });
            }
        }

        let total: usize = file_deps
            .iter()
            .filter_map(|fd| fd.list.as_ref())
            .map(|l| l.deps.len())
            .sum();
        tracing::info!(
            "Returning {} total dependencies ({} direct, {} indirect) from {} source(s)",
            total,
            direct_count,
            total - direct_count,
            file_deps.len()
        );

        Ok(Response::new(DependencyResponse {
            successful: true,
            error: String::new(),
            file_dep: file_deps,
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
