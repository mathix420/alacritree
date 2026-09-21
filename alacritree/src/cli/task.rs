//! `alacritree task`: the project an agent's shell writes to, and the
//! one-time UDA declarations agents calling `task` directly depend on.

use std::path::Path;

use clap::Subcommand;

use crate::multiplexer::Side;
use crate::tasks::facts;
use crate::tasks::scope::{node, session_from_env};
use crate::tasks::taskwarrior::{TaskError, Taskwarrior, UDA_DECLARATIONS};
use crate::{config, jobs, tools, wsl};

#[derive(Debug, Subcommand)]
pub(super) enum TaskCommand {
    /// Print the taskwarrior project for this directory and agent session.
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
    let (config, _) = config::load(config_dir, overrides);
    tools::configure(config.integrations.tool_paths());
    match command {
        TaskCommand::Scope => scope(json),
        TaskCommand::Setup => setup(json),
    }
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
    jobs::on_this_thread(|b| {
        let tw = Taskwarrior::for_side(side, b);
        let mut written = Vec::new();
        for (key, value) in UDA_DECLARATIONS {
            if tw.rc_value(key, b)?.as_deref() != Some(value) {
                tw.set_config(key, value, b)?;
                written.push(key);
            }
        }
        Ok(written)
    })
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
