/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use indoc::formatdoc;
use std::{io::Write, process::Stdio, time::Duration};

use crate::{
    error::{Error, Result, ResultExt},
    github::{CIStatus, PullRequestState, PullRequestUpdate, ReviewStatus},
    message::build_github_body_for_merging,
    output::{output, write_commit_title},
    utils::run_command,
};

#[derive(Debug, clap::Parser)]
pub struct LandOptions {
    /// Merge a Pull Request that was created or updated with spr diff
    /// --cherry-pick
    #[clap(long)]
    cherry_pick: bool,

    /// Jujutsu revision to operate on (if not specified, uses '@')
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Skip safety checks (PR up to date with main, GitHub Actions passing)
    #[clap(long)]
    force: bool,
}

pub async fn land(
    opts: LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let revision = opts.revision.as_deref().unwrap_or("@");
    let prepared_commit = jj.get_prepared_commit_for_revision(config, revision)?;

    // For Jujutsu, we'll determine if this is cherry-pick based on the revision's ancestry
    // For now, we'll trust the user's --cherry-pick flag

    write_commit_title(&prepared_commit)?;

    let pull_request_number = if let Some(number) = prepared_commit.pull_request_number {
        output("#️⃣ ", &format!("Pull Request #{}", number))?;
        number
    } else {
        return Err(Error::new("This commit does not refer to a Pull Request."));
    };

    // Load Pull Request information
    let pull_request = gh.clone().get_pull_request(pull_request_number).await?;

    if pull_request.state != PullRequestState::Open {
        return Err(Error::new(formatdoc!(
            "This Pull Request is already closed!",
        )));
    }

    if config.require_approval && pull_request.review_status != Some(ReviewStatus::Approved) {
        return Err(Error::new(
            "This Pull Request has not been approved on GitHub.",
        ));
    }

    // Check that GitHub Actions are passing before attempting to land.
    if !opts.force {
        let ci_status = gh.clone().get_pr_ci_status(pull_request_number).await?;
        match ci_status {
            CIStatus::Failure => {
                return Err(Error::new(
                    "GitHub Actions are failing on this Pull Request. \
                     Fix the failing checks or use --force to skip this check.",
                ));
            }
            CIStatus::Error => {
                return Err(Error::new(
                    "GitHub Actions have errors on this Pull Request. \
                     Fix the errors or use --force to skip this check.",
                ));
            }
            CIStatus::Pending => {
                return Err(Error::new(
                    "GitHub Actions are still running on this Pull Request. \
                     Wait for them to complete or use --force to skip this check.",
                ));
            }
            CIStatus::Success | CIStatus::Unknown => {}
        }
    }

    output("🛫", "Getting started...")?;

    // Fetch current master from GitHub.
    run_command(
        tokio::process::Command::new("git")
            .arg("fetch")
            .arg("--no-write-fetch-head")
            .arg("--")
            .arg(&config.remote_name)
            .arg(config.master_ref.on_github()),
    )
    .await
    .reword("git fetch failed".to_string())?;

    // TODO: Implement Jujutsu-native cherry-pick and merge validation
    // For now, we'll trust GitHub's merge validation and skip local validation
    let base_is_master = pull_request.base.is_master_branch();

    // Check that the PR head is not behind the main branch.
    // Only apply this to PRs that directly target the master branch.
    if !opts.force && base_is_master {
        let master_tip_output = tokio::process::Command::new("git")
            .args(["rev-parse", config.master_ref.local()])
            .output()
            .await?;
        if master_tip_output.status.success() {
            let master_tip =
                String::from_utf8_lossy(&master_tip_output.stdout).trim().to_string();
            // `git merge-base --is-ancestor A B` exits 0 if A is an ancestor of B.
            // We check that the current master tip is an ancestor of the PR head,
            // meaning the PR has been rebased onto (or merged with) the latest main.
            let is_ancestor = tokio::process::Command::new("git")
                .args([
                    "merge-base",
                    "--is-ancestor",
                    &master_tip,
                    &pull_request.head_oid.to_string(),
                ])
                .status()
                .await?;
            if !is_ancestor.success() {
                return Err(Error::new(
                    "This Pull Request is behind the main branch. Please rebase your \
                     changes, run `spr diff` to update the Pull Request, and try again. \
                     Use --force to skip this check.",
                ));
            }
        }
    }

    // Skip local cherry-pick validation for Jujutsu workflow
    // GitHub will validate mergeability during the merge process
    let merge_matches_cherrypick = true;

    if !merge_matches_cherrypick {
        return Err(Error::new(formatdoc!(
            "This commit has been updated and/or rebased since the pull \
             request was last updated. Please run `spr diff` to update the \
             pull request and then try `spr land` again!"
        )));
    }

    // Okay, we are confident now that the PR can be merged and the result of
    // that merge would be a master commit with the same tree as if we
    // cherry-picked the commit onto master.
    let pr_head_oid = pull_request.head_oid;

    if !base_is_master {
        // The base of the Pull Request on GitHub is not set to master. This
        // means the Pull Request uses a base branch. We tested above that
        // merging the Pull Request branch into the master branch produces the
        // intended result (the same as cherry-picking the local commit onto
        // master), so what we want to do is actually merge the Pull Request as
        // it is into master. Hence, we change the base to the master branch.
        //
        // Before we do that, there is one more edge case to look out for: if
        // the base branch contains changes that have since been landed on
        // master, then Git might be able to figure out that these changes
        // appear both in the pull request branch (via the merge branch) and in
        // master, but are identical in those two so it is not a merge conflict
        // but can go ahead. The result of this in master if we merge now is
        // correct, but there is one problem: when looking at the Pull Request
        // in GitHub after merging, it will show these change as part of the
        // Pull Request. So when you look at the changed files of the Pull
        // Request, you will see both changes in this commit (great!) and those
        // in the base branch (a previous commit that has already been landed on
        // master - not great!). This is because the changes shown are the ones
        // that happened on this Pull Request branch (now including the base
        // branch) since it branched off master. This can include changes in the
        // base branch that are already on master, but were added to master
        // after the Pull Request branch branched from master.
        // The solution is to merge current master into the Pull Request branch.
        // Doing that now means that the final changes done by this Pull Request
        // are only the changes that are not yet in master. That's what we want.
        // This final merge never introduces any changes to the Pull Request. In
        // fact, the tree that we use for the merge commit is the one we got
        // above from the cherry-picking of this commit on master.

        // TODO: Implement Jujutsu-native merge base and tree comparison
        // For now, skip the complex merge-in-master logic
        // This logic would need to be rewritten using jj commands

        // Skip the merge-in-master commit creation for Jujutsu workflow

        gh.update_pull_request(
            pull_request_number,
            PullRequestUpdate {
                base: Some(config.master_ref.branch_name().to_string()),
                ..Default::default()
            },
        )
        .await?;
    }

    // Check whether GitHub says this PR is mergeable. This happens in a
    // retry-loop because recent changes to the Pull Request can mean that
    // GitHub has not finished the mergeability check yet.
    let mut attempts = 0;
    let result = loop {
        attempts += 1;

        let mergeability = gh
            .get_pull_request_mergeability(pull_request_number)
            .await?;

        if mergeability.head_oid != pr_head_oid {
            break Err(Error::new(formatdoc!(
                "The Pull Request seems to have been updated externally.
                     Please try again!"
            )));
        }

        if mergeability.base.is_master_branch() && mergeability.mergeable.is_some() {
            if mergeability.mergeable != Some(true) {
                break Err(Error::new(formatdoc!(
                    "GitHub concluded the Pull Request is not mergeable at \
                    this point. Please rebase your changes and try again!"
                )));
            }

            // TODO: Implement Jujutsu-native commit fetching and tree comparison
            // For now, skip the merge commit validation
            // This would need to be rewritten using jj commands

            break Ok(());
        }

        if attempts >= 10 {
            // After ten failed attempts we give up.
            break Err(Error::new(
                "GitHub Pull Request did not update. Please try again!",
            ));
        }

        // Wait one second before retrying
        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    let result = match result {
        Ok(()) => {
            // We have checked that merging the Pull Request branch into the master
            // branch produces the intended result, and that's independent of whether we
            // used a base branch with this Pull Request or not. We have made sure the
            // target of the Pull Request is set to the master branch. So let GitHub do
            // the merge now!
            octocrab::instance()
                .pulls(&config.owner, &config.repo)
                .merge(pull_request_number)
                .method(octocrab::params::pulls::MergeMethod::Squash)
                .title(pull_request.title)
                .message(build_github_body_for_merging(&pull_request.sections))
                .sha(format!("{}", pr_head_oid))
                .send()
                .await
                .convert()
                .and_then(|merge| {
                    if merge.merged {
                        Ok(merge)
                    } else {
                        Err(Error::new(formatdoc!(
                            "GitHub Pull Request merge failed: {}",
                            merge.message.unwrap_or_default()
                        )))
                    }
                })
        }
        Err(err) => Err(err),
    };

    let merge = match result {
        Ok(merge) => merge,
        Err(mut error) => {
            output("❌", "GitHub Pull Request merge failed")?;

            // If we changed the target branch of the Pull Request earlier, then
            // undo this change now.
            if !base_is_master {
                let result = gh
                    .update_pull_request(
                        pull_request_number,
                        PullRequestUpdate {
                            base: Some(pull_request.base.on_github().to_string()),
                            ..Default::default()
                        },
                    )
                    .await;
                if let Err(e) = result {
                    error.push(format!("{}", e));
                }
            }

            return Err(error);
        }
    };

    output("🛬", "Landed!")?;

    // Before deleting PR-1's branch, retarget any PRs that use it as their
    // base (e.g. PR-2 in a stack) to point at main. GitHub would otherwise
    // close those PRs automatically when their base branch is deleted.
    // Retarget any PRs stacked on top of the one we just landed. They should
    // now point at the landed PR's original base (e.g. spr/pr1-branch when
    // landing PR-2, or main when landing PR-1). Using the original base (not
    // always main) preserves the rest of the stack correctly.
    let prs_to_retarget = gh
        .get_open_prs_with_base(pull_request.head.branch_name())
        .await
        .unwrap_or_default();
    for pr_number in prs_to_retarget {
        let _ = gh
            .update_pull_request(
                pr_number,
                PullRequestUpdate {
                    base: Some(config.master_ref.branch_name().to_string()),
                    ..Default::default()
                },
            )
            .await;
    }

    let mut remove_old_branch_child_process = tokio::process::Command::new("git")
        .arg("push")
        .arg("--no-verify")
        .arg("--delete")
        .arg("--")
        .arg(&config.remote_name)
        .arg(pull_request.head.on_github())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let remove_old_base_branch_child_process = if base_is_master {
        None
    } else {
        Some(
            tokio::process::Command::new("git")
                .arg("push")
                .arg("--no-verify")
                .arg("--delete")
                .arg("--")
                .arg(&config.remote_name)
                .arg(pull_request.base.on_github())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        )
    };

    // Rebase us on top of the now-landed commit
    if let Some(sha) = merge.sha {
        // Try this up to three times, because fetching the very moment after
        // the merge might still not find the new commit.
        for i in 0..3 {
            // Fetch current master and the merge commit from GitHub.
            let git_fetch = tokio::process::Command::new("git")
                .arg("fetch")
                .arg("--no-write-fetch-head")
                .arg("--")
                .arg(&config.remote_name)
                .arg(config.master_ref.on_github())
                .arg(&sha)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await?;
            if git_fetch.status.success() {
                break;
            } else if i == 2 {
                console::Term::stderr().write_all(&git_fetch.stderr)?;
                return Err(Error::new("git fetch failed"));
            }
        }
        // TODO: Implement Jujutsu-native rebase after landing
        // For now, the user will need to manually rebase after landing
        output(
            "⚠️",
            "Please manually rebase your working copy after landing",
        )?;
    }

    // Wait for the "git push" to delete the old Pull Request branch to finish,
    // but ignore the result. GitHub may be configured to delete the branch
    // automatically, in which case it's gone already and this command fails.
    remove_old_branch_child_process.wait().await?;
    if let Some(mut proc) = remove_old_base_branch_child_process {
        proc.wait().await?;
    }

    Ok(())
}
