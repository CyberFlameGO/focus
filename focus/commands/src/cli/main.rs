#![allow(clippy::too_many_arguments)]

use std::{
    convert::TryFrom,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::Parser;
use git2::Repository;

use focus_internals::{
    app::{App, ExitCode},
    coordinate::Coordinate,
    model::layering::LayerSets,
    operation::{
        self,
        maintenance::{self, ScheduleOpts},
    },
    tracing as focus_tracing,
    tracker::Tracker,
    util::{backed_up_file::BackedUpFile, git_helper, lock_file::LockFile, paths, time::FocusTime},
};
use strum::VariantNames;
use tracing::{debug, info, info_span};

#[derive(Parser, Debug)]
enum Subcommand {
    /// Create a sparse clone from named layers or ad-hoc build coordinates
    Clone {
        /// Path to the repository to clone.
        #[clap(long, default_value = "~/workspace/source")]
        dense_repo: String,

        /// Path where the new sparse repository should be created.
        #[clap(parse(from_os_str))]
        sparse_repo: PathBuf,

        /// The name of the branch to clone.
        #[clap(long, default_value = "master")]
        branch: String,

        /// Days of history to maintain in the sparse repo.
        #[clap(long, default_value = "90")]
        days_of_history: u64,

        /// Copy only the specified branch rather than all local branches.
        #[clap(parse(try_from_str), default_value = "true")]
        copy_branches: bool,

        /// Named layers and ad-hoc coordinates to include in the clone. Named layers are loaded from the dense repo's `focus/projects` directory.
        coordinates_and_layers: Vec<String>,
    },

    /// Update the sparse checkout to reflect changes to the build graph.
    Sync {
        /// Path to the sparse repository.
        #[clap(parse(from_os_str), default_value = ".")]
        sparse_repo: PathBuf,
    },

    /// Interact with repos configured on this system. Run `focus repo help` for more information.
    Repo {
        #[clap(subcommand)]
        subcommand: RepoSubcommand,
    },

    /// Interact with the stack of selected layers. Run `focus layer help` for more information.
    Layer {
        /// Path to the repository.
        #[clap(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        #[clap(subcommand)]
        subcommand: LayerSubcommand,
    },

    /// Interact with the ad-hoc coordinate stack. Run `focus adhoc help` for more information.
    Adhoc {
        /// Path to the repository.
        #[clap(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        #[clap(subcommand)]
        subcommand: AdhocSubcommand,
    },

    /// Detect whether there are changes to the build graph (used internally)
    DetectBuildGraphChanges {
        /// Path to the repository.
        #[clap(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        /// Extra arguments.
        args: Vec<String>,
    },

    /// Utility methods for listing and expiring outdated refs. Used to maintain a time windowed
    /// repository.
    Refs {
        #[clap(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        #[clap(subcommand)]
        subcommand: RefsSubcommand,
    },

    /// Set up an initial clone of the repo from the remote
    Init {
        /// By default we take 90 days of history, pass a date with this option
        /// if you want a different amount of history
        #[clap(long, parse(try_from_str = operation::init::parse_shallow_since_date))]
        shallow_since: Option<NaiveDate>,

        /// This command will only ever clone a single ref, by default this is
        /// "master". If you wish to clone a different branch, then use this option
        #[clap(long, default_value = "master")]
        branch_name: String,

        #[clap(long)]
        no_checkout: bool,

        /// The default is to pass --no-tags to clone, this option, if given,
        /// will cause git to do its normal default tag following behavior
        #[clap(long)]
        follow_tags: bool,

        /// If not given, we use --filter=blob:none. To use a different filter
        /// argument, use this option. To disable filtering, use --no-filter.
        #[clap(long, default_value = "blob:none")]
        filter: String,

        /// Do not pass a filter flag to git-clone. If both --no-filter and --filter
        /// options are given, --no-filter wins
        #[clap(long)]
        no_filter: bool,

        #[clap(long)]
        bare: bool,

        #[clap(long)]
        sparse: bool,

        #[clap(long)]
        progress: bool,

        #[clap(long)]
        push_url: Option<String>,

        #[clap(long, default_value=operation::init::SOURCE_RO_URL)]
        fetch_url: String,

        #[clap()]
        target_path: String,
    },

    #[clap(hide = true)]
    Maintenance {
        /// The git config key to look for paths of repos to run maintenance in. Defaults to
        /// 'maintenance.repo'
        #[clap(long, default_value=operation::maintenance::DEFAULT_CONFIG_KEY, global = true)]
        git_config_key: String,

        #[clap(subcommand)]
        subcommand: MaintenanceSubcommand,
    },

    /// git-trace allows one to transform the output of GIT_TRACE2_EVENT data into a format
    /// that the chrome://tracing viewer can understand and display. This is a convenient way
    /// to analyze the timing and call tree of a git command.
    ///
    /// For example, to analyze git gc:
    /// ```
    /// $ GIT_TRACE2_EVENT=/tmp/gc.json git gc
    /// $ focus git-trace /tmp/gc.json /tmp/chrome-trace.json
    /// ````
    /// Then open chrome://tracing in your browser and load the /tmp/chrome-trace.json flie.
    GitTrace { input: PathBuf, output: PathBuf },
}

/// Helper method to extract subcommand name. Tool insights client uses this to set
/// feature name.
fn feature_name_for(subcommand: &Subcommand) -> String {
    let subcommand_name = match subcommand {
        Subcommand::Clone { .. } => "clone",
        Subcommand::Sync { .. } => "sync",
        Subcommand::Repo { subcommand } => match subcommand {
            RepoSubcommand::List { .. } => "repo-list",
            RepoSubcommand::Repair { .. } => "repo-repair",
        },
        Subcommand::Layer { subcommand, .. } => match subcommand {
            LayerSubcommand::Available { .. } => "layer-available",
            LayerSubcommand::List { .. } => "layer-list",
            LayerSubcommand::Push { .. } => "layer-push",
            LayerSubcommand::Pop { .. } => "layer-pop",
            LayerSubcommand::Remove { .. } => "layer-remove",
        },
        Subcommand::Adhoc { subcommand, .. } => match subcommand {
            AdhocSubcommand::List { .. } => "adhoc-list",
            AdhocSubcommand::Push { .. } => "adhoc-push",
            AdhocSubcommand::Pop { .. } => "adhoc-pop",
            AdhocSubcommand::Remove { .. } => "adhoc-remove",
        },
        Subcommand::DetectBuildGraphChanges { .. } => "detect-build-graph-changes",
        Subcommand::Refs { subcommand, .. } => match subcommand {
            RefsSubcommand::Delete { .. } => "refs-delete",
            RefsSubcommand::ListExpired { .. } => "refs-list-expired",
            RefsSubcommand::ListCurrent { .. } => "refs-list-current",
        },
        Subcommand::Init { .. } => "init",
        Subcommand::Maintenance { subcommand, .. } => match subcommand {
            MaintenanceSubcommand::Run { .. } => "maintenance-run",
            MaintenanceSubcommand::Register { .. } => "maintenance-register",
            MaintenanceSubcommand::SetDefaultConfig { .. } => "maintenance-set-default-config",
            MaintenanceSubcommand::Schedule { subcommand } => match subcommand {
                MaintenanceScheduleSubcommand::Enable { .. } => "maintenance-schedule-enable",
                MaintenanceScheduleSubcommand::Disable { .. } => "maintenance-schedule-disable",
            },
        },
        Subcommand::GitTrace { .. } => "git-trace",
    };
    subcommand_name.into()
}

#[derive(Parser, Debug)]
enum MaintenanceSubcommand {
    /// Runs global (i.e. system-wide) git maintenance tasks on repositories listed in
    /// the $HOME/.gitconfig's `maintenance.repo` multi-value config key. This command
    /// is usually run by a system-specific scheduler (eg. launchd) so it's unlikely that
    /// end users would need to invoke this command directly.
    Run {
        /// The absolute path to the git binary to use. If not given, the default MDE path
        /// will be used.
        #[clap(long, default_value = maintenance::DEFAULT_GIT_BINARY_PATH_FOR_PLISTS, env = "FOCUS_GIT_BINARY_PATH")]
        git_binary_path: PathBuf,

        /// The git config file to use to read the list of repos to run maintenance in. If not
        /// given, then use the default 'global' config which is usually $HOME/.gitconfig.
        #[clap(long, env = "FOCUS_GIT_CONFIG_PATH")]
        git_config_path: Option<PathBuf>,

        /// run maintenance on repos tracked by focus rather than reading from the
        /// git global config file
        #[clap(long, conflicts_with = "git-config-path", env = "FOCUS_TRACKED")]
        tracked: bool,

        /// The time period of job to run
        #[clap(
            long,
            possible_values=operation::maintenance::TimePeriod::VARIANTS,
            default_value="hourly",
            env = "FOCUS_TIME_PERIOD"
        )]
        time_period: operation::maintenance::TimePeriod,
    },

    SetDefaultConfig {},

    Register {
        #[clap(long, parse(from_os_str))]
        repo_path: Option<PathBuf>,

        #[clap(long, parse(from_os_str))]
        git_config_path: Option<PathBuf>,
    },

    Schedule {
        #[clap(subcommand)]
        subcommand: MaintenanceScheduleSubcommand,
    },
}

