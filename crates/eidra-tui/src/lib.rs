//! eidra's interactive frontend (docs/DESIGN.md §12). It draws inline, like Claude Code: finished
//! output goes into the terminal's own scrollback, and a live region at the bottom holds the
//! streaming reply, the input box and a status line. It draws with crossterm alone, since
//! ratatui's inline viewport cannot change its height.

mod app;
mod editor;
mod markdown;
mod screen;
mod style;
mod wrap;

pub use app::{Options, run};
