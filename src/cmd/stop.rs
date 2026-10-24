use crate::core::StateDir;
use crate::exit;
use anyhow::Result;
use std::path::Path;
use std::time::Duration;

pub fn run(root: Option<&Path>, all: bool, json: bool, verbose: u8) -> Result<u8> {
    super::init_cli_logging(verbose);
    let grace = Duration::from_secs(5);
    let targets: Vec<StateDir> = if all {
        StateDir::all()
    } else {
        vec![StateDir::for_root(&super::resolve_root(root)?)]
    };

    let mut results = Vec::new();
    for state in &targets {
        let was_running = crate::core::ensure::stop(state, grace)?;
        if all {
            // Nothing else refers to this dir once its daemon is gone.
            state.remove_all();
        }
        results.push((state.root.clone(), was_running));
    }

    let any_running = results.iter().any(|(_, r)| *r);
    if json {
        let v: Vec<_> = results
            .iter()
            .map(|(root, r)| serde_json::json!({ "root": root, "was_running": r }))
            .collect();
        crate::outln!("{}", serde_json::to_string(&v)?);
    } else {
        for (root, r) in &results {
            crate::outln!(
                "{}: {}",
                root.display(),
                if *r { "stopped" } else { "not running" }
            );
        }
        if all && results.is_empty() {
            crate::outln!("no selx daemons found");
        }
    }
    Ok(if any_running || all {
        exit::OK
    } else {
        exit::NOT_RUNNING
    })
}
