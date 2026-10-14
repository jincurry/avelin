//! Publisher feed generation and explicit read-only update discovery.
use anyhow::{Context, Result, ensure};
use avelin_core::{reject_reparse, updates};
use std::path::Path;

pub fn publish(setup: &Path, setup_url: &str, channel: &str, output: &Path) -> Result<()> {
    let release = updates::release_for_setup(setup, setup_url, channel)?;
    let output = std::path::absolute(output)?;
    reject_reparse(&output)?;
    ensure!(
        output.parent().is_some_and(Path::is_dir),
        "feed output parent must exist"
    );
    let mut bytes = serde_json::to_vec_pretty(&release)?;
    bytes.push(b'\n');
    crate::build_output::write_new_file(&output, &bytes)?;
    println!(
        "{}",
        serde_json::json!({ "status": "created", "feed": output, "release": release })
    );
    Ok(())
}

/// `setup` is a local, publisher-supplied package whose embedded configuration
/// binds the feed and product. A URL from the command line cannot replace it.
pub fn check(setup: &Path, download: Option<&Path>) -> Result<()> {
    reject_reparse(setup)?;
    let (_, manifest) =
        avelin_core::embedded(setup)?.context("Setup has no embedded Avelin package")?;
    let config = manifest
        .updates
        .as_ref()
        .context("this package has no online update feed")?;
    let release = updates::check(config, &manifest.app_id, &manifest.app_version)?;
    match (release, download) {
        (Some(release), Some(destination)) => {
            let package = updates::download(&release, destination)?;
            println!(
                "{}",
                serde_json::json!({ "status": "downloaded", "setup": package.path, "release": package.release })
            );
        }
        (Some(release), None) => println!(
            "{}",
            serde_json::json!({ "status": "available", "release": release })
        ),
        (None, _) => println!("{}", serde_json::json!({ "status": "up_to_date" })),
    }
    Ok(())
}
