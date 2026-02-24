/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use graphql_client::{GraphQLQuery, Response};
use serde::Deserialize;

use crate::{
    error::{Error, Result, ResultExt},
    message::{MessageSection, MessageSectionsMap, build_github_body},
};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub struct GitHub {
    config: crate::config::Config,
    graphql_client: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub state: PullRequestState,
    pub title: String,
    pub body: Option<String>,
    pub sections: MessageSectionsMap,
    pub base: GitHubBranch,
    pub head: GitHubBranch,
    pub base_oid: git2::Oid,
    pub head_oid: git2::Oid,
    pub merge_commit: Option<git2::Oid>,
    pub reviewers: HashMap<String, ReviewStatus>,
    pub review_status: Option<ReviewStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewStatus {
    Requested,
    Approved,
    Rejected,
}

/// Normalise GitHub's \r\n line endings to \n and strip trailing whitespace
/// from each line, so comparisons don't false-positive on GitHub's formatting.
fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

const STACK_NAV_START: &str = "<!-- spr-stack-nav -->";
const STACK_NAV_END: &str = "<!-- /spr-stack-nav -->";

/// Strip the `<!-- spr-stack-nav --> ... <!-- /spr-stack-nav -->` block from a
/// PR body, returning the remaining content with surrounding whitespace trimmed.
pub fn strip_stack_nav(body: &str) -> String {
    if let (Some(start), Some(end_pos)) = (body.find(STACK_NAV_START), body.rfind(STACK_NAV_END)) {
        let end = end_pos + STACK_NAV_END.len();
        let before = body[..start].trim_end();
        let after = body[end..].trim_start();
        if before.is_empty() {
            after.to_string()
        } else if after.is_empty() {
            before.to_string()
        } else {
            format!("{before}\n\n{after}")
        }
    } else {
        body.to_string()
    }
}

/// Build the stack navigation block to append to a PR body.
///
/// `prs` is a slice of `(number, title)` for every PR in the stack (in order).
/// `current_pr` is highlighted with a bold arrow.
pub fn build_stack_nav_block(
    prs: &[(u64, &str)],
    current_pr: u64,
    config: &crate::config::Config,
) -> String {
    let items: Vec<String> = prs
        .iter()
        .map(|(number, title)| {
            let url = config.pull_request_url(*number);
            if *number == current_pr {
                format!("- **→ #{number} [{title}]({url})**")
            } else {
                format!("- #{number} [{title}]({url})")
            }
        })
        .collect();
    format!(
        "{}\n\n---\n\n**Stack**\n{}\n\n{}",
        STACK_NAV_START,
        items.join("\n"),
        STACK_NAV_END,
    )
}

#[derive(serde::Serialize, Default, Debug)]
pub struct PullRequestUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<PullRequestState>,
}

impl PullRequestUpdate {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.body.is_none() && self.base.is_none() && self.state.is_none()
    }

    pub fn update_message(&mut self, pull_request: &PullRequest, message: &MessageSectionsMap) {
        // Normalize both sides before comparing. GitHub returns \r\n line endings
        // and may have trailing whitespace; local content uses \n throughout.
        if let Some(local_title) = message.get(&MessageSection::Title)
            && normalize_line_endings(local_title).trim()
                != normalize_line_endings(&pull_request.title).trim()
        {
            self.title = Some(local_title.clone());
        }

        let body = build_github_body(message);
        // Strip any stack-nav block from both sides before comparing.
        // The local Summary may have nav embedded if `spr amend` was run
        // against a PR whose body already had nav appended; and GitHub's body
        // always has nav appended by the second-pass loop.  Stripping both
        // ensures neither side carries spurious nav during the comparison, and
        // the stored value is the clean, nav-free body so the second-pass loop
        // can append a fresh nav block afterwards.
        let body_clean = strip_stack_nav(body.trim());
        let pr_body_raw = pull_request.body.as_deref().unwrap_or("");
        let pr_body_clean = strip_stack_nav(pr_body_raw.trim());
        if normalize_line_endings(body_clean.trim()) != normalize_line_endings(pr_body_clean.trim()) {
            self.body = Some(body_clean);
        }
    }
}

