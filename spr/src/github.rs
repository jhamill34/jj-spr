/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use graphql_client::{GraphQLQuery, Response};
use reqwest::header;
use serde::Deserialize;

use crate::{
    error::{Error, Result, ResultExt},
    github::create_pull_request::CreatePullRequestInput,
    message::{MessageSection, MessageSectionsMap, build_github_body, parse_message},
};
use std::collections::{HashMap, HashSet};

pub const GRAPHQL_ENDPOINT: &str = "/graphql";

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
        let title = message.get(&MessageSection::Title);
        if title.is_some() && title != Some(&pull_request.title) {
            self.title = title.cloned();
        }

        let body = build_github_body(message);
        if pull_request.body.as_ref() != Some(&body) {
            self.body = Some(body);
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
    query_path = "src/gql/viewer_query.graphql",
    response_derives = "Debug"
)]
pub struct ViewerQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/team_query.graphql",
    response_derives = "Debug"
)]
pub struct TeamQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/user_query.graphql",
    response_derives = "Debug"
)]
pub struct UserQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/repository_query.graphql",
    response_derives = "Debug"
)]
pub struct RepositoryQuery;

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

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/create_pull_request.graphql",
    response_drives = "Debug"
)]
pub struct CreatePullRequest;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/request_reviewers.graphql",
    response_drives = "Debug"
)]
pub struct RequestReviews;

pub fn create_graphql_client(token: &str) -> Result<reqwest::Client> {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::ACCEPT,
        "application/json"
            .parse()
            .map_err(|_| Error::new("failed to parse content type"))?,
    );
    headers.insert(
        header::USER_AGENT,
        format!("spr/{}", env!("CARGO_PKG_VERSION"))
            .try_into()
            .map_err(|_| Error::new("failed to parse user agent"))?,
    );
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", token)
            .parse()
            .map_err(|_| Error::new("Failed to parse auth header"))?,
    );

    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|_| Error::new("Failed build reqwest client"))
}

impl GitHub {
    pub fn new(config: crate::config::Config, graphql_client: reqwest::Client) -> Self {
        Self {
            config,
            graphql_client,
        }
    }

    pub async fn post_graphql<Q: GraphQLQuery>(
        &self,
        variables: Q::Variables,
    ) -> Result<Response<Q::ResponseData>> {
        return graphql_client::reqwest::post_graphql::<Q, _>(
            &self.graphql_client,
            GRAPHQL_ENDPOINT,
            variables,
        )
        .await
        .map_err(|e| Error::new(format!("Failed to fetch graphql data: {}", e)));
    }

    pub async fn get_github_user(&self, login: String) -> Result<UserWithName> {
        let variables = user_query::Variables {
            login: login.clone(),
        };

        let response_body = self.post_graphql::<UserQuery>(variables).await?;
        let user = response_body
            .data
            .ok_or_else(|| Error::new("Failed to fetch user"))?
            .user
            .ok_or_else(|| Error::new(format!("User '{}' not found", login)))?;

        Ok(UserWithName {
            login: user.login,
            name: user.name,
            is_collaborator: false, // Not available in GraphQL user query
        })
    }

    pub async fn get_github_team(
        &self,
        owner: String,
        team: String,
    ) -> Result<team_query::TeamQueryOrganizationTeam> {
        let variables = team_query::Variables {
            org: owner.clone(),
            slug: team.clone(),
        };

        let response_body = self.post_graphql::<TeamQuery>(variables).await?;
        let team = response_body
            .data
            .and_then(|d| d.organization)
            .and_then(|org| org.team)
            .ok_or_else(|| Error::new(format!("Team '{}/{}' not found", owner, team)))?;
        Ok(team)
    }

    pub async fn get_pull_request(self, number: u64) -> Result<PullRequest> {
        let variables = pull_request_query::Variables {
            name: self.config.repo.clone(),
            owner: self.config.owner.clone(),
            number: number as i64,
        };
        let response_body = self.post_graphql::<PullRequestQuery>(variables).await?;
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

        let base = self.config.new_github_branch_from_ref(&pr.base_ref_name)?;
        let head = self.config.new_github_branch_from_ref(&pr.head_ref_name)?;

        // Fetch refs from remote using git (since we're in a colocated repo)
        let _fetch_result = tokio::process::Command::new("git")
            .args([
                "fetch",
                "--no-write-fetch-head",
                &self.config.remote_name,
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

        sections.insert(
            MessageSection::PullRequest,
            self.config.pull_request_url(number),
        );

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
        let repo = self.get_repo_info().await?;
        let repository_id = repo
            .repository
            .ok_or_else(|| Error::new("Unable to find repository"))?
            .id;

        let variables = create_pull_request::Variables {
            input: CreatePullRequestInput {
                title: message
                    .get(&MessageSection::Title)
                    .unwrap_or(&String::new())
                    .into(),
                repository_id,
                head_ref_name,
                base_ref_name,
                maintainer_can_modify: Some(true),
                body: Some(build_github_body(message)),
                draft: Some(draft),
                client_mutation_id: None,
                head_repository_id: None,
            },
        };

        let pr = self.post_graphql::<CreatePullRequest>(variables).await?;

        let number = pr
            .data
            .ok_or_else(|| Error::new("Unable to find pr data"))?
            .create_pull_request
            .ok_or_else(|| Error::new("Unable to find pr data (create_pull_request)"))?
            .pull_request
            .ok_or_else(|| Error::new("Unable to find pr data (pull_request)"))?
            .number as u64;

        Ok(number)
    }

    pub async fn update_pull_request(&self, number: u64, updates: PullRequestUpdate) -> Result<()> {
        octocrab::instance()
            .patch::<octocrab::models::pulls::PullRequest, _, _>(
                format!(
                    "repos/{}/{}/pulls/{}",
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
                    "repos/{}/{}/pulls/{}/requested_reviewers",
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

        let response_body = self
            .post_graphql::<PullRequestMergeabilityQuery>(variables)
            .await?;
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

    pub async fn get_repo_info(&self) -> Result<repository_query::ResponseData> {
        let variables = repository_query::Variables {
            owner: self.config.owner.clone(),
            name: self.config.repo.clone(),
        };

        let response_body = self.post_graphql::<RepositoryQuery>(variables).await?;

        response_body
            .data
            .ok_or_else(|| Error::new("failed to get repository info"))
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
}
