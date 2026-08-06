use clap::Parser;
use win_domain_flow::cli::Cli;

fn main() -> anyhow::Result<()> {
    win_domain_flow::cli::run(Cli::parse())
}
