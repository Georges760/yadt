//! yadt — Yet Another Diff Tool.
//!
//! A terminal diff viewer: compare two files side by side, or two directories with a
//! status tree that drills into the file view. Read-only — yadt never writes to the files
//! it is comparing.

pub mod app;
pub mod canvas;
pub mod diff;
pub mod dirdiff;
pub mod edit;
pub mod hex;
pub mod key;
pub mod model;
pub mod pane;
pub mod search;
pub mod style;
#[cfg(test)]
mod testing;
pub mod text;
pub mod theme;
pub mod ui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub use app::App;
pub use model::{Alignment, Cell, LineNo, RowKind, SideIdx};
pub use theme::Theme;

/// Exit codes follow `diff(1)`, so `yadt --quiet a b` is scriptable.
pub mod exit {
    pub const IDENTICAL: i32 = 0;
    pub const DIFFERENT: i32 = 1;
    pub const ERROR: i32 = 2;
}

/// What the two command-line paths turned out to be.
#[derive(Clone, Debug)]
pub enum Inputs {
    Files { left: PathBuf, right: PathBuf },
    Dirs { left: PathBuf, right: PathBuf },
}

/// Resolve two paths into a comparison mode.
///
/// Runs before the alternate screen is entered, so its errors are visible on a normal
/// terminal rather than being swallowed by the TUI.
pub fn resolve_inputs(left: &Path, right: &Path) -> Result<Inputs> {
    let lm = std::fs::metadata(left).with_context(|| format!("cannot read {}", left.display()))?;
    let rm =
        std::fs::metadata(right).with_context(|| format!("cannot read {}", right.display()))?;

    match (lm.is_dir(), rm.is_dir()) {
        (false, false) => Ok(Inputs::Files {
            left: left.to_path_buf(),
            right: right.to_path_buf(),
        }),
        (true, true) => Ok(Inputs::Dirs {
            left: left.to_path_buf(),
            right: right.to_path_buf(),
        }),
        (true, false) => bail!(
            "{} is a directory but {} is a file",
            left.display(),
            right.display()
        ),
        (false, true) => bail!(
            "{} is a file but {} is a directory",
            left.display(),
            right.display()
        ),
    }
}
