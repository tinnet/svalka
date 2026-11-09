use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use shadow_rs::shadow;
use std::time::Duration;

use svalka::{check, history, identify, notify, restore, run, signals, status, sweep};

shadow!(build);

#[derive(Parser)]
#[command(name = "svalka")]
#[command(about = "Fan control for Linux: temperatures in, PWM out", long_about = None)]
#[command(version = build::CLAP_LONG_VERSION)]
struct Cli {
    /// Where hwmon chips are listed. Only tests point this elsewhere.
    #[arg(long, global = true, hide = true, default_value = "/sys/class/hwmon")]
    hwmon_root: PathBuf,

    /// Config file
    #[arg(long, global = true, default_value = "/etc/svalka.toml")]
    config: PathBuf,

    /// Where `run` writes state.json and the history; must be on tmpfs. Only
    /// tests change it.
    #[arg(long, global = true, hide = true, default_value = "/run/svalka")]
    state_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print every temperature, fan speed and PWM duty (read-only)
    Status(StatusArgs),
    /// Validate the config against the hardware and show each fan's duty right now (read-only)
    Check,
    /// Show the last 15–30 minutes of each fan's temperature, duty and rpm (read-only)
    History(HistoryArgs),
    /// Run one fan at 100 % for a few seconds, then restore it; borrows it from `svalka run` if needed (needs root)
    Identify(IdentifyArgs),
    /// Step one fan from 100 % to 0 and back, to find where it stops and starts; borrows it from `svalka run` if needed (needs root)
    Sweep(SweepArgs),
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
struct HistoryArgs {
    /// Only this fan, e.g. exhaust
    fan: Option<String>,

    /// Only the last part, e.g. 30s, 5m or 1h
    #[arg(long, value_parser = history::parse_since)]
    since: Option<Duration>,

    /// Print the ticks as JSON lines, one per tick
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct IdentifyArgs {
    /// Fan name from the config, e.g. exhaust
    fan: String,

    /// How long to run the fan at full speed
    #[arg(long, default_value_t = 5)]
    seconds: u64,
}

#[derive(Args)]
struct SweepArgs {
    /// Fan name from the config, e.g. exhaust
    fan: String,

    /// Percent between steps
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..=50))]
    step: u8,

    /// Milliseconds between tachometer readings. Only tests change it.
    #[arg(long, hide = true, default_value_t = 2000)]
    sample_ms: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run_command(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            if needs_root(&error) {
                eprintln!("\nWriting to fans needs root; run this with sudo.");
            }
            ExitCode::FAILURE
        }
    }
}

/// Whether any cause of `error` is a permission error: the one place that
/// explains it, for every command.
fn needs_root(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

fn run_command(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Status(args) => {
            let filter = status::ChipFilter {
                name: args.chip.as_deref(),
                path: args.path.as_deref(),
            };
            print!("{}", status::run(&cli.hwmon_root, &filter)?);
        }
        Command::Check => print!("{}", check::run(&cli.config, &cli.hwmon_root)?),
        Command::History(args) => {
            let options = history::Options {
                fan: args.fan.as_deref(),
                since: args.since,
                json: args.json,
            };
            history::run(
                &cli.state_dir,
                &options,
                run::unix_millis(),
                &mut std::io::stdout(),
            )?;
        }
        Command::Identify(args) => {
            let options = identify::Options {
                fan: &args.fan,
                seconds: args.seconds,
            };
            let paths = run::Paths {
                config: &cli.config,
                hwmon_root: &cli.hwmon_root,
                state_dir: &cli.state_dir,
            };
            let stop = signals::stop_flag()?;
            identify::run(&paths, &options, &stop, &mut std::io::stdout())?;
        }
        Command::Sweep(args) => {
            let paths = run::Paths {
                config: &cli.config,
                hwmon_root: &cli.hwmon_root,
                state_dir: &cli.state_dir,
            };
            let options = sweep::Options {
                fan: &args.fan,
                step: args.step,
                sample: Duration::from_millis(args.sample_ms),
            };
            let stop = signals::stop_flag()?;
            sweep::run(&paths, &options, &stop, &mut std::io::stdout())?;
        }
        Command::Run => {
            let paths = run::Paths {
                config: &cli.config,
                hwmon_root: &cli.hwmon_root,
                state_dir: &cli.state_dir,
            };
            let signals = signals::DaemonSignals::register()?;
            let notifier = notify::Notifier::from_env()?;
            run::run(&paths, &signals, &notifier)?;
        }
        Command::Restore => {
            restore::run(
                &cli.config,
                &cli.hwmon_root,
                &cli.state_dir,
                &mut std::io::stdout(),
            )?;
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
