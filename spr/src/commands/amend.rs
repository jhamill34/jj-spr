/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use futures::future;

use crate::{
    error::{Error, Result},
    github::gh_trait::GitHubClient,
    message::validate_commit_message,
    output::{output, write_commit_title},
};

#[derive(Debug, clap::Parser)]
pub struct AmendOptions {
    /// Amend commits in range from base to revision
    #[clap(long, short = 'a')]
    all: bool,

    /// Base revision for --all mode (if not specified, uses trunk)
    #[clap(long)]
    base: Option<String>,

    /// Jujutsu revision(s) to operate on. Can be a single revision like '@' or a range like 'main..@' or 'a::c'.
    /// If a range is provided, behaves like --all mode. If not specified, uses '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,
}

pub async fn amend<G>(
    opts: AmendOptions,
    jj: &crate::jj::Jujutsu,
    gh: &G,
    config: &crate::config::Config,
) -> Result<()>
where
    G: GitHubClient,
{
    // Determine revision and whether to use range mode
    let (use_range_mode, base_rev, target_rev, is_inclusive) =
        crate::revision_utils::parse_revision_and_range(
            opts.revision.as_deref(),
            opts.all,
            opts.base.as_deref(),
        )?;

    let mut pc = if use_range_mode {
        jj.get_prepared_commits_from_to(config, &base_rev, &target_rev, is_inclusive)?
    } else {
        vec![jj.get_prepared_commit_for_revision(config, &target_rev)?]
    };

    if pc.is_empty() {
        output("👋", "No commits found - nothing to do. Good bye!")?;
        return Ok(());
    }

    let pull_request_futures: Vec<_> = pc
        .iter()
        .flat_map(|commit| commit.pull_request_number)
        .map(|number| gh.get_pull_request(number, config))
        .collect();

    let pull_requests = future::join_all(pull_request_futures).await;

    let mut failure = false;

    for (commit, pull_request) in pc.iter_mut().zip(pull_requests.into_iter()) {
        write_commit_title(commit)?;
        commit.message = pull_request?.sections;
        commit.message_changed = true;
        failure = validate_commit_message(&commit.message).is_err() || failure;
    }
    jj.rewrite_commit_messages(&mut pc)?;

    if failure { Err(Error::empty()) } else { Ok(()) }
}