#[derive(Parser, Debug)]
enum MaintenanceScheduleSubcommand {
    /// Set up a system-appropriate periodic job (launchctl, systemd, etc.) for running
    /// maintenance tasks on hourly, daily, and weekly bases
    Enable {
        /// The time period of job to schedule
        #[clap(
            long,
            possible_values=operation::maintenance::TimePeriod::VARIANTS,
            default_value="hourly",
            env = "FOCUS_TIME_PERIOD"
        )]
        time_period: operation::maintenance::TimePeriod,

        /// register jobs for all time periods
        #[clap(long, conflicts_with = "time-period", env = "FOCUS_ALL")]
        all: bool,

        /// path to the focus binary, defaults to the current running focus binary
        #[clap(long)]
        focus_path: Option<PathBuf>,

        /// path to git
        #[clap(long, default_value = operation::maintenance::DEFAULT_GIT_BINARY_PATH_FOR_PLISTS, env = "FOCUS_GIT_BINARY_PATH")]
        git_binary_path: PathBuf,

        /// Normally, we check to see if the scheduled job is already defined and if it is
        /// we do nothing. IF this flag is given, stop the existing job, remove its definition,
        /// rewrite the job manifest (eg. plist) and reload it.
        #[clap(long, env = "FOCUS_FORCE_RELOAD")]
        force_reload: bool,

        /// Add a flag to the maintenance cmdline that will run the tasks against all focus tracked repos
        #[clap(long, env = "FOCUS_TRACKED")]
        tracked: bool,
    },

    /// Unload all the scheduled jobs from the system scheduler (if loaded).
    Disable {
        /// Delete the plist after unloading
        #[clap(long)]
        delete: bool,
    },
}

