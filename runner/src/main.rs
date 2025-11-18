mod balloon_exp;

use clap::arg;
use libscail::{ScailError, ScailErrorType};

const RESULTS_DIR: &str = "results/";

fn run() -> Result<(), ScailError> {
    let matches = clap::Command::new("runner")
        .about("Jobserver runner application for the ephemeral memory research project")
        .arg(arg!(--print_results_path "Obselete"))
        .subcommand(crate::balloon_exp::cli_options())
        .subcommand_required(true)
        .disable_version_flag(true)
        .get_matches();

    match matches.subcommand() {
        Some(("balloon_exp", sub_m)) => crate::balloon_exp::run(sub_m),
        _ => unreachable!()
    }
}

fn main() {
    use console::style;

    env_logger::init();

    unsafe {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    // If an error was returned, try to print something helpful.
    if let Err(e) = run() {
        const MESSAGE: &str = r#"== ERROR ==================================================================================
        `runner` encountered an error. The command log above may offer clues. If the error pertains to SSH,
        you may be able to get useful information by setting the RUST_LOG=debug enviroent variable. It is
        recommended that you use `debug` builds of `runner`, rather than `release`, as the performance of
        `runner` is not that important and is almost always dominated by the experiment being run.
        "#;

        println!("{}", style(MESSAGE).red().bold());

        if let ScailErrorType::SpursError(_) = &e.err_type {
            println!("An error occurred while attempting to run a command over SSH");
        }

        // Print the error and backtrace.
        println!(
            "`runner` encountered the following error:\n{}\n{}",
            e.to_string(),
            e.backtrace(),
        );

        std::process::exit(101);
    }
}
