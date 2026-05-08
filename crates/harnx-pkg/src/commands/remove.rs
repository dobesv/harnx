use crate::cli::RemoveArgs;
use crate::install::remove_package;
use anyhow::Result;

pub async fn run(args: &RemoveArgs) -> Result<()> {
    remove_package(&args.name)?;
    Ok(())
}
