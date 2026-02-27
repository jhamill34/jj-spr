/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{
    error::{Error, Result, ResultExt},
    output::{output, write_commit_title},
};

#[derive(Debug, clap::Parser)]
pub struct ArtifactsOptions {
    /// Jujutsu revision to operate on (if not specified, uses '@')
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Download all artifacts without prompting for selection
    #[clap(long)]
    all: bool,

    /// Directory to download artifacts into (default: .spr-artifacts)
    #[clap(long, default_value = ".spr-artifacts")]
    output_dir: String,
}

// ── GitHub Actions REST API response shapes ──────────────────────────────────

#[derive(Deserialize, Debug)]
struct WorkflowRunsResponse {
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Deserialize, Debug)]
struct WorkflowRun {
    id: u64,
    name: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ArtifactsResponse {
    artifacts: Vec<Artifact>,
}

#[derive(Deserialize, Debug)]
struct Artifact {
    id: u64,
    name: String,
    size_in_bytes: u64,
    expired: bool,
}

// ─────────────────────────────────────────────────────────────────────────────

pub async fn artifacts(
    opts: ArtifactsOptions,
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    auth_token: &str,
) -> Result<()> {
    let revision = opts.revision.as_deref().unwrap_or("@");
    let prepared_commit = jj.get_prepared_commit_for_revision(config, revision)?;

    write_commit_title(&prepared_commit)?;

    let pull_request_number = if let Some(number) = prepared_commit.pull_request_number {
        output("#️⃣ ", &format!("Pull Request #{}", number))?;
        number
    } else {
        return Err(Error::new("This commit does not refer to a Pull Request."));
    };

    // We need the PR's head SHA to look up the correct workflow runs.
    // Fetch it from GitHub via git rev-parse of the PR's head ref.
    let head_sha = get_pr_head_sha(config, pull_request_number).await?;

    output("🔍", &format!("Looking up workflow runs for {}", &head_sha[..8]))?;

    // Build a reqwest client with auth for GitHub API calls.
    let client = build_http_client(auth_token)?;

    // Fetch all workflow runs for this commit SHA.
    let runs = fetch_workflow_runs(config, &client, &head_sha).await?;

    if runs.is_empty() {
        output("ℹ️ ", "No workflow runs found for this Pull Request's head commit.")?;
        return Ok(());
    }

    // Collect all artifacts across every run.
    let mut all_artifacts: Vec<(u64, String, u64, &WorkflowRun)> = Vec::new();
    for run in &runs {
        let run_artifacts = fetch_run_artifacts(config, &client, run.id).await?;
        for artifact in run_artifacts {
            if !artifact.expired {
                all_artifacts.push((artifact.id, artifact.name, artifact.size_in_bytes, run));
            }
        }
    }

    if all_artifacts.is_empty() {
        output("ℹ️ ", "No downloadable artifacts found for this Pull Request.")?;
        return Ok(());
    }

    // Decide which artifacts to download.
    let to_download: Vec<_> = if opts.all {
        all_artifacts.iter().collect()
    } else {
        select_artifacts(&all_artifacts)?
    };

    if to_download.is_empty() {
        output("ℹ️ ", "No artifacts selected.")?;
        return Ok(());
    }

    // Ensure the output directory exists and is gitignored.
    let output_dir = PathBuf::from(&opts.output_dir);
    let pr_dir = output_dir.join(pull_request_number.to_string());
    std::fs::create_dir_all(&pr_dir).convert()?;
    ensure_gitignored(&output_dir)?;

    output(
        "📦",
        &format!(
            "Downloading {} artifact(s) to {}/",
            to_download.len(),
            pr_dir.display()
        ),
    )?;

    for (artifact_id, name, size, run) in &to_download {
        let run_name = run.name.as_deref().unwrap_or("unknown");
        output(
            "  ⬇️ ",
            &format!("{} (from '{}', {} bytes)", name, run_name, size),
        )?;

        let zip_bytes = download_artifact(config, &client, *artifact_id).await?;

        // Write the ZIP archive to disk.
        let zip_path = pr_dir.join(format!("{}.zip", name));
        std::fs::write(&zip_path, &zip_bytes).convert()?;

        // Extract the ZIP into a subdirectory named after the artifact.
        let extract_dir = pr_dir.join(name.as_str());
        std::fs::create_dir_all(&extract_dir).convert()?;
        extract_zip(&zip_bytes, &extract_dir)?;

        output(
            "  ✅",
            &format!("Extracted to {}/", extract_dir.display()),
        )?;
    }

    output("🎉", "Done! Artifacts are in the output directory.")?;
    output(
        "ℹ️ ",
        &format!(
            "Note: '{}/' is gitignored and will not be checked in.",
            opts.output_dir
        ),
    )?;

    Ok(())
}

/// Retrieve the head SHA for a PR using the GitHub REST API.
async fn get_pr_head_sha(config: &crate::config::Config, pr_number: u64) -> Result<String> {
    #[derive(Deserialize)]
    struct PrHeadRef {
        sha: String,
    }
    #[derive(Deserialize)]
    struct PrResponse {
        head: PrHeadRef,
    }

    let pr: PrResponse = octocrab::instance()
        .get(
            format!(
                "/repos/{}/{}/pulls/{}",
                config.owner, config.repo, pr_number
            ),
            None::<&()>,
        )
        .await
        .map_err(Error::from)?;

    Ok(pr.head.sha)
}

/// Build a reqwest::Client with the GitHub auth token and a user-agent header.
fn build_http_client(auth_token: &str) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", auth_token)
            .parse()
            .map_err(|_| Error::new("invalid auth token".to_string()))?,
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        format!("jj-spr/{}", env!("CARGO_PKG_VERSION"))
            .parse()
            .map_err(|_| Error::new("invalid user-agent".to_string()))?,
    );
    headers.insert(
        reqwest::header::ACCEPT,
        "application/vnd.github+json"
            .parse()
            .map_err(|_| Error::new("invalid accept header".to_string()))?,
    );

    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| Error::new(format!("failed to build HTTP client: {}", e)))
}

