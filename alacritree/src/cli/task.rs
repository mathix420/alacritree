//! `alacritree task`: the project an agent's shell writes to, and the
//! one-time UDA declarations agents calling `task` directly depend on.

use std::path::Path;

use alacritree_common::side::Side;
use alacritree_common::{jobs, tools, wsl};
use alacritree_tasks::TaskError;
use alacritree_tasks::scope::{node, session_from_env};
use alacritree_taskwarrior::Taskwarrior;
use clap::Subcommand;

use crate::config;
use crate::tasks::facts;

#[derive(Debug, Subcommand)]
pub(super) enum TaskCommand {
    /// Print the project an agent in this directory writes its tasks to.
    Scope,
    /// Declare the `subof` and `order` UDAs in the taskrc on every side.
    Setup,
}

pub(super) fn run(
    command: TaskCommand,
    json: bool,
    config_dir: Option<&Path>,
    overrides: &[toml::Value],
) -> i32 {
    configure_tools(config_dir, overrides);
    match command {
        TaskCommand::Scope => scope(json),
        TaskCommand::Setup => setup(json),
    }
}

/// Points `task` and `git` at the configured programs, so
/// `[integrations.taskwarrior] path` and `-o` reach every call, and hands back
/// the integrations for the caller to pick a task backend from.
pub(super) fn configure_tools(
    config_dir: Option<&Path>,
    overrides: &[toml::Value],
) -> config::IntegrationsConfig {
    let (config, _) = config::load(config_dir, overrides);
    tools::configure(config.integrations.tool_paths());
    config.integrations
}

fn scope(json: bool) -> i32 {
    let Ok(cwd) = std::env::current_dir() else {
        eprintln!("alacritree: the current directory is unreadable");
        return 1;
    };
    let (_, place) = jobs::on_this_thread(|b| facts::place_for(&cwd, b));
    let project = node(&place, session_from_env(|key| std::env::var(key).ok()).as_ref());
    if json {
        println!("{}", serde_json::json!({ "project": project }));
    } else {
        println!("{project}");
    }
    0
}

fn declare(side: Side) -> Result<Vec<&'static str>, TaskError> {
    jobs::on_this_thread(|b| Taskwarrior::default().declare_fields(side, b))
}

fn setup(json: bool) -> i32 {
    let mut sides = vec![Side::Native];
    sides.extend(wsl::distros().into_iter().map(|d| Side::Wsl(d.name)));
    let mut failed = false;
    for side in sides {
        let name = side.name();
        let result = declare(side);
        failed |= result.is_err();
        match (json, result) {
            (true, result) => {
                let (written, error) = match result {
                    Ok(written) => (written, None),
                    Err(e) => (Vec::new(), Some(e.to_string())),
                };
                println!(
                    "{}",
                    serde_json::json!({ "side": name, "written": written, "error": error })
                );
            },
            (false, Ok(written)) if written.is_empty() => println!("{name}: already declared"),
            (false, Ok(written)) => println!("{name}: declared {}", written.join(", ")),
            (false, Err(e)) => eprintln!("{name}: {e}"),
        }
    }
    i32::from(failed)
}
