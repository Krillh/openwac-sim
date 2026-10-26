mod compile;
mod computer;
mod console;
mod controller;
mod device;
mod guest;
mod hal;
mod hal_functions;
mod kernel;
mod memory;
#[cfg(test)]
mod mod_checks;
mod simulation;
mod syscall;
mod validation;

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use controller::Controller;
use simulation::{SimulationConfig, SimulationConfigBuilder, SimulationHandle, TickPacer};

#[derive(Parser)]
#[command(version, about = "A tick-scheduled WebAssembly computer simulation")]
struct Cli {
    #[arg(long, value_enum, default_value_t = LogLevel::Info, global = true)]
    log_level: LogLevel,
    /// Run logical ticks without waiting for wall-clock pacing.
    #[arg(long, global = true)]
    unpaced: bool,
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(value_name = "CONFIG")]
    config: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for log::LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Off => Self::Off,
            LogLevel::Error => Self::Error,
            LogLevel::Warn => Self::Warn,
            LogLevel::Info => Self::Info,
            LogLevel::Debug => Self::Debug,
            LogLevel::Trace => Self::Trace,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    Run {
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    Build {
        #[arg(short, long, default_value = "simulation.toml")]
        output: PathBuf,
        #[arg(long, default_value = "bootloader.wat")]
        bootloader: PathBuf,
        #[arg(long, default_value_t = 64)]
        max_ticks: u64,
        #[arg(long, default_value_t = 1)]
        computers: usize,
        #[arg(long, default_value_t = 1)]
        disks_per_computer: usize,
        #[arg(long)]
        remove_disks_on_drop: bool,
    },
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    env_logger::Builder::new()
        .filter_level(cli.log_level.into())
        .init();

    match cli.command {
        Some(Commands::Config { action }) => match action {
            ConfigCommand::Build {
                output,
                bootloader,
                max_ticks,
                computers,
                disks_per_computer,
                remove_disks_on_drop,
            } => build_config(
                output,
                bootloader,
                max_ticks,
                computers,
                disks_per_computer,
                remove_disks_on_drop,
            ),
        },
        Some(Commands::Run { config }) => run_simulation(&config, cli.unpaced),
        None => {
            let config = cli.config.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "provide a config path or use `openwc config build`",
                )
            })?;
            run_simulation(&config, cli.unpaced)
        }
    }
}

fn build_config(
    output: PathBuf,
    bootloader: PathBuf,
    max_ticks: u64,
    computers: usize,
    disks_per_computer: usize,
    remove_disks_on_drop: bool,
) -> Result<(), Box<dyn Error>> {
    if computers == 0 {
        return Err("the config must contain at least one computer".into());
    }
    let config = SimulationConfigBuilder::new(
        max_ticks,
        bootloader,
        computers,
        disks_per_computer,
        remove_disks_on_drop,
    )
    .build();
    let output = config_file_path(&output)?;
    let contents = toml::to_string_pretty(&config)?;
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)?;
    file.write_all(contents.as_bytes())?;
    log::debug!("created simulation config at {}", output.display());
    Ok(())
}