#[derive(Parser, Debug)]
enum RepoSubcommand {
    /// List registered repositories
    List {},

    /// Attempt to repair the registry of repositories
    Repair {},
}

#[derive(Parser, Debug)]
enum LayerSubcommand {
    /// List all available layers
    Available {},

    /// List currently selected layers
    List {},

    /// Push a layer onto the top of the stack of currently selected layers
    Push {
        /// Names of layers to push.
        names: Vec<String>,
    },

    /// Pop one or more layer(s) from the top of the stack of current selected layers
    Pop {
        /// The number of layers to pop.
        #[clap(long, default_value = "1")]
        count: usize,
    },

    /// Filter out one or more layer(s) from the stack of currently selected layers
    Remove {
        /// Names of the layers to be removed.
        names: Vec<String>,
    },
}

#[derive(Parser, Debug)]
enum AdhocSubcommand {
    /// List the contents of the ad-hoc coordinate stack
    List {},

    /// Push one or more coordinate(s) onto the top of the ad-hoc coordinate stack
    Push {
        /// Names of coordinates to push.
        names: Vec<String>,
    },

    /// Pop one or more coordinates(s) from the top of the ad-hoc coordinate stack
    Pop {
        /// The number of coordinates to pop.
        #[clap(long, default_value = "1")]
        count: usize,
    },

    /// Filter out one or more coordinate(s) from the ad-hoc coordinate stack
    Remove {
        /// Names of the coordinates to be removed.
        names: Vec<String>,
    },
}

