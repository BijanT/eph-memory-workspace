use std::collections::HashMap;

use clap::arg;

use libscail::{
    Login, ScailError, ScailErrorType, dir, escape_for_bash, get_user_home_dir,
    output::{Parametrize, Timestamp},
    workloads::{TasksetCtxBuilder, TasksetCtxInterleaving},
};

use serde::{Deserialize, Serialize};

use spurs::{Execute, SshShell, cmd};

#[derive(Debug, Clone, Serialize, Deserialize, Parametrize)]
struct Config {
    #[name]
    exp: String,

    alloc_size: usize,
    shrink_size: usize,

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
        .arg(
            arg!(--alloc_size <alloc_size> "The amount of data in GB to allocate")
                .value_parser(clap::value_parser!(usize)),
        )
        .arg(
            arg!(--shrink_size <shrink_size> "The size to shrink the VM to after allocating data")
                .value_parser(clap::value_parser!(usize)),
        )
}

pub fn run(sub_m: &clap::ArgMatches) -> Result<(), ScailError> {
    let hostname = sub_m.get_one::<String>("hostname").unwrap();
    let username = sub_m.get_one::<String>("username").unwrap();
    let login = Login {
        hostname: hostname.as_str(),
        username: username.as_str(),
        host: hostname.as_str(),
    };
    let alloc_size = sub_m.get_one::<usize>("alloc_size").copied().unwrap();
    let shrink_size = sub_m.get_one::<usize>("shrink_size").copied().unwrap();

    let cfg = Config {
        exp: "balloon-exp".to_string(),
        alloc_size,
        shrink_size,
        timestamp: Timestamp::now(),
    };

    run_inner(&login, &cfg)
}

fn run_inner<A>(login: &Login<A>, cfg: &Config) -> Result<(), ScailError>
where
    A: std::net::ToSocketAddrs + std::fmt::Display + std::fmt::Debug + Clone,
{
    const VM_DOMAIN: &str = "balloon_vm";
    const VM_SIZE_GB: usize = 48;
    const NUM_VCPUS: usize = 2;
    // Reboot the machine to start from a fresh slate
    let host_shell = crate::reboot_and_connect(login)?;
    let host_home = get_user_home_dir(&host_shell)?;
    let host_results_dir = dir!(host_home, crate::RESULTS_DIR);
    let shrink_time_file = dir!(&host_results_dir, cfg.gen_file_name("shrink_time"));

    let (_output_file, params_file, _time_file, _sim_file) = cfg.gen_standard_names();

    host_shell.run(cmd!("mkdir -p {}", host_results_dir))?;
    host_shell.run(cmd!(
        "echo {} | sudo tee {}",
        escape_for_bash(&serde_json::to_string(&cfg)?),
        dir!(&host_results_dir, &params_file)
    ))?;

    let mut tctx = TasksetCtxBuilder::from_lscpu(&host_shell)?
        .numa_interleaving(TasksetCtxInterleaving::Sequential)
        .skip_hyperthreads(true)
        .build();

    let mut vcpu_map: HashMap<usize, usize> = HashMap::new();
    for i in 0..NUM_VCPUS {
        vcpu_map.insert(i, tctx.next().unwrap());
    }

    // Reserve enough HugeTLB pages for the VM
    host_shell.run(cmd!(
        "echo {} | sudo tee /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages",
        VM_SIZE_GB * 512
    ))?;

    let guest_shell = crate::start_and_connect_to_vm(
        &host_shell,
        VM_DOMAIN,
        &login.host,
        crate::START_NAT_PORT,
        Some(vcpu_map),
    )?;
    let guest_home = get_user_home_dir(&guest_shell)?;
    let guest_results_dir = dir!(&guest_home, crate::RESULTS_DIR);
    let guest_wkspc = dir!(&guest_home, crate::WKSPC_DIR);
    let alloc_data_file = dir!(&guest_results_dir, cfg.gen_file_name("alloc_data"));

    guest_shell.run(cmd!("mkdir -p {}", &guest_results_dir))?;
    crate::mount_guest_results(&guest_shell, &guest_results_dir)?;

    // Configure the swap space
    guest_shell.run(cmd!("sudo fallocate -l 32G /swapfile"))?;
    guest_shell.run(cmd!("sudo chmod 0600 /swapfile"))?;
    guest_shell.run(cmd!("sudo mkswap /swapfile"))?;
    guest_shell.run(cmd!("sudo swapon /swapfile"))?;

    guest_shell.spawn(
        cmd!(
            "./ubmks/alloc_data {} | sudo tee {}",
            cfg.alloc_size,
            alloc_data_file
        )
        .cwd(&guest_wkspc),
    )?;
    // alloc_data will print some data once it has finished allocating.
    // Wait for that.
    while !test_written(&guest_shell, &alloc_data_file)? {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    // Inflate the balloon to take memory from the VM and time how long it takes
    let target_shrink_size_kb = cfg.shrink_size * 1024 * 1024;
    host_shell.run(cmd!(
        "virsh -c {} setmem --domain {} --size {}G",
        crate::LIBVIRT_URI,
        VM_DOMAIN,
        cfg.shrink_size
    ))?;
    let start_time = std::time::Instant::now();
    loop {
        const MAX_WAIT_MS: usize = 5000;
        const MIN_WAIT_MS: usize = 500;
        const ORIG_SIZE_KB: usize = VM_SIZE_GB * 1024 * 1024;
        let guest_size_kb = host_shell
            .run(cmd!(
                "virsh -c {} dommemstat {} | grep actual | awk '{{print $2}}'",
                crate::LIBVIRT_URI,
                VM_DOMAIN
            ))?
            .stdout
            .trim()
            .parse::<usize>()
            .unwrap();

        if guest_size_kb == target_shrink_size_kb {
            break;
        } else {
            // Dynamically scale the wait time to the amount of data left to swap out
            let wait_time_ms = MAX_WAIT_MS * (guest_size_kb - target_shrink_size_kb)
                / (ORIG_SIZE_KB - target_shrink_size_kb);
            let wait_time_ms = std::cmp::max(wait_time_ms, MIN_WAIT_MS);
            std::thread::sleep(std::time::Duration::from_millis(wait_time_ms as u64));
        }
    }
    let elapsed_time = start_time.elapsed();
    host_shell.run(cmd!(
        "echo {} | sudo tee {}",
        elapsed_time.as_secs_f64(),
        shrink_time_file
    ))?;

    host_shell.run(cmd!(
        "virsh -c {} shutdown {}",
        crate::LIBVIRT_URI,
        VM_DOMAIN
    ))?;

    println!("RESULTS: {}", dir!(host_results_dir, cfg.gen_file_name("")));
    Ok(())
}

/// Returns true if the file has been written to since last read, false otherwise
fn test_written(shell: &SshShell, file: &str) -> Result<bool, ScailError> {
    let res = shell.run(cmd!("test -N {}", file));
    match res {
        Ok(_) => Ok(true),
        Err(e) => {
            if let spurs::SshError::NonZeroExit { exit, .. } = e
                && exit == 1
            {
                return Ok(false);
            }
            Err(ScailError::new(ScailErrorType::SpursError(e)))
        }
    }
}
