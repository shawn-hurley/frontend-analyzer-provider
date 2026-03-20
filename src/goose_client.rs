//! Goose headless client for AI-assisted fix generation.
//!
//! Shells out to `goose run` with the developer extension to apply
//! complex migration fixes that can't be handled by pattern matching.

use anyhow::{Context, Result};
use frontend_core::fix::LlmFixRequest;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Per-file timeout for goose subprocess (seconds).
const GOOSE_TIMEOUT_SECS: u64 = 120;

/// Delay between consecutive goose calls to avoid rate limiting (seconds).
const GOOSE_DELAY_SECS: u64 = 2;

/// Maximum retries when a goose call times out.
const GOOSE_MAX_RETRIES: u32 = 1;

/// Result of a goose fix attempt.
#[derive(Debug)]
pub struct GooseFixResult {
    pub file_path: PathBuf,
    pub rule_id: String,
    pub success: bool,
    pub output: String,
}

/// Run a goose command with a timeout. Returns the combined stdout+stderr
/// output, or an error if the process times out or fails to start.
fn run_goose_with_timeout(prompt: &str, max_turns: &str) -> Result<(bool, String)> {
    let mut cmd = Command::new("goose");
    cmd.args([
        "run",
        "--quiet",
        "--text",
        prompt,
        "--with-builtin",
        "developer",
        "--no-session",
        "--max-turns",
        max_turns,
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .stdin(std::process::Stdio::null());

    // Isolate goose in its own process group so that signals sent by
    // goose's child processes (e.g., claude-code) cannot propagate to
    // our parent process.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .context("Failed to execute goose. Is it installed and in PATH?")?;

    let timeout = Duration::from_secs(GOOSE_TIMEOUT_SECS);
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process exited
                let stdout = child
                    .stdout
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok();
                        buf
                    })
                    .unwrap_or_default();
                let stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok();
                        buf
                    })
                    .unwrap_or_default();
                let combined = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{}\n{}", stdout, stderr)
                };
                return Ok((status.success(), combined));
            }
            Ok(None) => {
                // Still running — check timeout
                if start.elapsed() >= timeout {
                    // Kill the entire process group (goose + any children like
                    // claude-code) to prevent orphaned processes.
                    #[cfg(unix)]
                    {
                        let pid = child.id() as i32;
                        // SIGTERM first to allow graceful shutdown
                        unsafe {
                            libc::kill(-pid, libc::SIGTERM);
                        }
                        std::thread::sleep(Duration::from_millis(1000));
                        // SIGKILL to ensure cleanup
                        unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.kill();
                    }
                    let _ = child.wait();
                    anyhow::bail!("goose timed out after {}s", GOOSE_TIMEOUT_SECS);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                anyhow::bail!("Failed to wait on goose process: {}", e);
            }
        }
    }
}

/// Run goose to fix a single incident.
pub fn run_goose_fix(request: &LlmFixRequest) -> Result<GooseFixResult> {
    let prompt = build_prompt(request);
    let (success, output) = run_goose_with_timeout(&prompt, "5")?;

    Ok(GooseFixResult {
        file_path: request.file_path.clone(),
        rule_id: request.rule_id.clone(),
        success,
        output,
    })
}

