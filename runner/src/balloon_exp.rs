use clap::arg;

use libscail::{
    Login, ScailError, dir, escape_for_bash, get_user_home_dir,
    output::{Parametrize, Timestamp},
};

use serde::{Deserialize, Serialize};

use spurs::{Execute, cmd};

#[derive(Debug, Clone, Serialize, Deserialize, Parametrize)]
struct Config {
    #[name]
    exp: String,

    #[timestamp]
    timestamp: Timestamp,
}

pub fn cli_options() -> clap::Command {
    clap::Command::new("balloon_exp")
        .about("Run a ballooning experiment")
        .arg_required_else_help(true)
        .disable_version_flag(true)
        .arg(arg!(<hostname> "The domain:port of the remote"))
        .arg(arg!(<username> "The username to use for SSH login"))
}

pub fn run(sub_m: &clap::ArgMatches) -> Result<(), ScailError> {
    let hostname = sub_m.get_one::<String>("hostname").unwrap();
    let username = sub_m.get_one::<String>("username").unwrap();
    let login = Login {
        hostname: hostname.as_str(),
        username: username.as_str(),
        host: hostname.as_str(),
    };

    let cfg = Config {
        exp: "balloon-exp".to_string(),
        timestamp: Timestamp::now(),
    };

    run_inner(&login, &cfg)
}

fn run_inner<A>(login: &Login<A>, cfg: &Config) -> Result<(), ScailError>
where
    A: std::net::ToSocketAddrs + std::fmt::Display + std::fmt::Debug + Clone,
{
    // Reboot the machine to start from a fresh slate
    let ushell = crate::reboot_and_connect(&login)?;
    let user_home = get_user_home_dir(&ushell)?;
    let results_dir = dir!(user_home, crate::RESULTS_DIR);

    let (_output_file, params_file, _time_file, _sim_file) = cfg.gen_standard_names();

    ushell.run(cmd!("mkdir -p {}", results_dir))?;
    ushell.run(cmd!(
        "echo {} > {}",
        escape_for_bash(&serde_json::to_string(&cfg)?),
        dir!(&results_dir, &params_file)
    ))?;

    println!("RESULTS: {}", dir!(results_dir, cfg.gen_file_name("")));
    Ok(())
}
