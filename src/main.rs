use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use similar::Algorithm;

use yadt::app::Options;
use yadt::diff::{DEFAULT_TIMEOUT, DiffOptions, Whitespace};
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

    /// Bytes per row when comparing two binaries as hex. Defaults to fitting the terminal.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=64))]
    hex_width: Option<u32>,

    /// Collapse runs of identical lines, keeping this many for context.
    #[arg(short = 'C', long, value_name = "N")]
    context: Option<u32>,

    /// Follow symbolic links when comparing directories.
    #[arg(long)]
    follow_symlinks: bool,

    /// Treat whitespace-only differences as equal.
    #[arg(short = 'w', long, value_enum, default_value_t = WhitespaceArg::Exact)]
    ignore_whitespace: WhitespaceArg,

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

/// Mirrors `diff`: `-w` ignores all whitespace, `-b` ignores changes in it.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum WhitespaceArg {
    /// Compare whitespace exactly.
    Exact,
    /// Ignore changes in the amount of whitespace.
    Changes,
    /// Ignore whitespace entirely.
    All,
}

impl From<WhitespaceArg> for Whitespace {
    fn from(value: WhitespaceArg) -> Self {
        match value {
            WhitespaceArg::Exact => Self::Exact,
            WhitespaceArg::Changes => Self::IgnoreChanges,
            WhitespaceArg::All => Self::IgnoreAll,
        }
    }
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
            whitespace: cli.ignore_whitespace.into(),
            hex_bytes_per_row: cli.hex_width.unwrap_or_else(initial_hex_width),
        },
        tab_width: cli.tab_width,
        max_file_size: cli.max_file_size.saturating_mul(1024 * 1024),
        follow_symlinks: cli.follow_symlinks,
        context: cli.context,
        hex_width_pinned: cli.hex_width.is_some(),
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

/// Bytes per hex row for the terminal as it is right now.
///
/// Done before the alternate screen is entered and before the files are read, so a large
/// binary is aligned at the right width once rather than being re-aligned on the first
/// draw. Falls back to the default if the size cannot be determined — output may be piped.
fn initial_hex_width() -> u32 {
    crossterm::terminal::size().map_or(yadt::hex::DEFAULT_BYTES_PER_ROW, |(cols, _)| {
        // A generous gutter guess: being one step conservative costs less than being wrong
        // the other way, and small files get corrected at draw time anyway.
        yadt::hex::fit_bytes_per_row(yadt::hex::pane_row_width(cols, 8))
    })
}