#[derive(Parser, Debug)]
enum RefsSubcommand {
    /// Expires refs that are outside the window of "current refs"
    Delete {
        #[clap(long, default_value = "2021-01-01")]
        cutoff_date: String,

        #[clap(long)]
        use_transaction: bool,

        /// If true, then ensure the merge base falls after the cutoff date.
        /// this avoids the problem of refs that refer to commits that are not
        /// included in master
        #[clap(short = 'm', long = "check-merge-base")]
        check_merge_base: bool,
    },

    ListExpired {
        #[clap(long, default_value = "2021-01-01")]
        cutoff_date: String,

        /// If true, then ensure the merge base falls after the cutoff date.
        /// this avoids the problem of refs that refer to commits that are not
        /// included in master
        #[clap(short = 'm', long = "check-merge-base")]
        check_merge_base: bool,
    },

    /// Output a list of still current (I.e. non-expired) refs
    ListCurrent {
        #[clap(long, default_value = "2021-01-01")]
        cutoff_date: String,

        /// If true, then ensure the merge base falls after the cutoff date.
        /// this avoids the problem of refs that refer to commits that are not
        /// included in master
        #[clap(short = 'm', long = "check-merge-base")]
        check_merge_base: bool,
    },
}

#[derive(Parser, Debug)]
#[clap(about = "Focused Development Tools")]
struct FocusOpts {
    /// Preserve the created sandbox directory for inspecting logs and other files.
    #[clap(long, global = true, env = "FOCUS_PRESERVE_SANDBOX")]
    preserve_sandbox: bool,

    /// Number of threads to use when performing parallel resolution (where possible).
    #[clap(
        long,
        default_value = "0",
        global = true,
        env = "FOCUS_RESOLUTION_THREADS"
    )]
    resolution_threads: usize,

    /// Change to the provided directory before doing anything else.
    #[clap(
        short = 'C',
        long = "work-dir",
        global = true,
        env = "FOCUS_WORKING_DIRECTORY"
    )]
    working_directory: Option<PathBuf>,

    /// Directory where the log files should be written. This directory will be
    /// created if it doesn't exist. If not given, we'll use a system-appropriate
    /// default (~/Library/Logs/focus/ for mac and ~/.local/focus/log/ for linux).
    /// Can also use FOCUS_LOG_DIR to set this.
    #[clap(long, global = true, env = "FOCUS_LOG_DIR")]
    log_dir: Option<PathBuf>,

    /// Disables use of ANSI color escape sequences
    #[clap(long, global = true, env = "NO_COLOR")]
    no_color: bool,

    #[clap(subcommand)]
    cmd: Subcommand,
}

fn ensure_directories_exist() -> Result<()> {
    Tracker::default()
        .ensure_directories_exist()
        .context("creating directories for the tracker")?;

    Ok(())
}

fn hold_lock_file(repo: &Path) -> Result<LockFile> {
    let path = repo.join(".focus").join("focus.lock");
    LockFile::new(&path)
}

