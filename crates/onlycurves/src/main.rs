mod hwmon;
mod sysfs;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use shadow_rs::shadow;

shadow!(build);

#[derive(Parser)]
#[command(name = "onlycurves")]
#[command(about = "Fan control for one PC: temperatures in, PWM out", long_about = None)]
#[command(version = build::CLAP_LONG_VERSION)]
struct Cli {
    /// Where hwmon chips are listed. Only tests point this elsewhere.
    #[arg(long, global = true, hide = true, default_value = "/sys/class/hwmon")]
    hwmon_root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print every temperature, fan speed and PWM duty (read-only)
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Status => println!("status: not implemented yet"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
