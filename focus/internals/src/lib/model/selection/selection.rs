use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeSet, HashSet},
    fmt::Display,
    path::{PathBuf, Path},
};
use tracing::{debug, error, warn};

use super::*;

/// A structure representing the current selection in memory. Instead of serializing this structure, a PersistedSelection is stored to disk. In addition to that structure being simpler to serialize, the indirection allows for updates to the underlying project definitions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub projects: HashSet<Project>,
    pub targets: HashSet<Target>,
}

impl Selection {
    pub fn from_persisted_selection(
        persisted_selection: PersistedSelection,
        projects: &Projects,
    ) -> Result<Self> {
        let mut selection = Selection::default();
        let operations = Vec::<Operation>::try_from(persisted_selection)
            .context("Structuring a persisted selection as a set of operations")?;
        selection
            .apply_operations(&operations, projects)
            .context("Creating a selection from its persisted form")?;
        Ok(selection)
    }

    pub fn apply_operations(
        &mut self,
        operations: &Vec<Operation>,
        projects: &Projects,
    ) -> Result<OperationResult> {
        let mut processor = SelectionOperationProcessor {
            selection: self,
            projects,
        };
        processor.process(operations)
    }
}

impl TryFrom<&Selection> for TargetSet {
    type Error = anyhow::Error;

    fn try_from(value: &Selection) -> Result<Self, Self::Error> {
        let mut set = value.targets.clone();
        for project in value.projects.iter() {
            set.extend(TargetSet::try_from(project)?);
        }
        Ok(set)
    }
}

impl Display for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "--- Projects ---")?;
        let sorted_projects =
            BTreeSet::<Project>::from_iter(self.projects.iter().filter_map(|project| {
                if project.mandatory {
                    None
                } else {
                    Some(project.to_owned())
                }
            }));

        if sorted_projects.is_empty() {
            writeln!(f, "None selected.")?;
        } else {
            let longest_project_name = sorted_projects
                .iter()
                .fold(0_usize, |highest, project| project.name.len().max(highest));
            for project in sorted_projects.iter() {
                let mut padded_project_name = String::from(&project.name);
                padded_project_name.extend(
                    " ".chars()
                        .cycle()
                        .take(longest_project_name - project.name.len()),
                );

                writeln!(
                    f,
                    "{}   {} ({} {})",
                    padded_project_name,
                    project.description,
                    project.targets.len(),
                    if project.targets.len() == 1 {
                        "target"
                    } else {
                        "targets"
                    }
                )?;
            }
        }
        writeln!(f)?;

        writeln!(f, "--- Targets ---")?;
        let sorted_targets =
            BTreeSet::<String>::from_iter(self.targets.iter().map(|target| target.to_string()));
        if sorted_targets.is_empty() {
            writeln!(f, "None selected.")?;
        } else {
            for target in sorted_targets.iter() {
                writeln!(f, "{}", target)?;
            }
        }

        Ok(())
    }
}

pub(crate) struct SelectionOperationProcessor<'processor> {
    pub selection: &'processor mut Selection,
    pub projects: &'processor Projects,
}

impl<'processor> SelectionOperationProcessor<'processor> {
    pub fn process(&mut self, operations: &Vec<Operation>) -> Result<OperationResult> {
        let mut result: OperationResult = Default::default();

        for operation in operations {
            debug!(?operation, "Processing operation");
            match (&operation.action, &operation.underlying) {
                (OperationAction::Add, Underlying::Target(target)) => {
                    if self.selection.targets.insert(target.clone()) {
                        result.added.insert(operation.underlying.clone());
                        debug!(?target, "Target added to selection")
                    } else {
                        result.ignored.insert(operation.underlying.clone());
                        debug!(?target, "Target already in selection")
                    }
                }
                (OperationAction::Add, Underlying::Project(name)) => {
                    match self.projects.underlying.get(name.as_str()) {
                        Some(project) => {
                            if self.selection.projects.insert(project.clone()) {
                                result.added.insert(operation.underlying.clone());
                                debug!(?project, "Project added to selection");
                            } else {
                                result.ignored.insert(operation.underlying.clone());
                                debug!(?project, "Project already in selection");
                            }
                        }
                        None => {
                            warn!(%name, "Project to be added was not found");
                            result.absent.insert(operation.underlying.clone());
                        }
                    }
                }
                (OperationAction::Remove, Underlying::Target(target)) => {
                    if self.selection.targets.remove(target) {
                        debug!(?target, "Target removed from selection");
                        result.removed.insert(operation.underlying.clone());
                    } else {
                        warn!(?target, "Target to be removed was not in selection");
                        result.ignored.insert(operation.underlying.clone());
                    }
                }
                (OperationAction::Remove, Underlying::Project(name)) => {
                    match self.projects.underlying.get(name) {
                        Some(project) => {
                            if self.selection.projects.remove(project) {
                                debug!(?project, "Project removed from selection");
                                result.removed.insert(operation.underlying.clone());
                            } else {
                                warn!(%name, "Project to be removed was not in selection");
                                result.ignored.insert(operation.underlying.clone());
                            }
                        }
                        None => {
                            error!(%name, "Project to be removed is not a defined project");
                            result.absent.insert(operation.underlying.clone());
                        }
                    }
                }
            }
        }

        Ok(result)
    }
}

