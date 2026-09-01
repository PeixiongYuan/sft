mod app;
mod askpass;
mod browser;
mod history;
mod path_store;
mod remote;
mod ssh_config;
mod transfer;
mod tui;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use app::App;

#[derive(Debug, Parser)]
#[command(
    name = "sft",
    version,
    about = "Split-pane SSH file transfer TUI",
    long_about = "sft reads your SSH config, lets you choose or edit a Host entry, then opens a keyboard-driven local/remote file browser and copies files over OpenSSH sftp."
)]
struct Cli {
    /// SSH host, alias, or user@host target. Omit to choose from ssh config.
    host: Option<String>,

    /// SSH config path. Defaults to ~/.ssh/config when it exists.
    #[arg(short = 'F', long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Initial local directory.
    #[arg(short, long, value_name = "PATH", default_value = ".")]
    local: PathBuf,

    /// Initial remote directory. Defaults to the remote login directory.
    #[arg(short, long, value_name = "PATH")]
    remote: Option<String>,

    /// Show dotfiles on startup.
    #[arg(long)]
    hidden: bool,
}

fn main() -> Result<()> {
    if askpass::handle_askpass_if_requested()? {
        return Ok(());
    }

    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config)?;
    let local_path = expand_local_path(cli.local)?;
    let app = App::new(config_path, cli.host, local_path, cli.remote, cli.hidden)?;
    tui::run(app)
}

fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return expand_local_path(path);
    }
    ssh_config::default_config_path().context("could not determine default ssh config path")
}

fn expand_local_path(path: PathBuf) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    let expanded = if raw == "~" {
        dirs::home_dir().context("could not determine home directory")?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()
            .context("could not determine home directory")?
            .join(rest)
    } else {
        path
    };

    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(std::env::current_dir()?.join(expanded))
    }
}
