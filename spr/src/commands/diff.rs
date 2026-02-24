/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::{iter::zip, path::Path};

use crate::{
    config::StackStrategy,
    error::{Error, Result, ResultExt, add_error},
    github::{
        GitHub, GitHubBranch, PullRequest, PullRequestRequestReviewers, PullRequestState,
        PullRequestUpdate,
    },
    message::{MessageSection, validate_commit_message},
    output::{output, write_commit_title},
    utils::{parse_name_list, remove_all_parens, run_command},
};
use git2::Oid;
use indoc::{formatdoc, indoc};

#[derive(Debug, clap::Parser)]
pub struct DiffOptions {
    /// Create/update pull requests for commits in range from base to revision
    #[clap(long, short = 'a')]
    all: bool,

    /// Update the pull request title and description on GitHub from the local
    /// commit message
    #[clap(long)]
    update_message: bool,

    /// Submit any new Pull Request as a draft
    #[clap(long)]
    draft: bool,

    /// Message to be used for commits updating existing pull requests (e.g.
    /// 'rebase' or 'review comments')
    #[clap(long, short = 'm')]
    message: Option<String>,

    /// Submit this commit as if it was cherry-picked on master. Do not base it
    /// on any intermediate changes between the master branch and this commit.
    #[clap(long)]
    cherry_pick: bool,

    /// Base revision for --all mode (if not specified, uses trunk)
    #[clap(long)]
    base: Option<String>,

    /// Jujutsu revision(s) to operate on. Can be a single revision like '@' or a range like 'main..@' or 'a::c'.
    /// If a range is provided, behaves like --all mode. If not specified, uses '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Skip opening $EDITOR for new PR descriptions (useful in CI / automation)
    #[clap(long)]
    no_edit: bool,
}

/// Strip the spr context comment that was prepended to the editor buffer.
/// Only removes a leading `<!-- spr: ...` block so user-written HTML comments
/// elsewhere in the body are left intact.
fn strip_context_comment(body: &str) -> &str {
    if body.starts_with("<!-- spr:")
        && let Some(end) = body.find("-->")
    {
        return body[end + 3..].trim_start();
    }
    body
}

/// Discover PR templates in the repository's `.github/` directory.
/// Returns a list of `(display_name, content)` pairs sorted by name.
/// Checks single-file locations first; falls back to the multi-template dir.
fn find_pr_templates(repo_path: &Path) -> Vec<(String, String)> {
    // Single-file locations (checked in order)
    for rel in &[
        ".github/PULL_REQUEST_TEMPLATE.md",
        ".github/pull_request_template.md",
        "PULL_REQUEST_TEMPLATE.md",
    ] {
        if let Ok(content) = std::fs::read_to_string(repo_path.join(rel)) {
            return vec![("default".to_string(), content)];
        }
    }
    // Directory with multiple templates
    let dir = repo_path.join(".github/PULL_REQUEST_TEMPLATE");
    if dir.is_dir() {
        let mut templates: Vec<(String, String)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension()?.to_str()? == "md" {
                    let name = path.file_name()?.to_str()?.to_string();
                    let content = std::fs::read_to_string(&path).ok()?;
                    Some((name, content))
                } else {
                    None
                }
            })
            .collect();
        templates.sort_by(|a, b| a.0.cmp(&b.0));
        if !templates.is_empty() {
            return templates;
        }
    }
    Vec::new()
}

