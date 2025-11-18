use clap::arg;

use libscail::{dir, get_user_home_dir, dump_sys_info, output::{Parametrize, Timestamp}, Login, ScailError};

use serde::{Deserialize, Serialize};

use spurs::{cmd, Execute, SshShell};
use spurs_util::escape_for_bash;

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
    let ushell = connect_and_setup_host(&login)?;
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

fn connect_and_setup_host<A>(login: &Login<A>) -> Result<SshShell, ScailError>
where
    A: std::net::ToSocketAddrs + std::fmt::Display + std::fmt::Debug + Clone,
{
    let ushell = SshShell::with_any_key(login.username, &login.host)?;
    let _ = ushell.run(cmd!("sudo reboot"));

    // It sometimes takes a few seconds for the reboot to actually happen,
    // so make sure to wait for a bit.
    std::thread::sleep(std::time::Duration::from_secs(10));

    // Keep trying to connect until we succeed
    let ushell = {
	let mut shell;
	loop {
	    println!("Attempting to reconnect...");
	    shell = match SshShell::with_any_key(login.username, &login.host) {
		Ok(s) => s,
		Err(_) => {
		    std::thread::sleep(std::time::Duration::from_secs(10));
		    continue;
		}
	    };
	    match shell.run(cmd!("whoami")) {
		Ok(_) => break,
		Err(_) => {
		    std::thread::sleep(std::time::Duration::from_secs(10));
		    continue;
		}
	    }
	}

	shell
    };

    dump_sys_info(&ushell)?;

    Ok(ushell)
}