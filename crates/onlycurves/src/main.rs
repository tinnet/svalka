use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use onlycurves::status;
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
    Status(StatusArgs),
}

#[derive(Args)]
struct StatusArgs {
    /// Only show the chip with this driver name, e.g. nct6798
    #[arg(long)]
    chip: Option<String>,

    /// Device path that tells chips with the same name apart, e.g. nct6775.656
    #[arg(long, requires = "chip")]
    path: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Status(args) => {
            let filter = status::ChipFilter {
                name: args.chip.as_deref(),
                path: args.path.as_deref(),
            };
            print!("{}", status::run(&cli.hwmon_root, &filter)?);
        }
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