#[tracing::instrument]
fn run_subcommand(app: Arc<App>, options: FocusOpts) -> Result<ExitCode> {
    let cloned_app = app.clone();
    let ti_client = cloned_app.tool_insights_client();
    let feature_name = feature_name_for(&options.cmd);
    ti_client.get_context().set_tool_feature_name(&feature_name);
    let span = info_span!("Running subcommand", ?feature_name);
    let _guard = span.enter();

    match options.cmd {
        Subcommand::Clone {
            dense_repo,
            sparse_repo,
            branch,
            days_of_history,
            copy_branches,
            coordinates_and_layers,
        } => {
            let origin = operation::clone::Origin::try_from(dense_repo.as_str())?;
            let sparse_repo = {
                let current_dir =
                    std::env::current_dir().context("Failed to obtain current directory")?;
                let expanded = paths::expand_tilde(sparse_repo)
                    .context("Failed to expand sparse repo path")?;
                current_dir.join(expanded)
            };

            info!("Cloning {:?} into {}", dense_repo, sparse_repo.display());

            let (coordinates, layers): (Vec<String>, Vec<String>) = coordinates_and_layers
                .into_iter()
                .partition(|item| Coordinate::try_from(item.as_str()).is_ok());

            // Add coordinates length to TI custom map.
            ti_client.get_context().add_to_custom_map(
                "coordinates_and_layers_count",
                coordinates.len().to_string(),
            );

            operation::clone::run(
                origin,
                sparse_repo,
                branch,
                coordinates,
                layers,
                copy_branches,
                days_of_history,
                app,
            )?;
            Ok(ExitCode(0))
        }

        Subcommand::Sync { sparse_repo } => {
            // TODO: Add total number of paths in repo to TI.
            let sparse_repo = paths::expand_tilde(sparse_repo)?;
            let _lock_file = hold_lock_file(&sparse_repo)?;
            operation::sync::run(&sparse_repo, app)?;
            Ok(ExitCode(0))
        }

        Subcommand::Refs {
            repo: repo_path,
            subcommand,
        } => {
            let repo = Repository::open(repo_path).context("opening the repo")?;
            match subcommand {
                RefsSubcommand::Delete {
                    cutoff_date,
                    use_transaction,
                    check_merge_base,
                } => {
                    let cutoff = FocusTime::parse_date(cutoff_date)?;
                    operation::refs::expire_old_refs(
                        &repo,
                        cutoff,
                        check_merge_base,
                        use_transaction,
                        app,
                    )?;
                    Ok(ExitCode(0))
                }

                RefsSubcommand::ListExpired {
                    cutoff_date,
                    check_merge_base,
                } => {
                    let cutoff = FocusTime::parse_date(cutoff_date)?;
                    let operation::refs::PartitionedRefNames {
                        current: _,
                        expired,
                    } = operation::refs::PartitionedRefNames::for_repo(
                        &repo,
                        cutoff,
                        check_merge_base,
                    )?;

                    println!("{}", expired.join("\n"));

                    Ok(ExitCode(0))
                }

                RefsSubcommand::ListCurrent {
                    cutoff_date,
                    check_merge_base,
                } => {
                    let cutoff = FocusTime::parse_date(cutoff_date)?;
                    let operation::refs::PartitionedRefNames {
                        current,
                        expired: _,
                    } = operation::refs::PartitionedRefNames::for_repo(
                        &repo,
                        cutoff,
                        check_merge_base,
                    )?;

                    println!("{}", current.join("\n"));

                    Ok(ExitCode(0))
                }
            }
        }

        Subcommand::Repo { subcommand } => match subcommand {
            RepoSubcommand::List {} => {
                operation::repo::list()?;
                Ok(ExitCode(0))
            }
            RepoSubcommand::Repair {} => {
                operation::repo::repair(app)?;
                Ok(ExitCode(0))
            }
        },

        Subcommand::DetectBuildGraphChanges { repo, args } => {
            let repo = paths::expand_tilde(repo)?;
            let repo = git_helper::find_top_level(app.clone(), &repo)
                .context("Failed to canonicalize repo path")?;
            operation::detect_build_graph_changes::run(&repo, args, app)
        }

        Subcommand::Layer { repo, subcommand } => {
            paths::assert_focused_repo(&repo)?;
            let _lock_file = hold_lock_file(&repo)?;
            ti_client.get_context().set_tool_feature_name("layer");

            let should_check_tree_cleanliness = match subcommand {
                LayerSubcommand::Available {} => false,
                LayerSubcommand::List {} => false,
                LayerSubcommand::Push { names: _ } => true,
                LayerSubcommand::Pop { count: _ } => true,
                LayerSubcommand::Remove { names: _ } => true,
            };
            if should_check_tree_cleanliness {
                operation::ensure_clean::run(repo.as_path(), app.clone())
                    .context("Ensuring working trees are clean failed")?;
            }

            let selected_layer_stack_backup = {
                let sets = LayerSets::new(&repo);
                if sets.selected_layer_stack_path().is_file() {
                    Some(BackedUpFile::new(
                        sets.selected_layer_stack_path().as_path(),
                    )?)
                } else {
                    None
                }
            };

            let mutated = match subcommand {
                LayerSubcommand::Available {} => operation::layer::available(&repo)?,
                LayerSubcommand::List {} => operation::layer::list(&repo)?,
                LayerSubcommand::Push { names } => operation::layer::push(&repo, names)?,
                LayerSubcommand::Pop { count } => operation::layer::pop(&repo, count)?,
                LayerSubcommand::Remove { names } => operation::layer::remove(&repo, names)?,
            };

            if mutated {
                info!("Syncing focused paths since the selected content has changed");
                operation::sync::run(repo.as_path(), app)
                    .context("Sync failed; changes to the stack will be reverted.")?;
            }

            // If there was a change, the sync succeeded, so we we can discard the backup.
            if let Some(backup) = selected_layer_stack_backup {
                backup.set_restore(false);
            }

            Ok(ExitCode(0))
        }

        Subcommand::Adhoc { repo, subcommand } => {
            paths::assert_focused_repo(&repo)?;
            let _lock_file = hold_lock_file(&repo)?;

            let should_check_tree_cleanliness = match subcommand {
                AdhocSubcommand::List {} => false,
                AdhocSubcommand::Push { names: _ } => true,
                AdhocSubcommand::Pop { count: _ } => true,
                AdhocSubcommand::Remove { names: _ } => true,
            };
            if should_check_tree_cleanliness {
                operation::ensure_clean::run(repo.as_path(), app.clone())
                    .context("Ensuring working trees are clean failed")?;
            }

            let adhoc_layer_set_backup = {
                let sets = LayerSets::new(&repo);
                if sets.adhoc_layer_path().is_file() {
                    Some(BackedUpFile::new(sets.adhoc_layer_path().as_path())?)
                } else {
                    None
                }
            };

            let mutated: bool = match subcommand {
                AdhocSubcommand::List {} => operation::adhoc::list(repo.clone())?,
                AdhocSubcommand::Push { names } => operation::adhoc::push(repo.clone(), names)?,
                AdhocSubcommand::Pop { count } => operation::adhoc::pop(repo.clone(), count)?,
                AdhocSubcommand::Remove { names } => operation::adhoc::remove(repo.clone(), names)?,
            };

            if mutated {
                info!("Syncing focused paths since the selected content has changed");
                operation::sync::run(repo.as_path(), app)
                    .context("Sync failed; changes to the stack will be reverted.")?;
            }

            // Sync (if necessary) succeeded, so skip reverting the ad-hoc coordinate stack.
            if let Some(backup) = adhoc_layer_set_backup {
                backup.set_restore(false);
            }

            Ok(ExitCode(0))
        }

        Subcommand::Init {
            shallow_since,
            branch_name,
            no_checkout,
            follow_tags,
            filter,
            no_filter,
            bare,
            sparse,
            progress,
            fetch_url,
            push_url,
            target_path,
        } => {
            let expanded = paths::expand_tilde(target_path)
                .context("expanding tilde on target_path argument")?;

            let target = expanded.as_path();

            let mut init_opts: Vec<operation::init::InitOpt> = Vec::new();

            let mut add_if_true = |n: bool, opt: operation::init::InitOpt| {
                if n {
                    init_opts.push(opt)
                };
            };

            add_if_true(no_checkout, operation::init::InitOpt::NoCheckout);
            add_if_true(bare, operation::init::InitOpt::Bare);
            add_if_true(sparse, operation::init::InitOpt::Sparse);
            add_if_true(follow_tags, operation::init::InitOpt::FollowTags);
            add_if_true(progress, operation::init::InitOpt::Progress);

            info!("Setting up a copy of the repo in {:?}", target);

            operation::init::run(
                shallow_since,
                Some(branch_name),
                if no_filter { None } else { Some(filter) },
                fetch_url,
                push_url,
                target.to_owned(),
                init_opts,
                app,
            )?;

            Ok(ExitCode(0))
        }

        Subcommand::Maintenance {
            subcommand,
            git_config_key,
        } => match subcommand {
            MaintenanceSubcommand::Run {
                git_binary_path,
                tracked,
                git_config_path,
                time_period,
            } => {
                operation::maintenance::run(
                    operation::maintenance::RunOptions {
                        git_binary_path,
                        git_config_key,
                        git_config_path,
                        tracked,
                    },
                    time_period,
                    app,
                )?;
                Ok(ExitCode(0))
            }

            MaintenanceSubcommand::Register {
                repo_path,
                git_config_path,
            } => {
                operation::maintenance::register(operation::maintenance::RegisterOpts {
                    repo_path,
                    git_config_key,
                    global_config_path: git_config_path,
                })?;
                Ok(ExitCode(0))
            }

            MaintenanceSubcommand::SetDefaultConfig { .. } => {
                operation::maintenance::set_default_git_maintenance_config(&std::env::current_dir()?)?;
                Ok(ExitCode(0))
            }

            MaintenanceSubcommand::Schedule { subcommand } => match subcommand {
                MaintenanceScheduleSubcommand::Enable {
                    time_period,
                    all,
                    focus_path,
                    git_binary_path,
                    force_reload,
                    tracked,
                } => {
                    maintenance::schedule_enable(maintenance::ScheduleOpts {
                        time_period: if all { None } else { Some(time_period) },
                        git_path: git_binary_path,
                        focus_path: match focus_path {
                            Some(fp) => fp,
                            None => std::env::current_exe()
                                .context("could not determine current executable path")?,
                        },
                        skip_if_already_scheduled: !force_reload,
                        tracked,
                    })?;
                    Ok(ExitCode(0))
                }

                MaintenanceScheduleSubcommand::Disable { delete } => {
                    maintenance::schedule_disable(delete)?;
                    Ok(ExitCode(0))
                }
            },
        },

        Subcommand::GitTrace { input, output } => {
            focus_internals::tracing::Trace::git_trace_from(input)?.write_trace_json_to(output)?;
            Ok(ExitCode(0))
        }
    }
}