pub async fn diff(
    opts: DiffOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let mut result = Ok(());

    // Determine revision and whether to use range mode
    let (use_range_mode, base_rev, target_rev, is_inclusive) =
        crate::revision_utils::parse_revision_and_range(
            opts.revision.as_deref(),
            opts.all,
            opts.base.as_deref(),
        )?;

    // Get commits to process
    let mut prepared_commits = if use_range_mode {
        // Get range of commits from base to target
        jj.get_prepared_commits_from_to(config, &base_rev, &target_rev, is_inclusive)?
    } else {
        // Just get the single specified revision
        vec![jj.get_prepared_commit_for_revision(config, &target_rev)?]
    };

    // Determine the master base OID - this is the commit on master that the stack is based on
    let master_base_oid = if let Some(first_commit) = prepared_commits.first() {
        if use_range_mode {
            // For range mode, the parent of the first commit is the master base
            first_commit.parent_oid
        } else {
            // For single commit mode, find the actual merge base with master
            jj.get_master_base_for_commit(config, first_commit.oid)?
        }
    } else {
        output("👋", "No commits found - nothing to do. Good bye!")?;
        return result;
    };

    #[allow(clippy::needless_collect)]
    let pull_request_tasks: Vec<_> = prepared_commits
        .iter()
        .map(|pc: &crate::jj::PreparedCommit| {
            pc.pull_request_number
                .map(|number| tokio::spawn(gh.clone().get_pull_request(number)))
        })
        .collect();

    let mut message_on_prompt = "".to_string();
    // Tracks the previous PR's branch and head commit OID so each PR in a stack
    // can target the one before it as its GitHub base.
    let mut prev_pr_info: Option<(GitHubBranch, Oid)> = None;
    // Collects (pr_number, title) for the stack nav second pass.
    let mut stack_prs: Vec<(u64, String)> = Vec::new();

    for (prepared_commit, pull_request_task) in
        zip(prepared_commits.iter_mut(), pull_request_tasks.into_iter())
    {
        if result.is_err() {
            break;
        }

        let pull_request = if let Some(task) = pull_request_task {
            Some(task.await??)
        } else {
            None
        };

        write_commit_title(prepared_commit)?;

        // The further implementation of the diff command is in a separate function.
        // This makes it easier to run the code to update the local commit message
        // with all the changes that the implementation makes at the end, even if
        // the implementation encounters an error or exits early.
        match diff_impl(
            &opts,
            &mut message_on_prompt,
            jj,
            gh,
            config,
            prepared_commit,
            master_base_oid,
            pull_request,
            prev_pr_info.take(),
        )
        .await
        {
            Ok((branch, oid, pr_number)) => {
                prev_pr_info = Some((branch, oid));
                let title = prepared_commit
                    .message
                    .get(&MessageSection::Title)
                    .cloned()
                    .unwrap_or_default();
                stack_prs.push((pr_number, title));
            }
            Err(e) => {
                result = Err(e);
            }
        }
    }

    // Second pass: update stack navigation in PR bodies when >= 2 PRs were
    // processed successfully.
    if result.is_ok() && stack_prs.len() >= 2 {
        let stack_refs: Vec<(u64, &str)> =
            stack_prs.iter().map(|(n, t)| (*n, t.as_str())).collect();
        for (pr_number, _) in &stack_prs {
            let update_result: Result<()> = async {
                let current_pr = gh.clone().get_pull_request(*pr_number).await?;
                let current_body = current_pr.body.as_deref().unwrap_or("");
                let stripped = crate::github::strip_stack_nav(current_body);
                let nav =
                    crate::github::build_stack_nav_block(&stack_refs, *pr_number, config);
                let new_body = if stripped.trim().is_empty() {
                    nav
                } else {
                    format!("{}\n\n{}", stripped.trim(), nav)
                };
                gh.update_pull_request(
                    *pr_number,
                    PullRequestUpdate {
                        body: Some(new_body),
                        ..Default::default()
                    },
                )
                .await?;
                output("🔗", &format!("Updated stack navigation for PR #{pr_number}"))?;
                Ok(())
            }
            .await;
            add_error(&mut result, update_result);
        }
    }

    // This updates the commit message in the local Jujutsu repository (if it was
    // changed by the implementation)
    add_error(
        &mut result,
        jj.rewrite_commit_messages(prepared_commits.as_mut_slice()),
    );

    result
}

