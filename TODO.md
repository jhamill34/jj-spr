# Things to fix

## 1. Add stack PR links in the GithubPR description

When we do a `jj spr diff --all` to submit the stack to github and create PRs that depend on each other
we should go through and update each PR body to include links to the other PRs in this stack. The list
should also show which one the current PR is in the list. 


## 2. Noisy merge commits

Every time we submit the stack to github a merge commit is created for all child PRs in the stack 
to merge in the previous branch into the current PR. This creates a lot of noisy bookkeeping commits
that are mostly empty. We should adding an option to rather than creating merge commits but rebasing 
onto the PRs base branch if needed. We should support both merge commits and rebases. 


