#![cfg(feature = "engine-cli")]

mod common;

use common::*;

#[test]
fn engine_complete_help_lists_buffer() -> Result<()> {
    cli()
        .args(["engine", "complete", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--buffer"))
        .stdout(predicate::str::contains("--compare").not());
    Ok(())
}

#[test]
fn engine_complete_requires_buffer() -> Result<()> {
    cli()
        .args(["engine", "complete"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--buffer"));
    Ok(())
}
