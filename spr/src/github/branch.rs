use crate::error::{Error, Result};

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