#[allow(clippy::too_many_arguments)]
async fn diff_impl(
    opts: &DiffOptions,
    message_on_prompt: &mut String,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
    local_commit: &mut crate::jj::PreparedCommit,
    master_base_oid: Oid,
    pull_request: Option<PullRequest>,
    prev_pr_info: Option<(GitHubBranch, Oid)>,
) -> Result<(GitHubBranch, Oid, u64)> {
    // Parsed commit message of the local commit
    let message = &mut local_commit.message;

    // Check if the local commit is based directly on the master branch.
    let directly_based_on_master = local_commit.parent_oid == master_base_oid;

    // Determine the trees the Pull Request branch and the base branch should
    // have when we're done here.
    let (new_head_tree, new_base_tree) = if !opts.cherry_pick || directly_based_on_master {
        // Unless the user tells us to --cherry-pick, these should be the trees
        // of the current commit and its parent.
        // If the current commit is directly based on master (i.e.
        // directly_based_on_master is true), then we can do this here even when
        // the user tells us to --cherry-pick, because we would cherry pick the
        // current commit onto its parent, which gives us the same tree as the
        // current commit has, and the master base is the same as this commit's
        // parent.
        let head_tree = jj.get_tree_oid_for_commit(local_commit.oid)?;
        let base_tree = jj.get_tree_oid_for_commit(local_commit.parent_oid)?;

        (head_tree, base_tree)
    } else {
        // Cherry-pick the current commit onto master
        let index = jj.cherrypick(local_commit.oid, master_base_oid)?;

        if index.has_conflicts() {
            return Err(Error::new(formatdoc!(
                "This commit cannot be cherry-picked on {master}.",
                master = config.master_ref.branch_name(),
            )));
        }

        // This is the tree we are getting from cherrypicking the local commit
        // on master.
        let cherry_pick_tree = jj.write_index(index)?;
        let master_tree = jj.get_tree_oid_for_commit(master_base_oid)?;

        (cherry_pick_tree, master_tree)
    };

    if let Some(number) = local_commit.pull_request_number {
        output(
            "#️⃣ ",
            &format!(
                "Pull Request #{}: {}",
                number,
                config.pull_request_url(number)
            ),
        )?;
    }

    if local_commit.pull_request_number.is_none() || opts.update_message {
        validate_commit_message(message)?;
    }

    if let Some(ref pull_request) = pull_request {
        if pull_request.state == PullRequestState::Closed {
            return Err(Error::new(formatdoc!(
                "Pull request is closed. If you want to open a new one, \
                 remove the 'Pull Request' section from the commit message."
            )));
        }

        if !opts.update_message {
            let mut pull_request_updates: PullRequestUpdate = Default::default();
            pull_request_updates.update_message(pull_request, message);

            if !pull_request_updates.is_empty() {
                output(
                    "⚠️",
                    indoc!(
                        "The Pull Request's title/message differ from the \
                         local commit's message.
                         Use `spr diff --update-message` to overwrite the \
                         title and message on GitHub with the local message, \
                         or `spr amend` to go the other way (rewrite the local \
                         commit message with what is on GitHub)."
                    ),
                )?;
                if let Some(ref local_title) = pull_request_updates.title {
                    output("  ", &format!("title  local : {:?}", local_title))?;
                    output("  ", &format!("title  github: {:?}", &pull_request.title))?;
                }
                if let Some(ref local_body) = pull_request_updates.body {
                    let gh_body = crate::github::strip_stack_nav(
                        pull_request.body.as_deref().unwrap_or(""),
                    );
                    output("  ", &format!("body   local : {:?}", local_body.trim()))?;
                    output("  ", &format!("body   github: {:?}", gh_body.trim()))?;
                }
            }
        }
    }

    // Parse "Reviewers" section, if this is a new Pull Request
    let mut requested_reviewers = PullRequestRequestReviewers::default();

    if local_commit.pull_request_number.is_none()
        && let Some(reviewers) = message.get(&MessageSection::Reviewers)
    {
        let reviewers = parse_name_list(reviewers);
        let mut checked_reviewers = Vec::new();

        for reviewer in reviewers {
            // Teams are indicated with a leading #
            if let Some(slug) = reviewer.strip_prefix('#') {
                if let Ok(team) = GitHub::get_github_team((&config.owner).into(), slug.into()).await
                {
                    requested_reviewers
                        .team_reviewers
                        .push(team.slug.to_string());

                    checked_reviewers.push(reviewer);
                } else {
                    return Err(Error::new(format!(
                        "Reviewers field contains unknown team '{}'",
                        reviewer
                    )));
                }
            } else if let Ok(user) = GitHub::get_github_user(reviewer.clone()).await {
                requested_reviewers.reviewers.push(user.login);
                if let Some(name) = user.name {
                    checked_reviewers.push(format!(
                        "{} ({})",
                        reviewer.clone(),
                        remove_all_parens(&name)
                    ));
                } else {
                    checked_reviewers.push(reviewer);
                }
            } else {
                return Err(Error::new(format!(
                    "Reviewers field contains unknown user '{}'",
                    reviewer
                )));
            }
        }

        message.insert(MessageSection::Reviewers, checked_reviewers.join(", "));
        local_commit.message_changed = true;
    }

    // For new PRs, open $EDITOR so the user can write the PR description.
    // Pre-fill with a repository PR template when one is found, otherwise use
    // whatever is already in the Summary section of the commit message.
    if pull_request.is_none() && !opts.no_edit {
        let templates = find_pr_templates(jj.repo_path());

        // When multiple templates exist, prompt the user to pick one.
        let template_content: Option<String> = if templates.len() > 1 {
            let names: Vec<String> = templates.iter().map(|(n, _)| n.clone()).collect();
            let idx = tokio::task::spawn_blocking(move || {
                dialoguer::Select::new()
                    .with_prompt("Select a PR template")
                    .items(&names)
                    .default(0)
                    .interact()
            })
            .await??;
            templates.into_iter().nth(idx).map(|(_, c)| c)
        } else {
            templates.into_iter().next().map(|(_, c)| c)
        };

        // Build a context comment shown at the top of the editor so the user
        // knows which commit's PR description they are writing.  It is stripped
        // before saving so it never appears in the GitHub PR body.
        let context_comment = {
            let pr_title = message
                .get(&MessageSection::Title)
                .map(String::as_str)
                .unwrap_or("(untitled)");
            let short_id = &local_commit.short_id;

            // Collect changed files via a git tree diff.
            let files_text = (|| -> Option<String> {
                let ct_oid = jj.get_tree_oid_for_commit(local_commit.oid).ok()?;
                let pt_oid = jj.get_tree_oid_for_commit(local_commit.parent_oid).ok()?;
                let commit_tree = jj.git_repo.find_tree(ct_oid).ok()?;
                let parent_tree = jj.git_repo.find_tree(pt_oid).ok()?;
                let diff = jj
                    .git_repo
                    .diff_tree_to_tree(Some(&parent_tree), Some(&commit_tree), None)
                    .ok()?;
                let mut lines: Vec<String> = Vec::new();
                let _ = diff.foreach(
                    &mut |delta, _| {
                        let status = match delta.status() {
                            git2::Delta::Added => "A",
                            git2::Delta::Deleted => "D",
                            git2::Delta::Renamed => "R",
                            git2::Delta::Copied => "C",
                            _ => "M",
                        };
                        let path = delta
                            .new_file()
                            .path()
                            .or_else(|| delta.old_file().path())
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        lines.push(format!("  {status}  {path}"));
                        true
                    },
                    None,
                    None,
                    None,
                );
                if lines.is_empty() {
                    Some("  (no files changed)".to_string())
                } else {
                    Some(lines.join("\n"))
                }
            })()
            .unwrap_or_else(|| "  (unavailable)".to_string());

            format!(
                "<!-- spr: writing PR body for: {pr_title} ({short_id})\n\
                 \n\
                 Changed files:\n\
                 {files_text}\n\
                 \n\
                 Everything below this line is saved as the PR description.\n\
                 This comment will not appear in the PR on GitHub.\n\
                 -->\n"
            )
        };

        let body_content = template_content
            .or_else(|| message.get(&MessageSection::Summary).cloned())
            .unwrap_or_default();
        let initial = format!("{context_comment}\n{body_content}");

        let edited = tokio::task::spawn_blocking(move || {
            dialoguer::Editor::new().extension(".md").edit(&initial)
        })
        .await??;

        if let Some(body) = edited {
            // Strip the context comment we prepended before saving.
            let body = strip_context_comment(body.trim()).trim().to_string();
            if !body.is_empty() {
                message.insert(MessageSection::Summary, body);
                local_commit.message_changed = true;
            }
        }
    }

    // Get the name of the existing Pull Request branch, or constuct one if
    // there is none yet.

    let title = message
        .get(&MessageSection::Title)
        .map(|t| &t[..])
        .unwrap_or("");

    let pull_request_branch = match &pull_request {
        Some(pr) => pr.head.clone(),
        None => {
            config.new_github_branch(&config.get_new_branch_name(&jj.get_all_ref_names()?, title))
        }
    };

    // Get the tree ids of the current head of the Pull Request, as well as the
    // base, and the commit id of the master commit this PR is currently based
    // on.
    // If there is no pre-existing Pull Request, we fill in the equivalent
    // values.
    let (pr_head_oid, pr_head_tree, pr_base_oid, pr_base_tree, pr_master_base) =
        if let Some(pr) = &pull_request {
            let pr_head_tree = jj.get_tree_oid_for_commit(pr.head_oid)?;

            let current_master_oid = jj.resolve_reference(config.master_ref.local())?;
            // Use git for merge base calculation since jj doesn't expose this directly
            let pr_base_oid = jj.git_repo.merge_base(pr.head_oid, pr.base_oid)?;
            let pr_base_tree = jj.get_tree_oid_for_commit(pr_base_oid)?;

            let pr_master_base = jj.git_repo.merge_base(pr.head_oid, current_master_oid)?;

            (
                pr.head_oid,
                pr_head_tree,
                pr_base_oid,
                pr_base_tree,
                pr_master_base,
            )
        } else if let Some((_, prev_oid)) = prev_pr_info.as_ref() {
            // New PR in a stack: initialise from the previous PR's pushed commit so
            // Case 1 applies (trees already match) and the correct parent is used.
            let prev_tree = jj.get_tree_oid_for_commit(*prev_oid)?;
            (*prev_oid, prev_tree, *prev_oid, prev_tree, master_base_oid)
        } else {
            let master_base_tree = jj.get_tree_oid_for_commit(master_base_oid)?;
            (
                master_base_oid,
                master_base_tree,
                master_base_oid,
                master_base_tree,
                master_base_oid,
            )
        };
    let needs_merging_master = pr_master_base != master_base_oid;

    // At this point we can check if we can exit early because no update to the
    // existing Pull Request is necessary
    if let Some(ref pull_request) = pull_request {
        // So there is an existing Pull Request...
        if !needs_merging_master && pr_head_tree == new_head_tree && pr_base_tree == new_base_tree {
            // ...and it does not need a rebase, and the trees of both Pull
            // Request branch and base are all the right ones.
            output("✅", "No update necessary")?;

            let mut pull_request_updates: PullRequestUpdate = Default::default();

            if opts.update_message {
                // However, the user requested to update the commit message on
                // GitHub
                pull_request_updates.update_message(pull_request, message);
            }

            // Even when content is unchanged, the GitHub base may need updating
            // to point at the previous PR's branch (stacked PRs).
            if let Some((ref prev_branch, _)) = prev_pr_info
                && pull_request.base.branch_name() != prev_branch.branch_name()
            {
                pull_request_updates.base = Some(prev_branch.branch_name().to_string());
            }

            if !pull_request_updates.is_empty() {
                gh.update_pull_request(pull_request.number, pull_request_updates)
                    .await?;
                if opts.update_message {
                    output("✍", "Updated commit message on GitHub")?;
                }
            }

            return Ok((pull_request_branch, pr_head_oid, pull_request.number));
        }
    }

    // Check if there is a base branch on GitHub already. That's the case when
    // there is an existing Pull Request, and its base is not the master branch.
    let base_branch = if let Some(ref pr) = pull_request {
        if pr.base.is_master_branch() {
            None
        } else {
            Some(pr.base.clone())
        }
    } else {
        None
    };

    // We are going to construct `pr_base_parent: Option<Oid>`.
    // The value will be the commit we have to merge into the new Pull Request
    // commit to reflect changes in the parent of the local commit (by rebasing
    // or changing commits between master and this one, although technically
    // that's also rebasing).
    // If it's `None`, then we will not merge anything into the new Pull Request
    // commit.
    // If we are updating an existing PR, then there are three cases here:
    // (1) the parent tree of this commit is unchanged and we do not need to
    //     merge in master, which means that the local commit was amended, but
    //     not rebased. We don't need to merge anything into the Pull Request
    //     branch.
    // (2) the parent tree has changed, but the parent of the local commit is on
    //     master (or we are cherry-picking) and we are not already using a base
    //     branch: in this case we can merge the master commit we are based on
    //     into the PR branch, without going via a base branch. Thus, we don't
    //     introduce a base branch here and the PR continues to target the
    //     master branch.
    // (3) the parent tree has changed, and we need to use a base branch (either
    //     because one was already created earlier, or we find that we are not
    //     directly based on master now): we need to construct a new commit for
    //     the base branch. That new commit's tree is always that of that local
    //     commit's parent (thus making sure that the difference between base
    //     branch and pull request branch are exactly the changes made by the
    //     local commit, thus the changes we want to have reviewed). The new
    //     commit may have one or two parents. The previous base is always a
    //     parent (that's either the current commit on an existing base branch,
    //     or the previous master commit the PR was based on if there isn't a
    //     base branch already). In addition, if the master commit this commit
    //     is based on has changed, (i.e. the local commit got rebased on newer
    //     master in the meantime) then we have to merge in that master commit,
    //     which will be the second parent.
    // If we are creating a new pull request then `pr_base_tree` (the current
    // base of the PR) was set above to be the tree of the master commit the
    // local commit is based one, whereas `new_base_tree` is the tree of the
    // parent of the local commit. So if the local commit for this new PR is on
    // master, those two are the same (and we want to apply case 1). If the
    // commit is not directly based on master, we have to create this new PR
    // with a base branch, so that is case 3.

    let (pr_base_parent, base_branch) = if pr_base_tree == new_base_tree && !needs_merging_master {
        // Case 1
        (None, base_branch)
    } else if base_branch.is_none() && (directly_based_on_master || opts.cherry_pick) {
        // Case 2
        (Some(master_base_oid), None)
    } else {
        // Case 3

        // We are constructing a base branch commit.
        // One parent of the new base branch commit will be the current base
        // commit, that could be either the top commit of an existing base
        // branch, or a commit on master.
        let mut parents = vec![pr_base_oid];

        // If we need to rebase on master, make the master commit also a
        // parent (except if the first parent is that same commit, we don't
        // want duplicates in `parents`).
        if needs_merging_master && pr_base_oid != master_base_oid {
            parents.push(master_base_oid);
        }

        let new_base_branch_commit = jj.create_derived_commit(
            local_commit.parent_oid,
            &format!(
                "[spr] {}\n\nCreated using jj-spr {}\n\n[skip ci]",
                if pull_request.is_some() {
                    "changes introduced through rebase".to_string()
                } else {
                    format!(
                        "changes to {} this commit is based on",
                        config.master_ref.branch_name()
                    )
                },
                env!("CARGO_PKG_VERSION"),
            ),
            new_base_tree,
            &parents[..],
        )?;

        // If `base_branch` is `None` (which means a base branch does not exist
        // yet), then make a `GitHubBranch` with a new name for a base branch
        let base_branch = if let Some(base_branch) = base_branch {
            base_branch
        } else {
            config.new_github_branch(&config.get_base_branch_name(&jj.get_all_ref_names()?, title))
        };

        (Some(new_base_branch_commit), Some(base_branch))
    };

    // In a stacked PR workflow, override the base branch to be the previous
    // PR's branch. We don't push anything to that branch (it was already
    // updated by the previous diff_impl call), so pr_base_parent is cleared.
    let (base_branch, pr_base_parent) = if let Some((prev_branch, _)) = prev_pr_info.as_ref() {
        (Some(prev_branch.clone()), None)
    } else {
        (base_branch, pr_base_parent)
    };

    let mut github_commit_message = opts.message.clone();

    // In rebase mode there is no merge commit, so skip the bookkeeping-message
    // logic entirely. github_commit_message stays None (uses the PR title).
    let is_rebase_stack =
        config.stack_strategy == StackStrategy::Rebase && prev_pr_info.is_some();

    if pull_request.is_some() && github_commit_message.is_none() && !is_rebase_stack {
        // If the resulting commit will have multiple parents (a merge commit),
        // generate the message automatically rather than prompting. The message
        // follows Git's conventional "Merge branch X into Y" format so it is
        // clear what the two parents represent.
        //
        // After the stacking override above, pr_base_parent is None for stacked
        // PRs; the second parent (prev_oid) will be added by the fix below.
        let will_be_merge_commit = pr_base_parent.map_or_else(|| false, |oid| oid != pr_head_oid)
            || prev_pr_info
                .as_ref()
                .is_some_and(|(_, prev_oid)| *prev_oid != pr_head_oid);

        // Only auto-generate when the PR's own delta hasn't changed — i.e. the
        // commit is purely a bookkeeping merge to update the parent chain.
        //
        // We compare deltas (not absolute trees) because when a parent PR is
        // amended, the absolute tree of every child PR changes even though the
        // child's own contribution is unchanged. `has_unchanged_delta` applies
        // the old delta on top of the new base and checks the result matches.
        let delta_unchanged =
            jj.has_unchanged_delta(pr_base_tree, new_base_tree, pr_head_tree, new_head_tree);

        if will_be_merge_commit && delta_unchanged {
            // Describe the "from" branch: previous PR's branch for stacks,
            // the existing base branch for Case 3, or master for Case 2.
            let from_branch = if let Some((prev_branch, _)) = prev_pr_info.as_ref() {
                prev_branch.branch_name().to_string()
            } else if let Some(ref bb) = base_branch {
                bb.branch_name().to_string()
            } else {
                config.master_ref.branch_name().to_string()
            };
            github_commit_message = Some(format!(
                "Merge branch '{}' into '{}'\n\n[skip ci]",
                from_branch,
                pull_request_branch.branch_name(),
            ));
        } else {
            let input = {
                let message_on_prompt = message_on_prompt.clone();

                tokio::task::spawn_blocking(move || {
                    dialoguer::Input::<String>::new()
                        .with_prompt("Message (leave empty to abort)")
                        .with_initial_text(message_on_prompt)
                        .allow_empty(true)
                        .interact_text()
                })
                .await??
            };

            if input.is_empty() {
                return Err(Error::new("Aborted as per user request".to_string()));
            }

            *message_on_prompt = input.clone();
            github_commit_message = Some(input);
        }
    }

    // Construct the new commit for the Pull Request branch.
    //
    // In rebase mode, when this PR is in a stack, the only parent is the
    // previous PR's head commit. This produces a linear (non-merge) commit.
    //
    // In merge mode (the default), the first parent is the current PR head
    // commit, followed by any bookkeeping parents needed for rebases.
    let pr_commit_parents: Vec<Oid> = if is_rebase_stack {
        let (_, prev_oid) = prev_pr_info.as_ref().unwrap();
        vec![*prev_oid]
    } else {
        // First parent is the current head commit of the Pull Request (set to
        // the master base commit earlier if the Pull Request does not yet exist)
        let mut parents = vec![pr_head_oid];

        // If we prepared a commit earlier that needs merging into the Pull
        // Request branch, then that commit is a parent of the new PR commit.
        if let Some(oid) = pr_base_parent {
            // ...unless if that's the same commit as the one we added first.
            if parents.first() != Some(&oid) {
                parents.push(oid);
            }
        }

        // For stacked PRs, ensure the previous PR's head is a parent so GitHub
        // can compute the correct merge base for the diff.
        if let Some((_, prev_oid)) = prev_pr_info.as_ref()
            && !parents.contains(prev_oid)
        {
            parents.push(*prev_oid);
        }

        parents
    };

    // Create the new commit
    let commit_title = message
        .get(&MessageSection::Title)
        .map(String::as_str)
        .unwrap_or("Initial change");
    let pr_commit = jj.create_derived_commit(
        local_commit.oid,
        &format!(
            "{}\n\nCreated using jj-spr {}",
            github_commit_message.as_deref().unwrap_or(commit_title),
            env!("CARGO_PKG_VERSION"),
        ),
        new_head_tree,
        &pr_commit_parents[..],
    )?;

    // In rebase-stack mode, the rewritten commit is not a fast-forward of the
    // existing PR branch tip, so we need --force-with-lease.
    let force_push = is_rebase_stack && pull_request.is_some();

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("push").arg("--atomic").arg("--no-verify");
    if force_push {
        cmd.arg("--force-with-lease");
    }
    cmd.arg("--")
        .arg(&config.remote_name)
        .arg(format!("{}:{}", pr_commit, pull_request_branch.on_github()));

    let final_pr_number: u64;

    if let Some(pull_request) = pull_request {
        // We are updating an existing Pull Request
        final_pr_number = pull_request.number;

        if needs_merging_master {
            output(
                "⚾",
                &format!(
                    "Commit was rebased - updating Pull Request #{}",
                    pull_request.number
                ),
            )?;
        } else {
            output(
                "🔁",
                &format!(
                    "Commit was changed - updating Pull Request #{}",
                    pull_request.number
                ),
            )?;
        }

        // Things we want to update in the Pull Request on GitHub
        let mut pull_request_updates: PullRequestUpdate = Default::default();

        if opts.update_message {
            pull_request_updates.update_message(&pull_request, message);
        }

        if let Some(base_branch) = base_branch {
            // We are using a base branch.

            if let Some(base_branch_commit) = pr_base_parent {
                // ...and we prepared a new commit for it, so we need to push an
                // update of the base branch.
                cmd.arg(format!(
                    "{}:{}",
                    base_branch_commit,
                    base_branch.on_github()
                ));
            }

            // Push the new commit onto the Pull Request branch (and also the
            // new base commit, if we added that to cmd above).
            run_command(&mut cmd)
                .await
                .reword("git push failed".to_string())?;

            // If the Pull Request's base is not set to the base branch yet,
            // change that now.
            if pull_request.base.branch_name() != base_branch.branch_name() {
                pull_request_updates.base = Some(base_branch.branch_name().to_string());
            }
        } else {
            // The Pull Request is against the master branch. In that case we
            // only need to push the update to the Pull Request branch.
            run_command(&mut cmd)
                .await
                .reword("git push failed".to_string())?;
        }

        if !pull_request_updates.is_empty() {
            gh.update_pull_request(pull_request.number, pull_request_updates)
                .await?;
        }
    } else {
        // We are creating a new Pull Request.

        // If there's a base branch, add it to the push
        if let (Some(base_branch), Some(base_branch_commit)) = (&base_branch, pr_base_parent) {
            cmd.arg(format!(
                "{}:{}",
                base_branch_commit,
                base_branch.on_github()
            ));
        }
        // Push the pull request branch and the base branch if present
        run_command(&mut cmd)
            .await
            .reword("git push failed".to_string())?;

        // Then call GitHub to create the Pull Request.
        let pull_request_number = gh
            .create_pull_request(
                message,
                base_branch
                    .as_ref()
                    .unwrap_or(&config.master_ref)
                    .branch_name()
                    .to_string(),
                pull_request_branch.branch_name().to_string(),
                opts.draft,
            )
            .await?;
        final_pr_number = pull_request_number;

        let pull_request_url = config.pull_request_url(pull_request_number);

        output(
            "✨",
            &format!(
                "Created new Pull Request #{}: {}",
                pull_request_number, &pull_request_url,
            ),
        )?;

        message.insert(MessageSection::PullRequest, pull_request_url);
        local_commit.message_changed = true;

        let result = gh
            .request_reviewers(pull_request_number, requested_reviewers)
            .await;
        match result {
            Ok(()) => (),
            Err(error) => {
                output("⚠️", "Requesting reviewers failed")?;
                for message in error.messages() {
                    output("  ", message)?;
                }
            }
        }
    }

    Ok((pull_request_branch, pr_commit, final_pr_number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config() -> crate::config::Config {
        crate::config::Config::new(
            "test_owner".into(),
            "test_repo".into(),
            "origin".into(),
            "main".into(),
            "spr/test/".into(),
            false,
            crate::config::StackStrategy::Merge,
        )
    }

    #[allow(dead_code)]
    fn create_test_git_repo() -> (TempDir, git2::Repository) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let repo = git2::Repository::init(temp_dir.path()).expect("Failed to init git repo");

        // Create initial commit
        let signature = git2::Signature::now("Test User", "test@example.com")
            .expect("Failed to create signature");
        let tree_id = {
            let mut index = repo.index().expect("Failed to get index");
            index.write_tree().expect("Failed to write tree")
        };
        let tree = repo.find_tree(tree_id).expect("Failed to find tree");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Initial commit",
            &tree,
            &[],
        )
        .expect("Failed to create initial commit");

        drop(tree); // Drop the tree reference before moving repo
        (temp_dir, repo)
    }

    #[allow(dead_code)]
    fn create_test_commit(repo: &git2::Repository, message: &str, content: &str) -> git2::Oid {
        let signature = git2::Signature::now("Test User", "test@example.com")
            .expect("Failed to create signature");

        // Write content to a test file
        let repo_path = repo.workdir().expect("Failed to get workdir");
        let file_path = repo_path.join("test.txt");
        fs::write(&file_path, content).expect("Failed to write test file");

        // Add file to index
        let mut index = repo.index().expect("Failed to get index");
        index
            .add_path(std::path::Path::new("test.txt"))
            .expect("Failed to add file to index");
        index.write().expect("Failed to write index");

        let tree_id = index.write_tree().expect("Failed to write tree");
        let tree = repo.find_tree(tree_id).expect("Failed to find tree");

        // Get HEAD commit as parent
        let parent_commit = repo
            .head()
            .expect("Failed to get HEAD")
            .peel_to_commit()
            .expect("Failed to peel to commit");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )
        .expect("Failed to create commit")
    }

    #[test]
    fn test_diff_options_default_values() {
        let opts = DiffOptions {
            all: false,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            base: None,
            revision: None,
            no_edit: true,
        };

        assert!(!opts.all);
        assert!(!opts.update_message);
        assert!(!opts.draft);
        assert!(!opts.cherry_pick);
        assert!(opts.message.is_none());
        assert!(opts.base.is_none());
    }

    #[test]
    fn test_diff_options_with_base() {
        let opts = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            base: Some("main".to_string()),
            revision: None,
            no_edit: true,
        };

        assert_eq!(opts.base, Some("main".to_string()));
        assert!(opts.all);
    }

    #[test]
    fn test_jujutsu_integration() {
        // Test configuration for jj-spr
        let config = create_test_config();
        assert_eq!(config.owner, "test_owner");
        assert_eq!(config.remote_name, "origin");
    }

    #[test]
    fn test_base_option_parsing() {
        // Test that the base option can be parsed correctly
        let opts_with_base = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            base: Some("main".to_string()),
            revision: None,
            no_edit: true,
        };

        assert_eq!(opts_with_base.base.as_deref(), Some("main"));
        assert!(opts_with_base.all);

        let opts_with_trunk = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            no_edit: true,
        };

        assert_eq!(opts_with_trunk.base.as_deref(), Some("trunk()"));
    }

    #[test]
    fn test_all_flag_behavior() {
        let opts_with_all = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            no_edit: true,
        };

        // When --all is specified, it should work with base revisions
        assert!(opts_with_all.all);
        assert!(opts_with_all.base.is_some());
    }

    #[test]
    fn test_diff_options_combinations() {
        // Test various valid combinations of options
        let opts = DiffOptions {
            all: true,
            update_message: true,
            draft: true,
            message: Some("Update message".to_string()),
            cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            no_edit: true,
        };

        assert!(opts.all);
        assert!(opts.update_message);
        assert!(opts.draft);
        assert_eq!(opts.message.as_deref(), Some("Update message"));
        assert!(!opts.cherry_pick);
        assert_eq!(opts.base.as_deref(), Some("trunk()"));
    }

    // Integration tests would require more complex setup with actual Git repositories
    // and proper mocking of GitHub API calls. The tests above focus on:
    // 1. Option parsing and validation
    // 2. Data structure correctness
    // 3. Basic logic flow verification
    //
    // For full integration testing, consider:
    // - Mocking GitHub API responses
    // - Creating test repositories with specific commit structures
    // - Testing the interaction between revision specification and commit preparation
}