#[derive(serde::Serialize, Default, Debug)]
pub struct PullRequestRequestReviewers {
    pub reviewers: Vec<String>,
    pub team_reviewers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestState {
    Open,
    Closed,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct UserWithName {
    pub login: String,
    pub name: Option<String>,
    #[serde(default)]
    pub is_collaborator: bool,
}

#[derive(Debug, Clone)]
pub struct PullRequestMergeability {
    pub base: GitHubBranch,
    pub head_oid: git2::Oid,
    pub mergeable: Option<bool>,
    pub merge_commit: Option<git2::Oid>,
}

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/pullrequest_query.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestQuery;
type GitObjectID = String;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/pullrequest_mergeability_query.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestMergeabilityQuery;

impl GitHub {
    pub fn new(config: crate::config::Config, graphql_client: reqwest::Client) -> Self {
        Self {
            config,
            graphql_client,
        }
    }

    pub async fn get_github_user(login: String) -> Result<UserWithName> {
        octocrab::instance()
            .get::<UserWithName, _, _>(format!("/users/{}", login), None::<&()>)
            .await
            .map_err(Error::from)
    }

    pub async fn get_github_team(
        owner: String,
        team: String,
    ) -> Result<octocrab::models::teams::Team> {
        octocrab::instance()
            .teams(owner)
            .get(team)
            .await
            .map_err(Error::from)
    }

    pub async fn get_pull_request(self, number: u64) -> Result<PullRequest> {
        let GitHub {
            config,
            graphql_client,
        } = self;

        let variables = pull_request_query::Variables {
            name: config.repo.clone(),
            owner: config.owner.clone(),
            number: number as i64,
        };
        let request_body = PullRequestQuery::build_query(variables);
        let res = graphql_client
            .post("https://api.github.com/graphql")
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<pull_request_query::ResponseData> = res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(format!("fetching PR #{number} failed")));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        let pr = response_body
            .data
            .ok_or_else(|| Error::new("failed to fetch PR"))?
            .repository
            .ok_or_else(|| Error::new("failed to find repository"))?
            .pull_request
            .ok_or_else(|| Error::new("failed to find PR"))?;

        let base = config.new_github_branch_from_ref(&pr.base_ref_name)?;
        let head = config.new_github_branch_from_ref(&pr.head_ref_name)?;

        // Fetch refs from remote using git (since we're in a colocated repo)
        let _fetch_result = tokio::process::Command::new("git")
            .args([
                "fetch",
                "--no-write-fetch-head",
                &config.remote_name,
                &format!("{}:{}", head.on_github(), head.local()),
                &format!("{}:{}", base.on_github(), base.local()),
            ])
            .output()
            .await;

        // Convert branch refs to OIDs
        let base_oid = if let Ok(output) = tokio::process::Command::new("git")
            .args(["rev-parse", base.local()])
            .output()
            .await
        {
            if output.status.success() {
                let oid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                git2::Oid::from_str(&oid_str).unwrap_or(git2::Oid::zero())
            } else {
                git2::Oid::zero()
            }
        } else {
            git2::Oid::zero()
        };

        let head_oid = if let Ok(output) = tokio::process::Command::new("git")
            .args(["rev-parse", head.local()])
            .output()
            .await
        {
            if output.status.success() {
                let oid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                git2::Oid::from_str(&oid_str).unwrap_or(git2::Oid::zero())
            } else {
                git2::Oid::zero()
            }
        } else {
            git2::Oid::zero()
        };

        let mut sections = MessageSectionsMap::new();
        let body = normalize_line_endings(&pr.body);
        let body = body.trim();
        if !body.is_empty() {
            sections.insert(MessageSection::Summary, body.to_string());
        }

        let title = pr.title.trim().to_string();
        sections.insert(
            MessageSection::Title,
            if title.is_empty() {
                String::from("(untitled)")
            } else {
                title
            },
        );

        sections.insert(MessageSection::PullRequest, config.pull_request_url(number));

        let reviewers: HashMap<String, ReviewStatus> = pr
            .latest_opinionated_reviews
            .iter()
            .flat_map(|all_reviews| &all_reviews.nodes)
            .flatten()
            .flatten()
            .flat_map(|review| {
                let user_name = review.author.as_ref()?.login.clone();
                let status = match review.state {
                    pull_request_query::PullRequestReviewState::APPROVED => ReviewStatus::Approved,
                    pull_request_query::PullRequestReviewState::CHANGES_REQUESTED => {
                        ReviewStatus::Rejected
                    }
                    _ => ReviewStatus::Requested,
                };
                Some((user_name, status))
            })
            .collect();

        let review_status = match pr.review_decision {
            Some(pull_request_query::PullRequestReviewDecision::APPROVED) => {
                Some(ReviewStatus::Approved)
            }
            Some(pull_request_query::PullRequestReviewDecision::CHANGES_REQUESTED) => {
                Some(ReviewStatus::Rejected)
            }
            Some(pull_request_query::PullRequestReviewDecision::REVIEW_REQUIRED) => {
                Some(ReviewStatus::Requested)
            }
            _ => None,
        };

        let requested_reviewers: Vec<String> = pr.review_requests
            .iter()
            .flat_map(|x| &x.nodes)
            .flatten()
            .flatten()
            .flat_map(|x| &x.requested_reviewer)
            .flat_map(|reviewer| {
              type UserType = pull_request_query::PullRequestQueryRepositoryPullRequestReviewRequestsNodesRequestedReviewer;
              match reviewer {
                UserType::User(user) => Some(user.login.clone()),
                UserType::Team(team) => Some(format!("#{}", team.slug)),
                _ => None,
              }
            })
            .chain(reviewers.keys().cloned())
            .collect::<HashSet<String>>() // de-duplicate
            .into_iter()
            .collect();

        sections.insert(
            MessageSection::Reviewers,
            requested_reviewers.iter().fold(String::new(), |out, slug| {
                if out.is_empty() {
                    slug.to_string()
                } else {
                    format!("{}, {}", out, slug)
                }
            }),
        );

        if review_status == Some(ReviewStatus::Approved) {
            sections.insert(
                MessageSection::ReviewedBy,
                reviewers
                    .iter()
                    .filter_map(|(k, v)| {
                        if v == &ReviewStatus::Approved {
                            Some(k)
                        } else {
                            None
                        }
                    })
                    .fold(String::new(), |out, slug| {
                        if out.is_empty() {
                            slug.to_string()
                        } else {
                            format!("{}, {}", out, slug)
                        }
                    }),
            );
        }

        Ok::<_, Error>(PullRequest {
            number: pr.number as u64,
            state: match pr.state {
                pull_request_query::PullRequestState::OPEN => PullRequestState::Open,
                _ => PullRequestState::Closed,
            },
            title: pr.title,
            body: Some(pr.body),
            sections,
            base,
            head,
            base_oid,
            head_oid,
            reviewers,
            review_status,
            merge_commit: pr
                .merge_commit
                .and_then(|sha| git2::Oid::from_str(&sha.oid).ok()),
        })
    }

    pub async fn create_pull_request(
        &self,
        message: &MessageSectionsMap,
        base_ref_name: String,
        head_ref_name: String,
        draft: bool,
    ) -> Result<u64> {
        let number = octocrab::instance()
            .pulls(self.config.owner.clone(), self.config.repo.clone())
            .create(
                message
                    .get(&MessageSection::Title)
                    .unwrap_or(&String::new()),
                head_ref_name,
                base_ref_name,
            )
            .body(build_github_body(message))
            .draft(Some(draft))
            .send()
            .await?
            .number;

        Ok(number)
    }

    /// Returns the numbers of all open PRs whose base branch is `base_branch`.
    pub async fn get_open_prs_with_base(&self, base_branch: &str) -> Result<Vec<u64>> {
        #[derive(Deserialize)]
        struct MinimalPr {
            number: u64,
        }
        let prs: Vec<MinimalPr> = octocrab::instance()
            .get(
                format!("/repos/{}/{}/pulls", self.config.owner, self.config.repo),
                Some(&[
                    ("state", "open"),
                    ("base", base_branch),
                    ("per_page", "100"),
                ]),
            )
            .await?;
        Ok(prs.into_iter().map(|p| p.number).collect())
    }

    pub async fn update_pull_request(&self, number: u64, updates: PullRequestUpdate) -> Result<()> {
        octocrab::instance()
            .patch::<octocrab::models::pulls::PullRequest, _, _>(
                format!(
                    "/repos/{}/{}/pulls/{}",
                    self.config.owner, self.config.repo, number
                ),
                Some(&updates),
            )
            .await?;

        Ok(())
    }

    pub async fn request_reviewers(
        &self,
        number: u64,
        reviewers: PullRequestRequestReviewers,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Ignore {}
        let _: Ignore = octocrab::instance()
            .post(
                format!(
                    "/repos/{}/{}/pulls/{}/requested_reviewers",
                    self.config.owner, self.config.repo, number
                ),
                Some(&reviewers),
            )
            .await?;

        Ok(())
    }

    pub async fn get_pull_request_mergeability(
        &self,
        number: u64,
    ) -> Result<PullRequestMergeability> {
        let variables = pull_request_mergeability_query::Variables {
            name: self.config.repo.clone(),
            owner: self.config.owner.clone(),
            number: number as i64,
        };
        let request_body = PullRequestMergeabilityQuery::build_query(variables);
        let res = self
            .graphql_client
            .post("https://api.github.com/graphql")
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<pull_request_mergeability_query::ResponseData> =
            res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(format!(
                "querying PR #{number} mergeability failed"
            )));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        let pr = response_body
            .data
            .ok_or_else(|| Error::new("failed to fetch PR"))?
            .repository
            .ok_or_else(|| Error::new("failed to find repository"))?
            .pull_request
            .ok_or_else(|| Error::new("failed to find PR"))?;

        Ok::<_, Error>(PullRequestMergeability {
            base: self.config.new_github_branch_from_ref(&pr.base_ref_name)?,
            head_oid: git2::Oid::from_str(&pr.head_ref_oid)?,
            mergeable: match pr.mergeable {
                pull_request_mergeability_query::MergeableState::CONFLICTING => Some(false),
                pull_request_mergeability_query::MergeableState::MERGEABLE => Some(true),
                pull_request_mergeability_query::MergeableState::UNKNOWN => None,
                _ => None,
            },
            merge_commit: pr
                .merge_commit
                .and_then(|sha| git2::Oid::from_str(&sha.oid).ok()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GitHubBranch {
    ref_on_github: String,
    ref_local: String,
    is_master_branch: bool,
}

impl GitHubBranch {
    pub fn new_from_ref(ghref: &str, remote_name: &str, master_branch_name: &str) -> Result<Self> {
        let ref_on_github = if ghref.starts_with("refs/heads/") {
            ghref.to_string()
        } else if ghref.starts_with("refs/") {
            return Err(Error::new(format!(
                "Ref '{ghref}' does not refer to a branch"
            )));
        } else {
            format!("refs/heads/{ghref}")
        };

        // The branch name is `ref_on_github` with the `refs/heads/` prefix
        // (length 11) removed
        let branch_name = &ref_on_github[11..];
        let ref_local = format!("refs/remotes/{remote_name}/{branch_name}");
        let is_master_branch = branch_name == master_branch_name;

        Ok(Self {
            ref_on_github,
            ref_local,
            is_master_branch,
        })
    }

    pub fn new_from_branch_name(
        branch_name: &str,
        remote_name: &str,
        master_branch_name: &str,
    ) -> Self {
        Self {
            ref_on_github: format!("refs/heads/{branch_name}"),
            ref_local: format!("refs/remotes/{remote_name}/{branch_name}"),
            is_master_branch: branch_name == master_branch_name,
        }
    }

    pub fn on_github(&self) -> &str {
        &self.ref_on_github
    }

    pub fn local(&self) -> &str {
        &self.ref_local
    }

    pub fn is_master_branch(&self) -> bool {
        self.is_master_branch
    }

    pub fn branch_name(&self) -> &str {
        // The branch name is `ref_on_github` with the `refs/heads/` prefix
        // (length 11) removed
        &self.ref_on_github[11..]
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    #[test]
    fn test_new_from_ref_with_branch_name() {
        let r = GitHubBranch::new_from_ref("foo", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_master_branch_name() {
        let r =
            GitHubBranch::new_from_ref("masterbranch", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_ref_name() {
        let r =
            GitHubBranch::new_from_ref("refs/heads/foo", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_master_ref_name() {
        let r =
            GitHubBranch::new_from_ref("refs/heads/masterbranch", "github-remote", "masterbranch")
                .unwrap();
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_branch_name() {
        let r = GitHubBranch::new_from_branch_name("foo", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_master_branch_name() {
        let r = GitHubBranch::new_from_branch_name("masterbranch", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_edge_case_ref_name() {
        let r = GitHubBranch::new_from_ref(
            "refs/heads/refs/heads/foo",
            "github-remote",
            "masterbranch",
        )
        .unwrap();
        assert_eq!(r.on_github(), "refs/heads/refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/refs/heads/foo");
        assert_eq!(r.branch_name(), "refs/heads/foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_edge_case_branch_name() {
        let r =
            GitHubBranch::new_from_branch_name("refs/heads/foo", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/refs/heads/foo");
        assert_eq!(r.branch_name(), "refs/heads/foo");
        assert!(!r.is_master_branch());
    }

    fn make_config() -> crate::config::Config {
        crate::config::Config::new(
            "owner".into(),
            "repo".into(),
            "origin".into(),
            "main".into(),
            "spr/".into(),
            false,
            crate::config::StackStrategy::Merge,
        )
    }

    #[test]
    fn test_strip_stack_nav_no_nav() {
        assert_eq!(strip_stack_nav("some body"), "some body");
        assert_eq!(strip_stack_nav(""), "");
    }

    #[test]
    fn test_strip_stack_nav_only_nav() {
        let body =
            format!("{}\n\n---\n\n**Stack**\n- item\n\n{}", STACK_NAV_START, STACK_NAV_END);
        assert_eq!(strip_stack_nav(&body), "");
    }

    #[test]
    fn test_strip_stack_nav_with_prefix() {
        let body = format!(
            "My description\n\n{}\n\n---\n\n**Stack**\n- item\n\n{}",
            STACK_NAV_START, STACK_NAV_END
        );
        assert_eq!(strip_stack_nav(&body), "My description");
    }

    #[test]
    fn test_strip_stack_nav_with_suffix() {
        let body = format!(
            "{}\n\n---\n\n**Stack**\n- item\n\n{}\n\nafter",
            STACK_NAV_START, STACK_NAV_END
        );
        assert_eq!(strip_stack_nav(&body), "after");
    }

    #[test]
    fn test_strip_stack_nav_idempotent() {
        let config = make_config();
        let prs = &[(1u64, "First PR"), (2u64, "Second PR")];
        let nav = build_stack_nav_block(prs, 1, &config);
        // Stripping twice returns the same result as stripping once
        assert_eq!(strip_stack_nav(&nav), strip_stack_nav(&strip_stack_nav(&nav)));
    }

    #[test]
    fn test_build_stack_nav_block_highlights_current() {
        let config = make_config();
        let prs = &[(1u64, "First"), (2u64, "Second"), (3u64, "Third")];
        let block = build_stack_nav_block(prs, 2, &config);
        assert!(block.contains(STACK_NAV_START));
        assert!(block.contains(STACK_NAV_END));
        assert!(block.contains("**→ #2"));
        assert!(!block.contains("**→ #1"));
        assert!(!block.contains("**→ #3"));
        assert!(block.contains("- #1 [First]"));
        assert!(block.contains("- #3 [Third]"));
    }
}
