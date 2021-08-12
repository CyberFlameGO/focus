mod detail;
mod model;
mod sandbox;
mod sandbox_command;
mod sparse_repos;
mod subcommands;
mod testing;
mod temporary_working_directory;
mod working_tree_synchronizer;
#[macro_use]
extern crate lazy_static;

use anyhow::{Context, Result};
use env_logger::{self, Env};
use sandbox::Sandbox;
use std::{path::PathBuf, str::FromStr, sync::Arc};
use structopt::StructOpt;

#[derive(Debug)]
struct Coordinates(Vec<String>);

impl FromStr for Coordinates {
    type Err = std::string::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let splayed: Vec<_> = s.split(",").map(|s| s.to_owned()).collect();
        Ok(Coordinates(splayed))
    }
}

#[derive(StructOpt, Debug)]
enum Subcommand {
    CreateSparseClone {
        #[structopt(long)]
        name: String,

        #[structopt(long, parse(from_os_str))]
        dense_repo: PathBuf,

        #[structopt(long, parse(from_os_str))]
        sparse_repo: PathBuf,

        #[structopt(long)]
        branch: String,

        #[structopt(long)]
        coordinates: Coordinates,

        #[structopt(long)]
        filter_sparse: bool,
    },

    Sync {
        #[structopt(long, parse(from_os_str), default_value = ".dense")]
        dense_repo: PathBuf,

        #[structopt(long, parse(from_os_str), default_value = ".")]
        sparse_repo: PathBuf,
    },

    AvailableLayers {
        #[structopt(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,
    },

    SelectedLayers {
        #[structopt(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,
    },

    PushLayer {
        #[structopt(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        names: Vec<String>,
    },

    PopLayer {
        #[structopt(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        #[structopt(long, default_value = "1")]
        count: usize,
    },

    RemoveLayer {
        #[structopt(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        names: Vec<String>,
    },
}

#[derive(StructOpt, Debug)]
#[structopt(about = "Focused Development Tools")]
struct ParachuteOpts {
    #[structopt(long)]
    preserve_sandbox: bool,

    #[structopt(subcommand)]
    cmd: Subcommand,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let opt = ParachuteOpts::from_args();

    let sandbox = Arc::new(Sandbox::new(opt.preserve_sandbox).context("Creating a sandbox")?);

    match opt.cmd {
        Subcommand::CreateSparseClone {
            name,
            dense_repo,
            sparse_repo,
            branch,
            coordinates,
            filter_sparse,
        } => sparse_repos::create_sparse_clone(
            &name,
            &dense_repo,
            &sparse_repo,
            &branch,
            &coordinates.0,
            filter_sparse,
            sandbox,
        ),

        Subcommand::Sync {
            dense_repo,
            sparse_repo,
        } => subcommands::sync::run(&sandbox, &dense_repo, &sparse_repo),

        Subcommand::AvailableLayers { repo } => subcommands::available_layers::run(&repo),

        Subcommand::SelectedLayers { repo } => subcommands::selected_layers::run(&repo),

        Subcommand::PushLayer { repo, names } => subcommands::push_layer::run(&repo, names),

        Subcommand::PopLayer { repo, count } => subcommands::pop_layer::run(&repo, count),

        Subcommand::RemoveLayer { repo, names } => subcommands::remove_layer::run(&repo, names),
    }
}
