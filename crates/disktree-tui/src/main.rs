//! A terminal view over the disktree core.

mod app;
mod render;
mod theme;
#[cfg(windows)]
mod windows_console;

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use disktree_core::scan::ScanOptions;
use disktree_core::space::volume_root_for;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, canonicalize_path};
use crate::theme::{ThemeChoice, ThemeState};

// Keep stdout free for the reviewed agent prompt, even when it is redirected.
#[cfg(windows)]
const TTY_OUTPUT: &str = "CONOUT$";
#[cfg(not(windows))]
const TTY_OUTPUT: &str = "/dev/tty";

struct TerminalSession {
    tty: File,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        let mut tty = OpenOptions::new()
            .write(true)
            .open(TTY_OUTPUT)
            .with_context(|| format!("open terminal output {TTY_OUTPUT}"))?;
        enable_raw_mode().context("enable terminal raw mode")?;
        if let Err(error) = execute!(tty, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self { tty })
    }

    fn writer(&self) -> io::Result<File> {
        self.tty.try_clone()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.tty, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn main() -> Result<()> {
    let Some((root, options, theme_choice)) = arguments()? else {
        return Ok(());
    };
    if !io::stdin().is_terminal() {
        bail!("disktree-tui needs an interactive terminal for input");
    }
    #[cfg(windows)]
    let _code_pages = windows_console::Utf8Console::enter()
        .context("enable UTF-8 in the Windows console")?;
    let mut app = App::new(root, options);
    let mut theme = ThemeState::new(theme_choice);
    let stdout_prompt = {
        let session = TerminalSession::enter()?;
        // A full mosaic produces many small ANSI writes. Buffering them keeps
        // classic Windows consoles from visibly painting one row at a time.
        let output = BufWriter::with_capacity(256 * 1024, session.writer()?);
        let mut terminal = Terminal::new(CrosstermBackend::new(output))?;
        let size = terminal.size()?;
        app.set_viewport(size.width, size.height);
        // Compact maps begin with two levels; the depth keys can reveal more.
        if size.width < 120 {
            app.view_depth = 2;
        }
        let mut dirty = true;
        let mut drew_first_frame = false;
        // Fast scans can finish before the first frame. Give them a short
        // window so the console paints the mosaic only once at startup.
        let first_frame_by = Instant::now() + Duration::from_millis(180);
        while !app.quit {
            dirty |= app.tick();
            dirty |= theme.tick();
            if dirty
                && (drew_first_frame
                    || app.scan.is_none()
                    || Instant::now() >= first_frame_by)
            {
                terminal
                    .draw(|frame| render::draw(frame, &app, theme.palette()))?;
                dirty = false;
                drew_first_frame = true;
            }
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key);
                        dirty = true;
                    }
                    Event::Resize(width, height) => {
                        terminal.clear()?;
                        app.set_viewport(width, height);
                        dirty = true;
                    }
                    _ => {}
                }
            }
        }
        app.stdout_prompt.take()
    };
    if let Some(prompt) = stdout_prompt {
        print!("{prompt}");
        io::stdout().flush()?;
    }
    Ok(())
}

fn arguments() -> Result<Option<(PathBuf, ScanOptions, ThemeChoice)>> {
    let mut root = None;
    let mut disk = false;
    let mut options = ScanOptions::default();
    let mut theme = ThemeChoice::Auto;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--help" | "-h") => {
                const HELP: &str = concat!(
                    "disktree-tui [OPTIONS] [PATH]\n",
                    "\n",
                    "A terminal treemap explorer. Defaults to your home.\n",
                    "\n",
                    "Options:\n",
                    "  --disk                   Scan the whole volume\n",
                    "  --apparent-size          Measure file length\n",
                    "  --no-hidden              Exclude hidden entries\n",
                    "  --follow-links           Follow symbolic links\n",
                    "  --theme auto|dark|light  Colors (default: auto)\n",
                    "  -h, --help               Show this help\n",
                    "\n",
                    "In the TUI, press ? for keys. In review, a prints an\n",
                    "agent prompt to stdout after the terminal closes."
                );
                println!("{HELP}");
                return Ok(None);
            }
            Some("--disk") => disk = true,
            Some("--apparent-size") => options.apparent_size = true,
            Some("--no-hidden") => options.include_hidden = false,
            Some("--follow-links") => options.follow_links = true,
            Some("--theme") => {
                let value = args.next().context("--theme needs a value")?;
                theme = value
                    .to_str()
                    .and_then(ThemeChoice::parse)
                    .context("--theme must be auto, dark or light")?;
            }
            Some(value) if value.starts_with("--theme=") => {
                theme = ThemeChoice::parse(&value[8..])
                    .context("--theme must be auto, dark or light")?;
            }
            Some(value) if value.starts_with('-') => {
                bail!("unknown option: {value}");
            }
            _ if root.is_none() => root = Some(PathBuf::from(argument)),
            _ => bail!("expected only one PATH"),
        }
    }
    let root = match root {
        Some(path) => path,
        None => std::env::home_dir().context("cannot find home directory")?,
    };
    let root = canonicalize_path(&root)
        .with_context(|| format!("cannot access {}", root.display()))?;
    let root = if disk {
        volume_root_for(&root).context("cannot find volume root")?
    } else {
        root
    };
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    Ok(Some((root, options, theme)))
}
