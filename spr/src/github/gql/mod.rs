use graphql_client::GraphQLQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/github/gql/schema.docs.graphql",
    query_path = "src/github/gql/pullrequest_query.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestQuery;
type GitObjectID = String;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/github/gql/schema.docs.graphql",
    query_path = "src/github/gql/pullrequest_mergeability_query.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestMergeabilityQuery;

#[allow(clippy::upper_case_acronyms)]
type URI = String;
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/github/gql/schema.docs.graphql",
    query_path = "src/github/gql/open_reviews.graphql",
    response_derives = "Debug"
)]
pub struct SearchQuery;
