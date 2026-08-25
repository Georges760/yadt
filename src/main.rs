use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use similar::Algorithm;

use yadt::app::Options;
use yadt::diff::{DEFAULT_TIMEOUT, DiffOptions};
use yadt::text::DEFAULT_MAX_FILE_SIZE;
use yadt::{App, Theme, app, exit, resolve_inputs};

#[derive(Debug, Parser)]
#[command(
    name = "yadt",
    about = "Yet Another Diff Tool \u{2014} side-by-side diff in your terminal",
    version
)]
struct Cli {
    /// Left-hand file or directory.
    left: PathBuf,

    /// Right-hand file or directory.
    right: PathBuf,

    /// Diff algorithm to use.
    #[arg(long, value_enum, default_value_t = AlgorithmArg::Histogram)]
    algorithm: AlgorithmArg,

    /// Columns per tab stop.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=16))]
    tab_width: u8,

    /// Refuse to load files larger than this many MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_FILE_SIZE / (1024 * 1024))]
    max_file_size: u64,

    /// Collapse runs of identical lines, keeping this many for context.
    #[arg(short = 'C', long, value_name = "N")]
    context: Option<u32>,

    /// Follow symbolic links when comparing directories.
    #[arg(long)]
    follow_symlinks: bool,

    /// Skip word-level highlighting inside changed lines.
    #[arg(long)]
    no_inline: bool,

    /// Report differences via the exit code only; do not start the TUI.
    #[arg(short, long)]
    quiet: bool,

    /// Disable colour output.
    #[arg(long)]
    no_color: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AlgorithmArg {
    Histogram,
    Myers,
    Patience,
    Lcs,
}

impl From<AlgorithmArg> for Algorithm {
    fn from(value: AlgorithmArg) -> Self {
        match value {
            AlgorithmArg::Histogram => Self::Histogram,
            AlgorithmArg::Myers => Self::Myers,
            AlgorithmArg::Patience => Self::Patience,
            AlgorithmArg::Lcs => Self::Lcs,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("yadt: {err:#}");
            ExitCode::from(exit::ERROR as u8)
        }
    }
}

fn run() -> Result<i32> {
    let cli = Cli::parse();

    let opts = Options {
        diff: DiffOptions {
            algorithm: cli.algorithm.into(),
            timeout: DEFAULT_TIMEOUT,
            inline: !cli.no_inline,
        },
        tab_width: cli.tab_width,
        max_file_size: cli.max_file_size.saturating_mul(1024 * 1024),
        follow_symlinks: cli.follow_symlinks,
        context: cli.context,
    };

    let theme = if cli.no_color || std::env::var_os("NO_COLOR").is_some() {
        Theme::no_color()
    } else {
        Theme::dark()
    };

    // Everything fallible happens before the alternate screen exists, so errors print to a
    // normal terminal instead of being swallowed by the TUI.
    let inputs = resolve_inputs(&cli.left, &cli.right).with_context(|| {
        format!(
            "comparing {} and {}",
            cli.left.display(),
            cli.right.display()
        )
    })?;
    let mut app = App::new(inputs, opts, theme)?;

    if cli.quiet {
        return Ok(app.exit_code_blocking());
    }

    // `run` restores the terminal on every path out: normal return, the `?` below, and a
    // panic (via the hook it installs).
    ratatui::run(|terminal| app::run_app(terminal, &mut app))?;

    Ok(app.exit_code())
}
