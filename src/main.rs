//! Portable Agent Memory CLI entrypoint.

use clap::Parser;
use mem::cli::{run, Cli, Command};

fn main() {
    let mut cli = Cli::parse();
    // `mem status | head` should end quietly when the reader closes, like
    // any Unix tool. The server keeps ignoring SIGPIPE so a client that
    // disconnects mid-stream cannot stop it.
    #[cfg(unix)]
    if !matches!(cli.command, Command::Serve { .. } | Command::Shell) {
        // SAFETY: restores the default disposition before any threads start.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    if let Err(err) = cli.resolve_store().and_then(|()| run(&cli)) {
        match err {
            mem::Error::NoStore => {
                eprintln!("fatal: not a mem store (or any of the parent directories): .mem");
                eprintln!("hint: run `mem init` for a store in this project, or `mem init --global` for ~/.mem");
            }
            err => eprintln!("error: {err}"),
        }
        std::process::exit(1);
    }
}
