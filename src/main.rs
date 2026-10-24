//! The `selx` binary: argument parsing, and nothing else. Everything it
//! does lives in the `selx` library beside it.

#![forbid(unsafe_code)]

use clap::{Args, Parser, Subcommand};
use selx::{cmd, exit};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(name = "selx", version, about, long_about = None)]
struct Cli {
    /// More logging on stderr (-v: info, -vv: debug). Selected text is only
    /// ever logged at -vv.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, Clone)]
pub struct RootArg {
    /// Workspace root. Default: the git worktree root of the current
    /// directory, or the current directory itself outside a repository.
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the daemon for this workspace if it is not already healthy; print its port.
    Ensure {
        #[command(flatten)]
        root: RootArg,
        /// Print a JSON object instead of the bare port.
        #[arg(long)]
        json: bool,
    },
    /// Send a selection to the workspace's agents (starts the daemon if needed).
    Send {
        /// File path (relative paths are resolved against the current directory).
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
        /// First line, 1-based. Omit --start/--end to mean the whole file.
        #[arg(long, value_name = "N")]
        start: Option<u32>,
        /// Last line, 1-based, inclusive. Defaults to --start.
        #[arg(long, value_name = "N", requires = "start")]
        end: Option<u32>,
        #[command(flatten)]
        root: RootArg,
        /// Mark the selection as a deliberate mention rather than ambient context.
        #[arg(long)]
        explicit: bool,
        /// Name of the sending frontend (informational).
        #[arg(long, default_value = "cli", value_name = "NAME")]
        source: String,
        /// Read the selected text from stdin and attach it to the event.
        #[arg(long, requires = "start")]
        text_stdin: bool,
        /// Print the routing outcome as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Put a sentence in the agent's prompt, for a selection that is not
    /// lines of a file (a rectangle of a map, say). Always explicit. Typed
    /// into the agent's pane, so it needs herdr; kept to one line.
    Say {
        /// The sentence. Ends with a space if the user's words are to
        /// follow it.
        #[arg(value_name = "TEXT")]
        text: String,
        #[command(flatten)]
        root: RootArg,
        /// Name of the sending frontend (informational).
        #[arg(long, default_value = "cli", value_name = "NAME")]
        source: String,
        /// Print the routing outcome as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run Claude Code connected to this workspace's daemon. Extra arguments
    /// are passed to `claude` (e.g. `selx claude --resume`).
    Claude {
        #[command(flatten)]
        root: RootArg,
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "CLAUDE_ARGS"
        )]
        args: Vec<std::ffi::OsString>,
    },
    /// Open files in the editor already running in this herdr workspace,
    /// instead of starting another one over the caller (e.g. lazygit's 'e').
    Open {
        /// Editor binary: the pane to look for, and what to run when there
        /// is no such pane. Only editors selx can type into are delivered
        /// to; any other is always run.
        #[arg(long, value_name = "BIN", default_value = cmd::open::DEFAULT_EDITOR)]
        editor: String,
        /// Files, as the editor itself takes them: FILE, FILE:LINE,
        /// FILE:LINE:COL, or +LINE FILE.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "FILE"
        )]
        files: Vec<std::ffi::OsString>,
    },
    /// Make the editor running in this herdr workspace re-read its files
    /// from disk — unless a buffer has unsaved changes or you are typing
    /// in it. `selx claude` runs this after every edit Claude makes.
    Reload {
        /// Editor binary: which pane to look for.
        #[arg(long, value_name = "BIN", default_value = cmd::open::DEFAULT_EDITOR)]
        editor: String,
        #[command(flatten)]
        root: RootArg,
        /// Run as a Claude Code hook: read the hook's JSON on stdin, act
        /// only on file edits and turn ends, and never fail.
        #[arg(long)]
        claude_hook: bool,
    },
    /// Show daemon health, connected clients, adapters and recent events.
    Status {
        #[command(flatten)]
        root: RootArg,
        #[arg(long)]
        json: bool,
        /// Keep running and print events as they arrive.
        #[arg(long, short)]
        follow: bool,
    },
    /// Stop the daemon for this workspace (or every daemon with --all).
    Stop {
        #[command(flatten)]
        root: RootArg,
        /// Stop every selx daemon owned by this user.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Internal: run the daemon process. Normally spawned by `ensure`.
    Daemon {
        #[arg(long, value_name = "PATH")]
        root: PathBuf,
        /// Preferred port; the daemon probes upward if it is taken.
        #[arg(long, value_name = "N")]
        port: Option<u16>,
        /// Run attached to the current terminal with human-readable logs.
        #[arg(long)]
        foreground: bool,
        /// Exit after this many seconds without clients or traffic
        /// (0 disables). Default: $SELX_IDLE_TIMEOUT or 4 hours.
        #[arg(long, value_name = "SECS")]
        idle_timeout: Option<u64>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Ensure { root, json } => cmd::ensure::run(root.root.as_deref(), json, cli.verbose),
        Command::Send {
            file,
            start,
            end,
            root,
            explicit,
            source,
            text_stdin,
            json,
        } => cmd::send::run(
            cmd::send::SendArgs {
                file: Some(file),
                lines: start.map(|s| selx::core::LineRange {
                    start: s,
                    end: end.unwrap_or(s),
                }),
                root: root.root,
                explicit,
                source,
                text_stdin,
                message: None,
                json,
            },
            cli.verbose,
        ),
        Command::Say {
            text,
            root,
            source,
            json,
        } => cmd::send::run(
            cmd::send::SendArgs {
                file: None,
                lines: None,
                root: root.root,
                explicit: true,
                source,
                text_stdin: false,
                message: Some(text),
                json,
            },
            cli.verbose,
        ),
        Command::Claude { root, args } => cmd::claude::run(root.root.as_deref(), args, cli.verbose),
        Command::Open { editor, files } => {
            cmd::open::run(cmd::open::OpenArgs { editor, files }, cli.verbose)
        }
        Command::Reload {
            editor,
            root,
            claude_hook,
        } => cmd::reload::run(
            cmd::reload::ReloadArgs {
                editor,
                root: root.root,
                claude_hook,
            },
            cli.verbose,
        ),
        Command::Status { root, json, follow } => {
            cmd::status::run(root.root.as_deref(), json, follow, cli.verbose)
        }
        Command::Stop { root, all, json } => {
            cmd::stop::run(root.root.as_deref(), all, json, cli.verbose)
        }
        Command::Daemon {
            root,
            port,
            foreground,
            idle_timeout,
        } => cmd::daemon::run(root, port, foreground, idle_timeout, cli.verbose),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("selx: {e:#}");
            ExitCode::from(exit::ERROR)
        }
    }
}
