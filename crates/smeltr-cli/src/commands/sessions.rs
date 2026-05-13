use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SessionsCmd {
    Ls,
    Show { id: String },
}

pub async fn run(_cmd: SessionsCmd) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented")
}
