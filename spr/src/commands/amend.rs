/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::{
    error::{Error, Result},
    jj::PreparedCommit,
    message::validate_commit_message,
    output::{output, write_commit_title},
};
use git2::Oid;

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

    /// Sync code changes pushed to the PR branch by external tooling (e.g. a
    /// GitHub Action) back into the local JJ change, in addition to syncing
    /// the commit message.
    #[clap(long)]
    sync: bool,
}

pub async fn amend(
    opts: AmendOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
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

    // Request the Pull Request information for each commit (well, those that
    // declare to have Pull Requests).
    let pull_requests: Vec<_> = pc
        .iter()
        .map(|commit: &PreparedCommit| {
            commit
                .pull_request_number
                .map(|number| tokio::spawn(gh.clone().get_pull_request(number)))
        })
        .collect();

    let mut failure = false;

    for (commit, pull_request) in pc.iter_mut().zip(pull_requests.into_iter()) {
        write_commit_title(commit)?;
        if let Some(pull_request) = pull_request {
            let pull_request = pull_request.await??;

            if opts.sync {
                sync_code_from_pr(jj, commit.oid, pull_request.head_oid)?;
            }

            commit.message = pull_request.sections;
            // Strip any stack navigation block that was appended to the PR
            // body on GitHub — it should not be stored in the local commit
            // message's Summary section.
            if let Some(summary) =
                commit.message.get_mut(&crate::message::MessageSection::Summary)
            {
                let cleaned =
                    crate::github::strip_stack_nav(summary.trim()).trim().to_string();
                *summary = cleaned;
            }
            commit.message_changed = true;
        }
        failure = validate_commit_message(&commit.message).is_err() || failure;
    }
    jj.rewrite_commit_messages(&mut pc)?;

    if failure { Err(Error::empty()) } else { Ok(()) }
}

fn sync_code_from_pr(
    jj: &crate::jj::Jujutsu,
    local_commit_oid: Oid,
    remote_head_oid: Oid,
) -> Result<()> {
    // Walk back from the remote head to find the last commit pushed by
    // jj-spr. Commits added on top of it by external tooling (GitHub
    // Actions, etc.) are the ones we want to absorb.
    let last_spr_oid = match jj.find_last_spr_commit(remote_head_oid) {
        Some(oid) => oid,
        None => {
            output("⚠️", "Could not find a jj-spr commit on the remote branch — skipping code sync")?;
            return Ok(());
        }
    };

    if last_spr_oid == remote_head_oid {
        output("✅", "No remote code changes to sync")?;
        return Ok(());
    }

    let changed_files = jj.get_action_changed_files(last_spr_oid, remote_head_oid)?;
    if changed_files.is_empty() {
        output("✅", "No remote code changes to sync")?;
        return Ok(());
    }

    let change_id = jj.get_change_id_for_commit(local_commit_oid)?;
    jj.restore_from_remote(&remote_head_oid.to_string(), &changed_files, &change_id)?;

    output(
        "🔄",
        &format!(
            "Synced {} file(s) from remote PR branch into local change",
            changed_files.len()
        ),
    )?;
    Ok(())
}
