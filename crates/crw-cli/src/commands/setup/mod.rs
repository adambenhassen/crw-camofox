//! Interactive setup wizard for CRW.
//!
//! Guides users through Cloud or Local installation with clear
//! explanations at each step.

mod browser;
mod cloud;
mod config_file;
mod docker;
mod llm;
mod local;
mod searxng;
mod shell;
pub mod ui;
mod wizard;

use clap::Args;

/// Setup command arguments.
#[derive(Args)]
pub struct SetupArgs {
    /// Skip interactive prompts and use defaults (for scripting).
    #[arg(long)]
    pub non_interactive: bool,

    /// Force cloud setup mode.
    #[arg(long, conflicts_with = "local")]
    pub cloud: bool,

    /// Force local setup mode.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,

    /// Disable colored output.
    #[arg(long)]
    pub no_color: bool,

    /// Strip every `# CRW Configuration` block from the shell rc file and exit.
    ///
    /// Run this once after upgrading to the config.toml-first setup if your
    /// `.zshrc` / `.bashrc` accumulated duplicate `export CRW_*` lines from
    /// earlier setup runs. Won't touch `~/.config/crw/config.toml`.
    #[arg(long, conflicts_with_all = ["cloud", "local"])]
    pub reset_shell: bool,
}

/// Run the setup command.
pub async fn run(args: SetupArgs) {
    // Initialize color settings
    ui::init_color(args.no_color);

    // Short-circuit: --reset-shell does one job and exits.
    if args.reset_shell {
        let res = run_reset_shell();
        match res {
            Ok(()) => return,
            Err(e) => {
                eprintln!();
                eprintln!("  Reset failed: {}", e);
                eprintln!();
                std::process::exit(1);
            }
        }
    }

    // If specific mode is requested, run that directly
    let result = if args.cloud {
        cloud::run().await
    } else if args.local {
        local::run().await
    } else {
        // Interactive wizard
        wizard::run_wizard().await
    };

    match result {
        Ok(()) => {}
        Err(e) => {
            // Check if it was a cancellation
            if let ui::SetupError::Cancelled = e {
                ui::print_cancelled();
                std::process::exit(130); // Standard exit code for Ctrl+C
            } else {
                eprintln!();
                eprintln!("  Setup failed: {}", e);
                eprintln!();
                std::process::exit(1);
            }
        }
    }
}

/// Implementation of `--reset-shell`. Pulled out of `run()` so the early-exit
/// path stays readable and the error handling lives in one place.
fn run_reset_shell() -> Result<(), String> {
    let shell_kind = shell::detect_shell();
    if shell_kind == shell::Shell::Unknown {
        return Err("Could not detect your shell. Edit your rc file manually.".into());
    }

    let report = shell::reset_rc(shell_kind)?;
    if report.lines_removed == 0 {
        println!(
            "  No CRW Configuration blocks found in {}",
            report.rc_path.display()
        );
        println!("  Nothing to clean up.");
        return Ok(());
    }
    println!(
        "  Cleaned {} line(s) from {}",
        report.lines_removed,
        report.rc_path.display()
    );
    println!(
        "  Open a new shell or run `source {}` to apply.",
        report.rc_path.display()
    );
    Ok(())
}
