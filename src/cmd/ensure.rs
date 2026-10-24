use crate::exit;
use anyhow::Result;
use std::path::Path;

pub fn run(root: Option<&Path>, json: bool, verbose: u8) -> Result<u8> {
    super::init_cli_logging(verbose);
    let root = super::resolve_root(root)?;
    let out = super::ensure_root(&root)?;
    if json {
        crate::outln!("{}", serde_json::to_string(&out)?);
    } else {
        crate::outln!("{}", out.port);
    }
    Ok(exit::OK)
}
