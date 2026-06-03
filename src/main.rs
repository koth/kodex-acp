use anyhow::Result;
use clap::Parser;
use codex_arg0::arg0_dispatch_or_else;
use codex_utils_cli::CliConfigOverrides;

#[derive(Parser)]
struct AcpCli {
    #[arg(long)]
    port: Option<u16>,
    #[command(flatten)]
    config_overrides: CliConfigOverrides,
}

fn main() -> Result<()> {
    arg0_dispatch_or_else(|args| async move {
        let cli = AcpCli::parse();
        codex_acp::run_main(args.codex_linux_sandbox_exe, cli.config_overrides, cli.port).await?;
        Ok(())
    })
}
