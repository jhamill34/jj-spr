# REST to GraphQL Migration Summary

## What We've Done So Far

1. **Current User (viewer) Migration**
   - Created `spr/src/gql/viewer_query.graphql` to fetch the current authenticated user's `login` and `name`.
   - Added `ViewerQuery` struct in `src/github.rs`.
   - Replaced all uses of `octocrab.current().user().await?` with the new GraphQL query.
   - Tested and confirmed the `init` flow works as expected.

2. **Repository Info Migration**
   - Created `spr/src/gql/repository_query.graphql` to fetch a repository's default branch.
   - Added `RepositoryQuery` struct in `src/github.rs`.
   - Replaced REST call for repository info in `init.rs` with the new GraphQL query.
   - Tested and confirmed the flow works as expected.

3. **User Info by Login Migration**
   - Created `spr/src/gql/user_query.graphql` to fetch a user's `login` and `name` by login.
   - Added `UserQuery` struct in `src/github.rs`.
   - Refactored `get_github_user` to use the new GraphQL query.
   - Set `is_collaborator` to `false` (not available in GraphQL user object).
   - Confirmed all call sites are updated.

4. **Team Info Migration**
   - Created `spr/src/gql/team_query.graphql` to fetch a team's `id`, `name`, `slug`, and `description` by org and slug.
   - Added `TeamQuery` struct in `src/github.rs`.
   - Refactored `get_github_team` to use the new GraphQL query.
   - Updated return type to `team_query::TeamQueryOrganizationTeam`.

## What Still Needs To Be Done

- [x] Update all call sites of `get_github_team` to expect the new return type (`team_query::TeamQueryOrganizationTeam`).
- [x] If any features require `is_collaborator` or other REST-only fields, consider additional GraphQL queries or permission checks.
- [x] Remove any now-unused REST-specific code or dependencies (except for device authentication).
- [x] Do a final audit for any remaining direct REST calls to `api.github.com` (excluding device authentication).
- [ ] Track REST endpoints still in use for PR mutations (creation, update, reviewer assignment, merge) and migrate to GraphQL when GitHub API supports it:
    - PR creation: `octocrab::instance().pulls(...).create(...)` (`spr/src/github.rs`)
    - PR update: `octocrab::instance().patch::<octocrab::models::pulls::PullRequest, _, _>(...)` (`spr/src/github.rs`)
    - Request reviewers: `octocrab::instance().post(.../requested_reviewers, ...)` (`spr/src/github.rs`)
    - PR merge: `octocrab::instance().pulls(...).merge(...)` (`spr/src/commands/land.rs`)
- [ ] Add or update tests for all migrated code paths.
- [ ] Update documentation to reflect the new GraphQL-based implementation.

---

**This file summarizes the migration from GitHub REST API to GraphQL for user, repository, and team info in the project.**