/// List all workflow runs for a commit SHA.
async fn fetch_workflow_runs(
    config: &crate::config::Config,
    client: &reqwest::Client,
    head_sha: &str,
) -> Result<Vec<WorkflowRun>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/actions/runs?head_sha={}&per_page=100",
        config.owner, config.repo, head_sha,
    );

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(Error::from)?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(Error::new(format!(
            "GitHub API error listing workflow runs (HTTP {}): {}",
            status, body
        )));
    }

    let data: WorkflowRunsResponse = response.json().await.map_err(Error::from)?;
    Ok(data.workflow_runs)
}

/// List all artifacts for a workflow run.
async fn fetch_run_artifacts(
    config: &crate::config::Config,
    client: &reqwest::Client,
    run_id: u64,
) -> Result<Vec<Artifact>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/actions/runs/{}/artifacts?per_page=100",
        config.owner, config.repo, run_id,
    );

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(Error::from)?;

    if !response.status().is_success() {
        // Non-fatal: a run might not have artifacts.
        return Ok(Vec::new());
    }

    let data: ArtifactsResponse = response.json().await.map_err(Error::from)?;
    Ok(data.artifacts)
}

/// Download an artifact ZIP from GitHub, returning the raw bytes.
/// GitHub returns a 302 redirect; reqwest follows it automatically.
async fn download_artifact(
    config: &crate::config::Config,
    client: &reqwest::Client,
    artifact_id: u64,
) -> Result<Vec<u8>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/actions/artifacts/{}/zip",
        config.owner, config.repo, artifact_id,
    );

    let response = client
        .get(&url)
        // Override Accept for binary download
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .map_err(Error::from)?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(Error::new(format!(
            "GitHub API error downloading artifact (HTTP {}): {}",
            status, body
        )));
    }

    let bytes = response.bytes().await.map_err(Error::from)?;
    Ok(bytes.to_vec())
}

/// Interactively ask the user which artifacts to download.
fn select_artifacts<'a>(
    all: &'a [(u64, String, u64, &WorkflowRun)],
) -> Result<Vec<&'a (u64, String, u64, &'a WorkflowRun)>> {
    use dialoguer::MultiSelect;

    let labels: Vec<String> = all
        .iter()
        .map(|(_, name, size, run)| {
            format!(
                "{} (from '{}', {} bytes)",
                name,
                run.name.as_deref().unwrap_or("unknown"),
                size
            )
        })
        .collect();

    let selections = MultiSelect::new()
        .with_prompt("Select artifacts to download (space to toggle, enter to confirm)")
        .items(&labels)
        .interact()
        .map_err(|e| Error::new(format!("selection error: {}", e)))?;

    Ok(selections.into_iter().map(|i| &all[i]).collect())
}

/// Extract the contents of a ZIP archive (as raw bytes) into `dest_dir`.
fn extract_zip(zip_bytes: &[u8], dest_dir: &Path) -> Result<()> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| Error::new(format!("invalid zip: {}", e)))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| Error::new(format!("zip read error: {}", e)))?;

        // Sanitize path to prevent directory traversal.
        let out_path = match file.enclosed_name() {
            Some(p) => dest_dir.join(p),
            None => continue,
        };

        if file.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| Error::new(format!("failed to create directory: {}", e)))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::new(format!("failed to create directory: {}", e)))?;
            }
            let mut out_file = std::fs::File::create(&out_path)
                .map_err(|e| Error::new(format!("failed to create file: {}", e)))?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .map_err(|e| Error::new(format!("failed to read zip entry: {}", e)))?;
            out_file
                .write_all(&buf)
                .map_err(|e| Error::new(format!("failed to write file: {}", e)))?;
        }
    }

    Ok(())
}

/// Ensure `dir_name/` is listed in `.gitignore` (which jj also respects).
/// Creates `.gitignore` if it does not yet exist.
fn ensure_gitignored(output_dir: &Path) -> Result<()> {
    // Use the directory name as the pattern (relative to repo root).
    let dir_name = output_dir
        .to_str()
        .unwrap_or(".spr-artifacts")
        .trim_end_matches('/');
    let pattern = format!("{}/", dir_name);

    // Try to find an existing .gitignore in the current working directory.
    let gitignore_path = Path::new(".gitignore");

    if gitignore_path.exists() {
        let content = std::fs::read_to_string(gitignore_path)
            .map_err(|e| Error::new(format!("failed to read .gitignore: {}", e)))?;

        // Check if the pattern is already present.
        if content.lines().any(|l| l.trim() == pattern.trim_end_matches('/') || l.trim() == pattern) {
            return Ok(());
        }

        // Append the pattern.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(gitignore_path)
            .map_err(|e| Error::new(format!("failed to open .gitignore: {}", e)))?;

        let entry = if content.ends_with('\n') || content.is_empty() {
            format!("{}\n", pattern)
        } else {
            format!("\n{}\n", pattern)
        };

        file.write_all(entry.as_bytes())
            .map_err(|e| Error::new(format!("failed to write .gitignore: {}", e)))?;
    } else {
        // Create a new .gitignore.
        std::fs::write(gitignore_path, format!("{}\n", pattern))
            .map_err(|e| Error::new(format!("failed to create .gitignore: {}", e)))?;
    }

    Ok(())
}
