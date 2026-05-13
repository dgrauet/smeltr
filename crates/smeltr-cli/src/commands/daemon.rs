use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    Start,
    Stop,
    Status,
}

pub async fn run(_cmd: DaemonCmd) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented")
}