fn setup_thread_pool(resolution_threads: usize) -> Result<()> {
    if resolution_threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(resolution_threads)
            .build_global()
            .context("Failed to create the task thread pool")?;
    }

    Ok(())
}

// TODO: there needs to be a way to know if we should re-load the plists, (eg. on a version change)
fn setup_maintenance_scheduler(opts: &FocusOpts) -> Result<()> {
    if std::env::var("FOCUS_NO_SCHEDULE").is_ok() {
        return Ok(());
    }

    match opts.cmd {
        Subcommand::Clone {..}
        | Subcommand::Sync {..}
        | Subcommand::Layer {..}
        | Subcommand::Adhoc {..}
        | Subcommand::Init {..} => operation::maintenance::schedule_enable(ScheduleOpts::default()),
        _ => Ok(())
    }
}

/// Run the main and any destructors. Local variables are not guaranteed to be
/// dropped if `std::process::exit` is called, so make sure to bubble up the
/// return code to the top level, which is the only place in the code that's
/// allowed to call `std::process::exit`.
fn main_and_drop_locals() -> Result<ExitCode> {
    let started_at = Instant::now();
    let options = FocusOpts::parse();

    let FocusOpts {
        preserve_sandbox,
        resolution_threads,
        working_directory,
        log_dir,
        no_color,
        cmd: _,
    } = &options;

    if let Some(working_directory) = working_directory {
        std::env::set_current_dir(working_directory).context("Switching working directory")?;
    }
    setup_thread_pool(*resolution_threads)?;

    let is_tty = termion::is_tty(&std::io::stdout());

    let _guard = if is_tty {
        focus_tracing::Guard::default()
    } else {
        focus_tracing::init_tracing(focus_tracing::TracingOpts {
            is_tty,
            no_color: *no_color,
            log_dir: log_dir.to_owned(),
        })?
    };

    ensure_directories_exist().context("Failed to create necessary directories")?;
    setup_maintenance_scheduler(&options).context("Failed to setup maintenance scheduler")?;

    let app = Arc::from(App::new(*preserve_sandbox)?);
    let ti_context = app.tool_insights_client();

    let exit_code = match run_subcommand(app.clone(), options) {
        Ok(exit_code) => {
            ti_context
                .get_inner()
                .write_invocation_message(Some(0), None);
            exit_code
        }
        Err(e) => {
            ti_context
                .get_inner()
                .write_invocation_message(Some(1), None);
            return Err(e);
        }
    };

    let total_runtime = started_at.elapsed();
    debug!(
        total_runtime_secs = total_runtime.as_secs_f32(),
        "Finished normally"
    );

    Ok(exit_code)
}

fn main() -> Result<()> {
    let ExitCode(exit_code) = main_and_drop_locals()?;
    std::process::exit(exit_code);
}