pub struct Selections {
    selection_path: PathBuf,
    pub optional_projects: Projects,
    pub mandatory_projects: Projects,
    selection: Selection,
}

impl Selections {
    pub fn from_repo(repo: &Repo) -> Result<Self> {
        let working_tree = repo
            .working_tree()
            .ok_or_else(|| anyhow::anyhow!("The repo must have a working tree"))?;

        let paths = DataPaths::from_working_tree(working_tree)?;
        let optional_projects = Projects::new(
            ProjectSets::new(&paths.project_dir).context("Loading optional projects")?,
        )?;
        let mandatory_projects = Projects::new(
            ProjectSets::new(&paths.focus_dir).context("Loading mandatory projects")?,
        )?;

        Self::new(&paths.selection_file, optional_projects, mandatory_projects)
    }

    fn new(
        selection_path: impl AsRef<Path>,
        optional_projects: Projects,
        mandatory_projects: Projects,
    ) -> Result<Self> {
        let mut instance = Self {
            selection_path: selection_path.as_ref().to_owned(),
            optional_projects,
            mandatory_projects,
            selection: Default::default(),
        };
        instance.reload()?;
        Ok(instance)
    }

    /// Load a selection from the given `path` using project definitions from `projects`.
    fn load(path: impl AsRef<Path>, projects: &Projects) -> Result<Selection> {
        let persisted_selection = load_model(path).context("Loading persisted selection")?;
        Selection::from_persisted_selection(persisted_selection, projects)
    }

    /// Load the selection from disk.
    pub fn reload(&mut self) -> Result<()> {
        let selection: Selection = Self::load(&self.selection_path, &self.optional_projects)?;
        debug!(?selection, path = ?self.selection_path, "Reloaded selection");
        self.selection = selection;
        Ok(())
    }

    /// Save the current selection to the configured `selection_path`.
    pub fn save(&self) -> Result<()> {
        let selection = self.selection.clone();
        let persisted_selection = PersistedSelection::from(&selection);
        store_model(&self.selection_path, &persisted_selection)?;
        debug!(?persisted_selection, path = ?self.selection_path, "Saved selection");
        Ok(())
    }

    /// Returns a Selection combining both user-selected and mandatory projects and targets.
    pub fn computed_selection(&self) -> Result<Selection> {
        let mut selection = self.selection.clone();
        debug!(selected = ?selection, "User-selected projects");
        let mandatory_projects = self
            .mandatory_projects
            .underlying
            .values()
            .cloned()
            .collect::<Vec<Project>>();
        debug!(mandatory = ?mandatory_projects, "Mandatory projects");
        selection.projects.extend(mandatory_projects);
        Ok(selection)
    }

    pub fn mutate(
        &mut self,
        action: OperationAction,
        projects_and_targets: &Vec<String>,
    ) -> Result<bool> {
        let operations = projects_and_targets
            .iter()
            .map(|value| Operation::new(action, value.clone()))
            .collect::<Vec<Operation>>();
        let result = self.process(&operations)?;
        if !result.is_success() {
            bail!("Failed to update the selection");
        }
        Ok(result.changed())
    }

    pub fn process(&mut self, operations: &Vec<Operation>) -> Result<OperationResult> {
        let mut selection = self.selection.clone();
        let mut processor = SelectionOperationProcessor {
            selection: &mut selection,
            projects: &self.optional_projects,
        };
        match processor.process(operations) {
            Ok(result) => {
                if result.is_success() {
                    self.selection = selection;
                } else {
                    error!("The selection will not be updated because an error occured while applying the requested changes");
                }
                Ok(result)
            }
            Err(e) => Err(e),
        }
    }
}
