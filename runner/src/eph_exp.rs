use std::collections::HashMap;

use clap::arg;

use libscail::{
    Login, ScailError, dir, escape_for_bash, get_user_home_dir,
    output::{Parametrize, Timestamp},
    workloads::{TasksetCtxBuilder, TasksetCtxInterleaving},
};

use serde::{Deserialize, Serialize};

use spurs::{Execute, SshShell, cmd};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Workload {
    AllocData(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize, Parametrize)]
struct Config {
    #[name]
    exp: String,
    #[name]
    workload: Workload,

    thp: bool,
    flamegraph: Option<u32>,
    host_flamegraph: Option<u32>,
    pf_trace: bool,
    bpf_stats: bool,

    #[timestamp]
    timestamp: Timestamp,
}

pub fn cli_options() -> clap::Command {
    clap::Command::new("eph_exp")
        .about("Run ephemeral memory experiments")
        .arg_required_else_help(true)
        .disable_version_flag(true)
        .arg(arg!(<hostname> "The domain:port of the remote"))
        .arg(arg!(<username> "The username to use for SSH login"))
        .arg(
            arg!(--no_thp "Disable Transparent Huge Pages (THP) in the guest VM")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            arg!(--flamegraph [num_splits] "Collect FlameGraph data during the experiment")
                .default_missing_value("1")
                .value_parser(clap::value_parser!(u32)),
        )
        .arg(
            arg!(--host_flamegraph [num_splits] "Collect FlameGraph data on the host")
                .default_missing_value("1")
                .value_parser(clap::value_parser!(u32)),
        )
        .arg(
            arg!(--pf_trace "Collect page fault trace data during the experiment")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            arg!(--bpf_stats "Collect BPF overhead statistics during the experiment")
                .action(clap::ArgAction::SetTrue)
                .requires("pf_trace"),
        )
        .subcommand(
            clap::Command::new("alloc_data")
                .about("Allocate a specified amount of data in memory")
                .arg(
                    arg!(<size_gb> "Size of data to allocate in GB")
                        .value_parser(clap::value_parser!(u64))
                        .required(true),
                ),
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
    let thp = !sub_m.get_flag("no_thp");
    let flamegraph = sub_m.get_one::<u32>("flamegraph").cloned();
    let host_flamegraph = sub_m.get_one::<u32>("host_flamegraph").cloned();
    let pf_trace = sub_m.get_flag("pf_trace");
    let bpf_stats = sub_m.get_flag("bpf_stats");

    // Parse subcommand
    let workload = match sub_m.subcommand() {
        Some(("alloc_data", alloc_m)) => {
            let size_gb: u64 = *alloc_m.get_one::<u64>("size_gb").unwrap();

            Workload::AllocData(size_gb)
        }
        _ => unreachable!(),
    };

    let config = Config {
        exp: "eph_exp".to_string(),
        workload,
        thp,
        flamegraph,
        host_flamegraph,
        pf_trace,
        bpf_stats,
        timestamp: Timestamp::now(),
    };

    run_inner(&login, &config)
}

fn run_inner<A>(login: &Login<A>, cfg: &Config) -> Result<(), ScailError>
where
    A: std::net::ToSocketAddrs + std::fmt::Display + std::fmt::Debug + Clone,
{
    const VM_DOMAIN: &str = "balloon_vm";

    // Reboot the machine to start from a fresh slate
    let host_shell = crate::reboot_and_connect(login)?;
    let host_home = get_user_home_dir(&host_shell)?;
    let host_wkspc = dir!(&host_home, crate::WKSPC_DIR);
    let host_results_dir = dir!(&host_home, crate::RESULTS_DIR);
    let mut cmd_prefix = String::new();

    let (_output_file, params_file, _time_file, _sim_file) = cfg.gen_standard_names();
    let host_flamegraph_file_stem = dir!(&host_results_dir, cfg.gen_file_name("host_flamegraph"));

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
    for i in 0..crate::NUM_VCPUS {
        vcpu_map.insert(i, tctx.next().unwrap());
    }

    // Reserve enough HugeTLB pages for the VM
    host_shell.run(cmd!(
        "echo {} | sudo tee /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages",
        crate::VM_SIZE_GB * 512
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
    let flamegraph_file_stem = dir!(&guest_results_dir, cfg.gen_file_name("flamegraph"));
    let pf_trace_file = dir!(&guest_results_dir, cfg.gen_file_name("pf_trace"));
    let bpf_stats_file = dir!(&guest_results_dir, cfg.gen_file_name("bpf_stats"));
    // We can reuse these files for both guest and host because their tmp
    // directories are different
    let perf_record_file = "/tmp/perf_record.out";

    guest_shell.run(cmd!("mkdir -p {}", &guest_results_dir))?;
    crate::mount_guest_results(&guest_shell, &guest_results_dir)?;

    let proc_name = match &cfg.workload {
        Workload::AllocData(_) => "alloc_data",
    };

    if cfg.thp {
        guest_shell.run(cmd!(
            "echo always | sudo tee /sys/kernel/mm/transparent_hugepage/enabled"
        ))?;
    } else {
        guest_shell.run(cmd!(
            "echo never | sudo tee /sys/kernel/mm/transparent_hugepage/enabled"
        ))?;
    }

    if cfg.pf_trace {
        guest_shell.run(cmd!("rm -f /tmp/stop_pf_trace"))?;
        guest_shell.spawn(cmd!(
            "sudo {}/bpf/pf_trace {} > {}",
            &guest_wkspc,
            proc_name,
            pf_trace_file
        ))?;
        // Give some time for the BPF program to be loaded and verified
        std::thread::sleep(std::time::Duration::from_secs(5));

        if cfg.bpf_stats {
            guest_shell.run(cmd!("echo 1 | sudo tee /proc/sys/kernel/bpf_stats_enabled"))?;
        }
    }

    if cfg.flamegraph.is_some() {
        cmd_prefix.push_str(&format!(
            "sudo perf record -F 99 -a -g -o {} ",
            perf_record_file
        ));
    }

    if cfg.host_flamegraph.is_some() {
        host_shell.spawn(cmd!(
            "sudo perf record -F 99 -a -g -o {} -p $(pgrep qemu-system)",
            perf_record_file
        ))?;
    }

    // Run the specified workload
    match &cfg.workload {
        Workload::AllocData(size_gb) => {
            guest_shell.run(
                cmd!(
                    "{} ./ubmks/alloc_data {} halt | sudo tee {}",
                    cmd_prefix,
                    size_gb,
                    alloc_data_file
                )
                .cwd(&guest_wkspc),
            )?;
        }
    }

    // If collecting FlameGraph data, process it now
    if let Some(num_splits) = cfg.flamegraph {
        generate_flamegraph(
            &guest_shell,
            &guest_wkspc,
            perf_record_file,
            &flamegraph_file_stem,
            num_splits,
        )?;
    }
    if let Some(num_splits) = cfg.host_flamegraph {
        host_shell.run(cmd!("sudo pkill -INT perf"))?;
        // Give the host user permissions to the results directory
        host_shell.run(cmd!("sudo chown -R $USER {}", &host_results_dir))?;
        generate_flamegraph(
            &host_shell,
            &host_wkspc,
            perf_record_file,
            &host_flamegraph_file_stem,
            num_splits,
        )?;
    }

    if cfg.pf_trace {
        // Collect BPF stats if enabled.
        // This needs to happen before stopping pf_trace.
        if cfg.bpf_stats {
            guest_shell.run(cmd!("echo 0 | sudo tee /proc/sys/kernel/bpf_stats_enabled"))?;
            guest_shell.run(cmd!(
                "sudo {}/bpftool/src/bpftool prog show | tee {}",
                &guest_wkspc,
                bpf_stats_file
            ))?;
        }

        // Stop the pf_trace program
        guest_shell.run(cmd!("touch /tmp/stop_pf_trace"))?;
    }

    host_shell.run(cmd!(
        "virsh -c {} shutdown {}",
        crate::LIBVIRT_URI,
        VM_DOMAIN
    ))?;

    println!("RESULTS: {}", dir!(host_results_dir, cfg.gen_file_name("")));
    Ok(())
}

fn generate_flamegraph(
    shell: &SshShell,
    wkspc: &str,
    perf_record_file: &str,
    flamegraph_file_stem: &str,
    num_splits: u32,
) -> Result<(), ScailError> {
    let perf_script_file_stem = "/tmp/perf_script";
    shell.run(cmd!(
        "sudo perf script -i {} | {}/scripts/split_perf_script.py {} {}",
        perf_record_file,
        wkspc,
        num_splits,
        perf_script_file_stem,
    ))?;
    for i in 0..num_splits {
        let perf_script_file = format!("{}_{}.perfscript", perf_script_file_stem, i);
        let flamegraph_file = format!("{}_{}.svg", &flamegraph_file_stem, i);
        shell.run(cmd!(
            "cat {} | ./FlameGraph/stackcollapse-perf.pl > /tmp/flamegraph",
            perf_script_file
        ))?;
        shell.run(cmd!(
            "./FlameGraph/flamegraph.pl /tmp/flamegraph > {}",
            flamegraph_file
        ))?;
    }
    Ok(())
}
