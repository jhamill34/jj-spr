/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::error::Error;
use crate::error::Result;
use crate::github::gh_trait::GitHubClient;
use crate::github::gql::search_query;

pub async fn list<G>(gh: &G, config: &crate::config::Config) -> Result<()>
where
    G: GitHubClient,
{
    let term = console::Term::stdout();
    let nodes = gh
        .list_open_reviews(config.owner.clone(), config.repo.clone())
        .await?;

    for pr in nodes {
        if let search_query::SearchQuerySearchNodes::PullRequest(pr) = pr {
            let decision = match pr.review_decision {
                Some(search_query::PullRequestReviewDecision::APPROVED) => {
                    console::style("Accepted").green()
                }
                Some(search_query::PullRequestReviewDecision::CHANGES_REQUESTED) => {
                    console::style("Changed Requested").red()
                }
                None | Some(search_query::PullRequestReviewDecision::REVIEW_REQUIRED) => {
                    console::style("Pending")
                }
                Some(search_query::PullRequestReviewDecision::Other(ref d)) => {
                    console::style(d.as_str())
                }
            };
            term.write_line(&format!(
                "{} {} {}",
                decision,
                console::style(&pr.title).bold(),
                console::style(&pr.url).dim(),
            ))
            .map_err(|_| Error::new("Unexpected error"))?;
        }
    }

    Ok(())
}
