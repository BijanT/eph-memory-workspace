mod balloon_exp;
mod setup_vms;

use clap::arg;
use libscail::{Login, ScailError, ScailErrorType};
use spurs::{cmd, Execute, SshShell};

const RESULTS_DIR: &str = "results";
const WKSPC_DIR: &str = "research-workspace";
const IMGS_DIR: &str = "imgs";
const DOMAINS_DIR: &str = "domains";
const GUEST_KERNEL_DIR: &str = "guest-kernel";

const VM_USERNAME: &str = "ubuntu";
const LIBVIRT_URI: &str = "qemu:///system";
const START_NAT_PORT: u16 = 2222;

fn run() -> Result<(), ScailError> {
    let matches = clap::Command::new("runner")
        .about("Jobserver runner application for the ephemeral memory research project")
        .arg(arg!(--print_results_path "Obselete"))
        .subcommand(crate::balloon_exp::cli_options())
        .subcommand(crate::setup_vms::cli_options())
        .subcommand_required(true)
        .disable_version_flag(true)
        .get_matches();

    match matches.subcommand() {
        Some(("balloon_exp", sub_m)) => crate::balloon_exp::run(sub_m),
        Some(("setup_vms", sub_m)) => crate::setup_vms::run(sub_m),
        _ => unreachable!(),
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

fn nft_rule_exists(shell: &SshShell, table: &str, chain: &str, rule: &str) -> Result<bool, ScailError> {
    let res = shell.run(cmd!("sudo nft list chain ip {} {} | grep '{}'", table, chain, rule));

    // grep returns an exit code of 0 if a match is found, 1 if a match is not found, and 2 on error.
    match res {
        Ok(_) => Ok(true),
        Err(e) => {
            if let spurs::SshError::NonZeroExit { exit, .. } = e {
                if exit == 1 {
                    return Ok(false);
                }
            }
            Err(ScailError::new(ScailErrorType::SpursError(e)))
        }
    }
}

fn setup_port_forwarding(host_shell: &SshShell, host_port: u16, guest_ip: &str, guest_port: u16) -> Result<(), ScailError> {
    let accept_rule = format!("oifname \"virbr0\" ip daddr {} tcp dport {} accept", guest_ip, guest_port);
    let prerouting_rule = format!("tcp dport {} dnat to {}:{}", host_port, guest_ip, guest_port);
    // Make sure that packet forwarding is enabled on the host.
    host_shell.run(cmd!("sudo sysctl -w net.ipv4.ip_forward=1"))?;

    // Make sure to accept forwarded packets if the rule isn't already present.
    if !nft_rule_exists(host_shell, "filter", "FORWARD", &accept_rule)? {
        host_shell.run(cmd!("sudo nft insert rule ip filter FORWARD {}", accept_rule))?;
    }
    // This command will do nothing if the PREROUTING chain already exists.
    host_shell.run(cmd!("sudo nft -- create chain ip nat PREROUTING {{ type nat hook prerouting priority -100 \\; }}"))?;
    // Add DNAT rule if it isn't already present.
    if !nft_rule_exists(host_shell, "nat", "PREROUTING", &prerouting_rule)? {
        host_shell.run(cmd!("sudo nft insert rule ip nat PREROUTING {}", prerouting_rule))?;
    }

    Ok(())
}

fn get_vm_ip(host_shell: &SshShell, vm_name: &str) -> Result<String, ScailError> {
    let output = host_shell.run(cmd!(
        "virsh -c {} domifaddr {} | grep ipv4 | awk '{{print $4}}' | cut -d'/' -f1",
        LIBVIRT_URI,
        vm_name
    ))?;
    let ip = output.stdout.trim().to_string();
    if ip.is_empty() {
        return Err(ScailError::new(ScailErrorType::InvalidValueError{msg: format!(
            "Could not get IP address for VM {}",
            vm_name
        )}));
    }
    Ok(ip)
}

fn reboot_and_connect<A>(login: &Login<A>) -> Result<SshShell, ScailError>
where
    A: std::net::ToSocketAddrs + std::fmt::Display + std::fmt::Debug + Clone,
{
    let ushell = SshShell::with_any_key(login.username, &login.host)?;
    let _ = ushell.run(cmd!("sudo reboot"));

    // It sometimes takes a few seconds for the reboot to actually happen,
    // so make sure to wait for a bit.
    std::thread::sleep(std::time::Duration::from_secs(30));

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

    libscail::dump_sys_info(&ushell)?;

    Ok(ushell)
}

fn start_and_connect_to_vm<A>(
    host_shell: &SshShell,
    domain: &str,
    host: A,
    host_port: u16
) -> Result<SshShell, ScailError>
where
    A: std::net::ToSocketAddrs
{
    host_shell.run(cmd!("virsh -c {} start {}", crate::LIBVIRT_URI, domain))?;

    // Wait a few seconds for the VM to boot
    let mut count = 0;
    let vm_ip = loop {
        match crate::get_vm_ip(host_shell, domain) {
            Ok(ip) => break ip,
            Err(e) => {
                count += 1;
                if count >= 5 {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
        }
    };

    // Setup SSH access to the VM by adding a port forwarding rule on the host
    crate::setup_port_forwarding(
        host_shell,
        host_port,
        &vm_ip,
        22
    )?;

    // It takes some time between the VM's IP being available to its sshd being up
    std::thread::sleep(std::time::Duration::from_secs(15));

    let host_remote_ip = host.to_socket_addrs().unwrap().next().unwrap().ip();
    println!("Connecting to VM {} at {}@{}:{}", domain, crate::VM_USERNAME, host_remote_ip, host_port);
    let guest_shell = SshShell::with_any_key(crate::VM_USERNAME, (host_remote_ip, host_port))?;

    Ok(guest_shell)
}
