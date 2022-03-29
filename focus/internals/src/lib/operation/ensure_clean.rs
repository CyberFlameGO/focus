use std::{path::Path, sync::Arc};

use anyhow::{bail, Context, Result};

use crate::model::repo::Repo;
use focus_util::app::App;

use super::util::perform;

pub fn run(sparse_repo_path: &Path, app: Arc<App>) -> Result<()> {
    let repo = Repo::open(sparse_repo_path, app.clone())
        .with_context(|| format!("Opening repo in {}", sparse_repo_path.display()))?;
    let working_tree = match repo.working_tree() {
        Some(t) => t,
        None => bail!("No working tree"),
    };

    let clean = perform("Checking that sparse repo is in a clean state", || {
        working_tree.is_clean(app.clone())
    })?;

    if !clean {
        eprintln!("The working tree in the sparse repo must be in a clean state. Commit or stash changes and try to run the sync again.");
        bail!("Sparse repo working tree is not in a clean state");
    }

    Ok(())
}
