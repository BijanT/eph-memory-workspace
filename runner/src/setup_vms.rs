/// Configure fresh CloudLab machine, install dependencies, and setup VMs
use crate::{GUEST_KERNEL_DIR, WKSPC_DIR};

use clap::{ArgAction, ArgMatches, arg};
use libscail::{GitRepo, Login, ScailError, clone_git_repo, dir, get_user_home_dir};
use spurs::{Execute, SshShell, cmd};

pub fn cli_options() -> clap::Command {
    clap::Command::new("setup_vms")
    	.about("Setup VMs on fresh CloudLab machines")
	.arg_required_else_help(true)
	.disable_version_flag(true)
	.arg(arg!(<hostname> "The domain:port of the remote machine"))
	.arg(arg!(<username> "The username to use for SSH login"))
	.arg(arg!(--git_user <git_user> "Git username for cloning private repos"))
	.arg(arg!(--secret <secret> "Git personal access token or password for cloning private repos"))
	.arg(arg!(--wkspc_branch <wkspc_branch> "(Optional) If passed, clone the specific workspace branch"))
	.arg(arg!(--guest_kernel_branch <guest_kernel_branch> "(Optional) If passed, clone and build the specific guest kernel branch"))
	.arg(arg!(--resize_root "(Option) Resize root partition to fill disk")
		.action(ArgAction::SetTrue))
}

struct Config<'a> {
    git_user: Option<&'a str>,
    secret: Option<&'a str>,
    wkspc_branch: Option<&'a str>,
    guest_kernel_branch: Option<&'a str>,
    resize_root: bool,
}

pub fn run(sub_m: &ArgMatches) -> Result<(), ScailError> {
    let hostname = sub_m.get_one::<String>("hostname").unwrap();
    let username = sub_m.get_one::<String>("username").unwrap();
    let login = Login {
        hostname: hostname.as_str(),
        username: username.as_str(),
        host: hostname.as_str(),
    };

    let config = Config {
        git_user: sub_m.get_one::<String>("git_user").map(|s| s.as_str()),
        secret: sub_m.get_one::<String>("secret").map(|s| s.as_str()),
        wkspc_branch: sub_m.get_one::<String>("wkspc_branch").map(|s| s.as_str()),
        guest_kernel_branch: sub_m
            .get_one::<String>("guest_kernel_branch")
            .map(|s| s.as_str()),
        resize_root: sub_m.get_flag("resize_root"),
    };

    run_inner(&login, &config)
}

fn run_inner<A>(login: &Login<A>, cfg: &Config) -> Result<(), ScailError>
where
    A: std::net::ToSocketAddrs + std::fmt::Display + std::fmt::Debug + Clone,
{
    let ushell = SshShell::with_any_key(login.username, &login.host)?;

    if cfg.resize_root {
        libscail::resize_root_partition(&ushell)?;
    }

    install_host_dependencies(&ushell)?;
    clone_research_workspace(&ushell, cfg)?;

    Ok(())
}

fn install_host_dependencies(ushell: &SshShell) -> Result<(), ScailError> {
    ushell.run(cmd!("sudo apt update; sudo apt upgrade -y"))?;

    let apt_packages = [
        "build-essential",
        "libssl-dev",
        "libelf-dev",
        "libncurses-dev",
        "libevent-dev",
        "dwarves",
        "numactl",
        "linux-tools-common",
        "openjdk-8-jdk",
        "qemu-system",
        "python3",
        "python3-pip",
        "cmake",
        "curl",
        "bpfcc-tools",
        "maven",
        "autoconf",
        "pkgconf",
        "bison",
        "flex",
        "libnuma-dev",
        "cgroup-tools",
    ];
    ushell.run(cmd!("sudo apt install -y {}", apt_packages.join(" ")))?;

    libscail::install_rust(&ushell)?;

    Ok(())
}

fn clone_research_workspace(ushell: &SshShell, cfg: &Config) -> Result<(), ScailError> {
    const SUBMODULES: &[&str] = &["libscail"];
    let user_home = get_user_home_dir(ushell)?;
    let wkspc_dir = dir!(user_home, WKSPC_DIR);
    let user = cfg.git_user.unwrap();
    let secret = cfg.secret.unwrap();
    let branch = cfg.wkspc_branch;

    let wkspc_repo = GitRepo::HttpsPrivate {
        repo: "github.com/BijanT/eph-memory-workspace.git",
        username: user,
        secret,
    };

    clone_git_repo(ushell, wkspc_repo, Some(&wkspc_dir), branch, SUBMODULES)?;

    Ok(())
}

fn setup_guest_vms(ushell: &SshShell, cfg: &Config) -> Result<(), ScailError> {
    let user_home = get_user_home_dir(ushell)?;
    let guest_kernel_dir = dir!(user_home, GUEST_KERNEL_DIR);
    let user = cfg.git_user.unwrap();
    let secret = cfg.secret.unwrap();
    let branch = cfg.guest_kernel_branch;

    let guest_kernel_repo = GitRepo::HttpsPrivate {
        repo: "github.com/BijanT/linux_eph_memory.git",
        username: user,
        secret,
    };

    clone_git_repo(
        ushell,
        guest_kernel_repo,
        Some(&guest_kernel_dir),
        branch,
        &[],
    )?;

    Ok(())
}