/// Run goose to fix multiple incidents in the same file (batched).
///
/// Scales the max turns with the number of fixes: base 5 (read + edit + write
/// with margin) plus 1 extra turn per fix to allow the LLM room for multi-step
/// edits, capped at 30 to avoid runaway sessions.
pub fn run_goose_fix_batch(
    file_path: &PathBuf,
    requests: &[&LlmFixRequest],
) -> Result<GooseFixResult> {
    let prompt = build_batch_prompt(file_path, requests);
    let max_turns = (5 + requests.len()).min(30);
    let max_turns_str = max_turns.to_string();
    let (success, output) = run_goose_with_timeout(&prompt, &max_turns_str)?;

    Ok(GooseFixResult {
        file_path: file_path.clone(),
        rule_id: requests
            .iter()
            .map(|r| r.rule_id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        success,
        output,
    })
}

/// Run goose fixes for all pending LLM requests.
/// Groups requests by file path for batch processing.
/// If `log_dir` is provided, saves prompts and responses to JSON files.
pub fn run_all_goose_fixes(
    requests: &[LlmFixRequest],
    verbose: bool,
    log_dir: Option<&std::path::Path>,
) -> Vec<GooseFixResult> {
    // Create log directory if specified
    if let Some(dir) = log_dir {
        let _ = std::fs::create_dir_all(dir);
    }

    // Group by file path for batching
    let mut by_file: BTreeMap<PathBuf, Vec<&LlmFixRequest>> = BTreeMap::new();
    for req in requests {
        by_file.entry(req.file_path.clone()).or_default().push(req);
    }

    let total_files = by_file.len();
    let total_fixes = requests.len();
    eprintln!(
        "  Processing {} fixes across {} files via goose...\n",
        total_fixes, total_files
    );

    let mut results = Vec::new();
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let pipeline_start = std::time::Instant::now();

    for (i, (file_path, file_requests)) in by_file.iter().enumerate() {
        let file_name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file_path.display().to_string());

        let rule_ids: Vec<&str> = file_requests.iter().map(|r| r.rule_id.as_str()).collect();
        let rules_display = if rule_ids.len() <= 3 {
            rule_ids.join(", ")
        } else {
            format!(
                "{}, ... +{} more",
                rule_ids[..2].join(", "),
                rule_ids.len() - 2
            )
        };

        eprintln!(
            "  [{}/{}] {} ({} fixes)",
            i + 1,
            total_files,
            file_name,
            file_requests.len()
        );
        eprintln!("         rules: {}", rules_display);
        eprint!("         goose: running...");

        let file_start = std::time::Instant::now();

        // Split large batches into chunks to avoid overwhelming the LLM.
        // Each chunk runs sequentially -- the LLM reads the file as modified
        // by the previous chunk. A context summary of previously applied
        // fixes is prepended to each subsequent chunk.
        let max_fixes_per_batch = 8;
        let mut result: Result<GooseFixResult> = Ok(GooseFixResult {
            file_path: file_path.clone(),
            rule_id: String::new(),
            success: true,
            output: String::new(),
        });
        let mut all_prompts = Vec::new();
        let mut applied_summaries: Vec<String> = Vec::new();

        if file_requests.len() == 1 {
            result = run_goose_fix(file_requests[0]);
            all_prompts.push(build_prompt(file_requests[0]));
        } else {
            let chunks: Vec<&[&LlmFixRequest]> =
                file_requests.chunks(max_fixes_per_batch).collect();
            let chunk_count = chunks.len();

            for (chunk_idx, chunk) in chunks.iter().enumerate() {
                if chunk_idx > 0 {
                    eprintln!(
                        "\r         goose: chunk {}/{} ({} fixes)...",
                        chunk_idx + 1,
                        chunk_count,
                        chunk.len()
                    );
                }

                let prompt = build_batch_prompt_with_context(
                    file_path,
                    chunk,
                    if applied_summaries.is_empty() {
                        None
                    } else {
                        Some(&applied_summaries)
                    },
                );
                all_prompts.push(prompt.clone());

                let max_turns = (5 + chunk.len()).min(20);
                let max_turns_str = max_turns.to_string();
                let chunk_result = run_goose_with_timeout(&prompt, &max_turns_str);

                match chunk_result {
                    Ok((success, output)) => {
                        // Record what was applied for context in next chunk
                        for req in chunk.iter() {
                            applied_summaries.push(format!(
                                "- {} (line {}): {}",
                                req.rule_id,
                                req.line,
                                req.message.lines().next().unwrap_or("")
                            ));
                        }
                        result = Ok(GooseFixResult {
                            file_path: file_path.clone(),
                            rule_id: file_requests
                                .iter()
                                .map(|r| r.rule_id.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                            success,
                            output,
                        });
                        if !success {
                            eprintln!(
                                "\r         goose: chunk {}/{} failed, stopping",
                                chunk_idx + 1,
                                chunk_count
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        // Retry on timeout
                        if format!("{}", e).contains("timed out") {
                            let backoff = Duration::from_secs(10);
                            eprintln!(
                                "\r         goose: chunk {}/{} timed out, retrying...",
                                chunk_idx + 1,
                                chunk_count
                            );
                            std::thread::sleep(backoff);
                            let retry_result = run_goose_with_timeout(&prompt, &max_turns_str);
                            match retry_result {
                                Ok((success, output)) => {
                                    for req in chunk.iter() {
                                        applied_summaries.push(format!(
                                            "- {} (line {}): {}",
                                            req.rule_id,
                                            req.line,
                                            req.message.lines().next().unwrap_or("")
                                        ));
                                    }
                                    result = Ok(GooseFixResult {
                                        file_path: file_path.clone(),
                                        rule_id: file_requests
                                            .iter()
                                            .map(|r| r.rule_id.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                        success,
                                        output,
                                    });
                                }
                                Err(e2) => {
                                    result = Err(e2);
                                    break;
                                }
                            }
                        } else {
                            result = Err(e);
                            break;
                        }
                    }
                }
            }
        }

        let elapsed = file_start.elapsed();

        // Build the prompt for logging (use the last prompt or combined)
        let prompt_text = all_prompts.join("\n\n--- NEXT CHUNK ---\n\n");

        match result {
            Ok(r) => {
                if r.success {
                    succeeded += 1;
                    eprintln!("\r         goose: ok ({:.1}s)", elapsed.as_secs_f64());
                } else {
                    failed += 1;
                    eprintln!("\r         goose: FAILED ({:.1}s)", elapsed.as_secs_f64());
                }
                if verbose && !r.output.is_empty() {
                    for line in r.output.lines().take(5) {
                        eprintln!("           {}", line);
                    }
                }

                // Save prompt + response to log file
                if let Some(dir) = log_dir {
                    let log_entry = serde_json::json!({
                        "file": file_path.display().to_string(),
                        "rule_ids": file_requests.iter().map(|r| &r.rule_id).collect::<Vec<_>>(),
                        "prompt": prompt_text,
                        "response": r.output,
                        "success": r.success,
                        "elapsed_secs": elapsed.as_secs_f64(),
                    });
                    let log_file = dir.join(format!("goose-fix-{:03}.json", i + 1));
                    let _ = std::fs::write(
                        &log_file,
                        serde_json::to_string_pretty(&log_entry).unwrap_or_default(),
                    );
                }

                results.push(r);
            }
            Err(e) => {
                failed += 1;
                eprintln!(
                    "\r         goose: ERROR ({:.1}s) — {}",
                    elapsed.as_secs_f64(),
                    e
                );
                results.push(GooseFixResult {
                    file_path: file_path.clone(),
                    rule_id: file_requests
                        .iter()
                        .map(|r| r.rule_id.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    success: false,
                    output: format!("Error: {}", e),
                });
            }
        }
        eprintln!();

        // Delay between calls to avoid rate limiting
        if i + 1 < total_files && GOOSE_DELAY_SECS > 0 {
            std::thread::sleep(Duration::from_secs(GOOSE_DELAY_SECS));
        }
    }

    let total_elapsed = pipeline_start.elapsed();
    eprintln!(
        "  Goose complete: {} succeeded, {} failed ({:.0}s total, {:.1}s avg per file)",
        succeeded,
        failed,
        total_elapsed.as_secs_f64(),
        total_elapsed.as_secs_f64() / total_files.max(1) as f64,
    );

    results
}

// ── Prompt construction ───────────────────────────────────────────────────

fn build_prompt(request: &LlmFixRequest) -> String {
    let code_context = request
        .code_snip
        .as_deref()
        .unwrap_or("(no code snippet available)");

    format!(
        r#"You are applying a PatternFly v5 to v6 migration fix.

File: {file_path}
Line: {line}

Migration rule [{rule_id}]:
{message}

Code context around line {line}:
```
{code_context}
```

Instructions:
1. Read the file at {file_path}
2. Apply ONLY the change described by the migration rule at or near line {line}
3. Make the minimum edit necessary — do not change unrelated code
4. Write the fixed file

IMPORTANT constraints:
- NEVER use deep import paths like '@patternfly/react-core/dist/esm/...' or '@patternfly/react-core/next'. Always use the public barrel import '@patternfly/react-core'.
- NEVER replace PatternFly components (Button, MenuToggle, etc.) with raw HTML elements (<button>, <a>, <div>). If a component still exists in PF6, keep using it.
- NEVER remove data-ouia-component-id, ouiaId, or other test identifier props unless the migration rule specifically says to.
- NEVER invent or use component names that are not mentioned in the migration rules or already imported in the file. Only use components explicitly named in the rule message.
- When adding new components (ModalHeader, ModalBody, ModalFooter, etc.), import them from the same package as the parent component.
- If the migration rule says a component was "restructured" or "still exists", keep the component and only restructure its props/children as described. If the rule says a component was "removed" and tells you to remove the import, DO remove it and migrate to the replacement described in the rule.
- When a prop migration says to pass a prop to a child component (e.g., 'actions → pass as children of <ModalFooter>'), you MUST create that child component element, import it, and render the prop value within it.

Do not explain what you're doing. Just read the file, make the edit, and write it."#,
        file_path = request.file_path.display(),
        line = request.line,
        rule_id = request.rule_id,
        message = request.message,
        code_context = code_context,
    )
}

fn build_batch_prompt(file_path: &PathBuf, requests: &[&LlmFixRequest]) -> String {
    build_batch_prompt_with_context(file_path, requests, None)
}

fn build_batch_prompt_with_context(
    file_path: &PathBuf,
    requests: &[&LlmFixRequest],
    previously_applied: Option<&[String]>,
) -> String {
    // Group requests by rule_id so that multiple incidents from the same rule
    // (e.g., a multi-step composition rule firing at different lines) are merged
    // into a single fix instruction with all affected lines listed together.
    // This gives the LLM the full picture instead of seeing the same message
    // repeated as separate fixes with narrow code snippets.
    let mut by_rule: std::collections::BTreeMap<&str, Vec<&LlmFixRequest>> =
        std::collections::BTreeMap::new();
    for req in requests {
        by_rule.entry(&req.rule_id).or_default().push(req);
    }

    let mut fixes = String::new();
    let mut fix_num = 0;
    for (rule_id, rule_requests) in &by_rule {
        fix_num += 1;

        if rule_requests.len() == 1 {
            // Single incident -- show as before
            let req = rule_requests[0];
            let code_context = req.code_snip.as_deref().unwrap_or("(no snippet)");
            fixes.push_str(&format!(
                r#"
### Fix {num}
Line: {line}
Rule [{rule_id}]:
{message}

Code context:
```
{code_context}
```
"#,
                num = fix_num,
                line = req.line,
                rule_id = rule_id,
                message = req.message,
                code_context = code_context,
            ));
        } else {
            // Multiple incidents from the same rule -- merge into one fix
            // with all affected lines and code contexts combined
            let lines: Vec<String> = rule_requests.iter().map(|r| r.line.to_string()).collect();
            let mut all_snippets = String::new();
            for req in rule_requests {
                if let Some(snip) = &req.code_snip {
                    all_snippets.push_str(&format!("  (line {}):\n{}\n", req.line, snip));
                }
            }
            // Use the message from the first request (they're all the same)
            let message = &rule_requests[0].message;
            fixes.push_str(&format!(
                r#"
### Fix {num}
Lines: {lines}
Rule [{rule_id}]:
{message}

This rule affects multiple locations in the file. Apply ALL steps together as one logical change.

Code contexts:
```
{all_snippets}```
"#,
                num = fix_num,
                lines = lines.join(", "),
                rule_id = rule_id,
                message = message,
                all_snippets = all_snippets,
            ));
        }
    }

    let context_section = if let Some(applied) = previously_applied {
        format!(
            "\n## Previously applied fixes (DO NOT revert these):\n\
             The following fixes were already applied to this file in a previous pass.\n\
             The file on disk already reflects these changes. Do NOT undo them.\n\
             {}\n\n",
            applied.join("\n")
        )
    } else {
        String::new()
    };

    format!(
        r#"You are applying PatternFly v5 to v6 migration fixes to a single file.

File: {file_path}
{context_section}
Apply ALL of the following {count} fixes to this file:
{fixes}
Instructions:
1. Read the file at {file_path}
2. Apply ALL {count} fixes described above
3. Make the minimum edits necessary — do not change unrelated code
4. Do NOT revert any changes that were already applied in previous passes
5. Write the fixed file once with all changes applied

IMPORTANT constraints:
- NEVER use deep import paths like '@patternfly/react-core/dist/esm/...' or '@patternfly/react-core/next'. Always use the public barrel import '@patternfly/react-core'.
- NEVER replace PatternFly components (Button, MenuToggle, etc.) with raw HTML elements (<button>, <a>, <div>). If a component still exists in PF6, keep using it.
- NEVER remove data-ouia-component-id, ouiaId, or other test identifier props unless the migration rule specifically says to.
- NEVER invent or use component names that are not mentioned in the migration rules or already imported in the file. Only use components explicitly named in the rule message.
- When adding new components (ModalHeader, ModalBody, ModalFooter, etc.), import them from the same package as the parent component.
- If the migration rule says a component was "restructured" or "still exists", keep the component and only restructure its props/children as described. If the rule says a component was "removed" and tells you to remove the import, DO remove it and migrate to the replacement described in the rule.
- When a prop migration says to pass a prop to a child component (e.g., 'actions → pass as children of <ModalFooter>'), you MUST create that child component element, import it, and render the prop value within it.

Do not explain what you're doing. Just read the file, make the edits, and write it."#,
        file_path = file_path.display(),
        context_section = context_section,
        count = fix_num,
        fixes = fixes,
    )
}
