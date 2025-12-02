/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub mod branch;
pub mod gh_trait;
pub mod gql;
pub mod pr;

use async_trait::async_trait;
use graphql_client::{GraphQLQuery, Response};
use indoc::formatdoc;
use octocrab::models::Author;
use reqwest::header;
use serde::Deserialize;

use crate::{
    config::Config,
    error::{Error, Result, ResultExt},
    github::{
        gh_trait::{GitHubClient, UserWithName},
        gql::{
            PullRequestMergeabilityQuery, PullRequestQuery, SearchQuery,
            pull_request_mergeability_query, pull_request_query, search_query,
        },
        pr::{
            PullRequest, PullRequestMergeability, PullRequestRequestReviewers, PullRequestState,
            PullRequestUpdate, ReviewStatus,
        },
    },
    message::{
        MessageSection, MessageSectionsMap, build_github_body, build_github_body_for_merging,
        parse_message,
    },
};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub struct GitHub {
    graphql_client: reqwest::Client,
    crab: octocrab::Octocrab,
}

impl GitHub {
    pub fn from_token(token: String) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::ACCEPT, "application/json".parse()?);
        headers.insert(
            header::USER_AGENT,
            format!("spr/{}", env!("CARGO_PKG_VERSION")).try_into()?,
        );
        headers.insert(header::AUTHORIZATION, format!("Bearer {}", token).parse()?);

        let graphql_client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;

        let crab = octocrab::OctocrabBuilder::default()
            .personal_token(token.clone())
            .build()?;

        Ok(Self {
            graphql_client,
            crab,
        })
    }
}

#[async_trait]
impl GitHubClient for GitHub {
    async fn get_current_user(&self) -> Result<Author> {
        self.crab.current().user().await.map_err(Error::from)
    }

    async fn get_github_user(&self, login: String) -> Result<UserWithName> {
        let profile = self
            .crab
            .users(login)
            .profile()
            .await
            .map_err(Error::from)?;

        Ok(UserWithName {
            login: profile.login,
            name: profile.name,
            is_collaborator: false,
        })
    }

    async fn get_github_team(
        &self,
        owner: String,
        team: String,
    ) -> Result<octocrab::models::teams::Team> {
        self.crab.teams(owner).get(team).await.map_err(Error::from)
    }

    async fn get_repository(
        &self,
        owner: String,
        repo: String,
    ) -> Result<octocrab::models::Repository> {
        self.crab
            .repos(owner, repo)
            .get()
            .await
            .map_err(Error::from)
    }

    async fn get_pull_request(&self, number: u64, config: &Config) -> Result<PullRequest> {
        let variables = pull_request_query::Variables {
            name: config.repo.clone(),
            owner: config.owner.clone(),
            number: number as i64,
        };
        let request_body = PullRequestQuery::build_query(variables);
        let res = self
            .graphql_client
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

        let mut sections = parse_message(&pr.body, MessageSection::Summary);

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

    async fn create_pull_request(
        &self,
        owner: String,
        repo: String,
        message: &MessageSectionsMap,
        base_ref_name: String,
        head_ref_name: String,
        draft: bool,
    ) -> Result<u64> {
        let number = self
            .crab
            .pulls(owner, repo)
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

    async fn update_pull_request(
        &self,
        owner: String,
        repo: String,
        number: u64,
        updates: PullRequestUpdate,
    ) -> Result<()> {
        self.crab
            .patch::<octocrab::models::pulls::PullRequest, _, _>(
                format!("repos/{}/{}/pulls/{}", owner, repo, number),
                Some(&updates),
            )
            .await?;

        Ok(())
    }

    async fn request_reviewers(
        &self,
        owner: String,
        repo: String,
        number: u64,
        reviewers: PullRequestRequestReviewers,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Ignore {}
        let _: Ignore = self
            .crab
            .post(
                format!(
                    "repos/{}/{}/pulls/{}/requested_reviewers",
                    owner, repo, number
                ),
                Some(&reviewers),
            )
            .await?;

        Ok(())
    }

    async fn get_pull_request_mergeability(
        &self,
        number: u64,
        config: &Config,
    ) -> Result<PullRequestMergeability> {
        let variables = pull_request_mergeability_query::Variables {
            name: config.repo.clone(),
            owner: config.owner.clone(),
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
            base: config.new_github_branch_from_ref(&pr.base_ref_name)?,
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

    async fn list_open_reviews(
        &self,
        owner: String,
        repo: String,
    ) -> Result<Vec<search_query::SearchQuerySearchNodes>> {
        let variables = search_query::Variables {
            query: format!(
                "repo:{}/{} is:open is:pr author:@me archived:false",
                owner, repo
            ),
        };
        let request_body = SearchQuery::build_query(variables);
        let res = self
            .graphql_client
            .post("https://api.github.com/graphql")
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<search_query::ResponseData> = res.json().await?;

        let result = response_body
            .data
            .ok_or_else(|| Error::new("Failed to fech search query"))?
            .search
            .nodes
            .ok_or_else(|| Error::new("Failed to find search nodes"))?
            .into_iter()
            .flatten()
            .collect();

        Ok(result)
    }

    async fn merge_pull_request(
        &self,
        owner: String,
        repo: String,
        pull_request: &PullRequest,
    ) -> Result<octocrab::models::pulls::Merge> {
        self.crab
            .pulls(owner, repo)
            .merge(pull_request.number)
            .method(octocrab::params::pulls::MergeMethod::Squash)
            .title(pull_request.title.as_str())
            .message(build_github_body_for_merging(&pull_request.sections))
            .sha(format!("{}", pull_request.head_oid))
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
}

#[cfg(test)]
mod tests {
    use crate::github::branch::GitHubBranch;

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
}