fn run_simulation(config_path: &std::path::Path, unpaced: bool) -> Result<(), Box<dyn Error>> {
    let config_path = config_file_path(config_path)?;
    let config = SimulationConfig::load(&config_path)?;
    let max_ticks = config.max_ticks;
    let tick_interval = Duration::from_millis(config.tick_interval_ms);
    let (controller, commands) = Controller::start(config)?;
    let simulation = SimulationHandle::new(commands);
    let console_simulation = simulation.clone();
    thread::Builder::new()
        .name("sim_console".into())
        .spawn(move || console::run_console(console_simulation))?;

    let mut completed = false;
    let mut first_failure = None;
    let mut tick_count = 0_u64;
    let mut total_tick_time = Duration::ZERO;
    let mut pacer = TickPacer::new(tick_interval);
    for _ in 0..max_ticks {
        let tick_started = Instant::now();
        let report = simulation.synchronize()?;
        let tick_time = tick_started.elapsed();
        tick_count += 1;
        total_tick_time += tick_time;
        let finished_at = Instant::now();
        let wait = pacer.advance_at(finished_at);
        let behind = pacer.offset_ms_at(finished_at);
        if let Some(message) = tick_schedule_message(report.tick, tick_time, tick_interval, behind)
        {
            log::warn!("{message}");
        }
        log::info!("tick {} completed in {:?}", report.tick, tick_time);
        log::debug!("tick {}:", report.tick);
        for event in report.events {
            if event.description.starts_with("trapped:")
                || event.description.starts_with("computer crashed:")
                || event.description.starts_with("kernel panic:")
                || event.description.starts_with("spawn failed:")
            {
                let failure = format!(
                    "computer {} process {}: {}",
                    event.computer_id, event.process_id, event.description
                );
                log::error!("{failure}");
                first_failure.get_or_insert(failure);
            }
            log::debug!(
                "  computer {} process {}: {}",
                event.computer_id,
                event.process_id,
                event.description
            );
        }
        for (computer_id, operations) in report.hardware_operations {
            log::debug!("  computer {computer_id} hardware operations: {operations}");
        }
        if report.all_finished {
            completed = true;
            break;
        }
        if let Some(wait) = wait.filter(|_| !unpaced) {
            thread::sleep(wait.saturating_sub(finished_at.elapsed()));
        }
    }

    simulation.shutdown()?;
    controller
        .join()
        .map_err(|_| std::io::Error::other("sim_controller thread panicked"))?;
    if !completed {
        log::info!("simulation reached its tick limit before all processes exited");
    }
    if tick_count > 0 {
        let average_tick_time_ms = total_tick_time.as_secs_f64() * 1000.0 / tick_count as f64;
        println!("average tick time: {average_tick_time_ms:.4} ms across {tick_count} ticks");
    } else {
        log::info!("no ticks were run; average tick time is unavailable");
    }
    if let Some(failure) = first_failure {
        return Err(std::io::Error::other(failure).into());
    }
    Ok(())
}

/// Returns a warning line for a tick that overran its interval or that ended
/// with the simulation behind schedule, otherwise `None`. Columns: the tick,
/// how long it took, the difference from the interval, and the simulation's
/// signed offset from its schedule (`sim_offset_ms`; positive is behind, and
/// it includes lag carried over from earlier ticks).
fn tick_schedule_message(
    tick: u64,
    tick_time: Duration,
    interval: Duration,
    sim_offset_ms: f64,
) -> Option<String> {
    if tick_time <= interval && sim_offset_ms <= 0.0 {
        return None;
    }
    let took = format!("{:.4}ms", tick_time.as_secs_f64() * 1000.0);
    let versus_interval = format!(
        "{:+.4}ms",
        (tick_time.as_secs_f64() - interval.as_secs_f64()) * 1000.0
    );
    Some(format!(
        "tick {tick:>5}: {took:>9} ({versus_interval:>10}) -- sim: {sim_offset_ms:+.4}ms"
    ))
}

fn config_file_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::{config_file_path, tick_schedule_message};

    #[test]
    fn overrunning_tick_is_reported_in_fixed_width_columns() {
        let interval = Duration::from_millis(10);
        assert_eq!(
            tick_schedule_message(1, Duration::from_nanos(46_979_500), interval, 36.9812)
                .as_deref(),
            Some("tick     1: 46.9795ms (+36.9795ms) -- sim: +36.9812ms")
        );
    }

    #[test]
    fn fast_tick_that_is_still_behind_is_reported() {
        let interval = Duration::from_millis(10);
        assert_eq!(
            tick_schedule_message(2, Duration::from_nanos(1_607_600), interval, 28.9731).as_deref(),
            Some("tick     2:  1.6076ms ( -8.3924ms) -- sim: +28.9731ms")
        );
    }

    #[test]
    fn overrunning_tick_on_or_ahead_of_schedule_shows_a_negative_offset() {
        let interval = Duration::from_millis(10);
        assert_eq!(
            tick_schedule_message(7, Duration::from_millis(12), interval, -3.5).as_deref(),
            Some("tick     7: 12.0000ms ( +2.0000ms) -- sim: -3.5000ms")
        );
    }

    #[test]
    fn ticks_within_the_interval_and_on_schedule_are_not_reported() {
        let interval = Duration::from_millis(10);
        assert_eq!(
            tick_schedule_message(1, Duration::from_millis(10), interval, 0.0),
            None
        );
        assert_eq!(
            tick_schedule_message(2, Duration::from_millis(3), interval, -6.0),
            None
        );
    }

    #[test]
    fn config_filename_is_relative_to_current_directory() {
        assert_eq!(
            config_file_path(Path::new("examples/basic/config.example.toml")).unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("examples/basic/config.example.toml")
        );
        assert_eq!(
            config_file_path(Path::new("examples/custom/nested/local.toml")).unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("examples/custom/nested/local.toml")
        );
    }
}
