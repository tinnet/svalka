use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use onlycurves::{check, identify, restore, run, signals, status};
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

    /// Config file
    #[arg(long, global = true, default_value = "/etc/onlycurves.toml")]
    config: PathBuf,

    /// Where `run` writes state.json; must be on tmpfs. Only tests change it.
    #[arg(long, global = true, hide = true, default_value = "/run/onlycurves")]
    state_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print every temperature, fan speed and PWM duty (read-only)
    Status(StatusArgs),
    /// Validate the config against this PC and show each fan's duty right now (read-only)
    Check,
    /// Run one fan at 100 % for a few seconds, then restore it (needs root)
    Identify(IdentifyArgs),
    /// Control the fans until stopped, then hand them back to the chip (needs root)
    Run,
    /// Hand every configured fan back to the chip's own curve (needs root)
    Restore,
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

#[derive(Args)]
struct IdentifyArgs {
    /// Fan name from the config, e.g. exhaust
    fan: String,

    /// How long to run the fan at full speed
    #[arg(long, default_value_t = 5)]
    seconds: u64,
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
        Command::Check => print!("{}", check::run(&cli.config, &cli.hwmon_root)?),
        Command::Identify(args) => {
            let options = identify::Options {
                fan: &args.fan,
                seconds: args.seconds,
            };
            let stop = signals::stop_flag()?;
            identify::run(
                &cli.config,
                &cli.hwmon_root,
                &options,
                &stop,
                &mut std::io::stdout(),
            )?;
        }
        Command::Run => {
            let stop = signals::stop_flag()?;
            run::run(&cli.config, &cli.hwmon_root, &cli.state_dir, &stop)?;
        }
        Command::Restore => restore::run(&cli.config, &cli.hwmon_root, &mut std::io::stdout())?,
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
