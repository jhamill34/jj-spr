use crate::{
    config::Config,
    error::Result,
    github::{
        gql::search_query,
        pr::{
            PullRequest, PullRequestMergeability, PullRequestRequestReviewers, PullRequestUpdate,
        },
    },
    message::MessageSectionsMap,
};

use async_trait::async_trait;
use octocrab::models::pulls::Merge;

#[derive(serde::Deserialize, Debug, Clone)]
pub struct UserWithName {
    pub login: String,
    pub name: Option<String>,
    #[serde(default)]
    pub is_collaborator: bool,
}

#[async_trait]
pub trait GitHubClient {
    async fn get_github_user(&self, login: String) -> Result<UserWithName>;

    async fn get_github_team(
        &self,
        owner: String,
        team: String,
    ) -> Result<octocrab::models::teams::Team>;

    async fn get_pull_request(&self, number: u64, config: &Config) -> Result<PullRequest>;

    async fn create_pull_request(
        &self,
        owner: String,
        repo: String,
        message: &MessageSectionsMap,
        base_ref_name: String,
        head_ref_name: String,
        draft: bool,
    ) -> Result<u64>;

    async fn update_pull_request(
        &self,
        owner: String,
        repo: String,
        number: u64,
        updates: PullRequestUpdate,
    ) -> Result<()>;

    async fn request_reviewers(
        &self,
        owner: String,
        repo: String,
        number: u64,
        reviewers: PullRequestRequestReviewers,
    ) -> Result<()>;

    async fn get_pull_request_mergeability(
        &self,
        number: u64,
        config: &Config,
    ) -> Result<PullRequestMergeability>;

    async fn list_open_reviews(
        &self,
        owner: String,
        repo: String,
    ) -> Result<Vec<search_query::SearchQuerySearchNodes>>;

    async fn merge_pull_request(
        &self,
        owner: String,
        repo: String,
        pull_request: &PullRequest,
    ) -> Result<Merge>;
}
