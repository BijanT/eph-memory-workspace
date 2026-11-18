use std::collections::HashMap;

use clap::arg;

use libscail::{
    Login, ScailError, ScailErrorType,
    background::{BackgroundContext, BackgroundTask},
    dir, escape_for_bash, get_user_home_dir,
    output::{Parametrize, Timestamp},
    workloads::{TasksetCtx, TasksetCtxBuilder, TasksetCtxInterleaving},
};

use serde::{Deserialize, Serialize};

use spurs::{Execute, SshShell, cmd};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Workload {
    AllocData(u64),
    MultiAlloc {
        size_gb: u64,
        procs: u64,
    },
    Spark {
        scale_factor: u64,
        executor_mem_gb: u64,
    },
    DuckDB,
}

#[derive(Debug, Clone, Serialize, Deserialize, Parametrize)]
struct Config {
    #[name]
    exp: String,
    #[name(self.donor_workload.is_some())]
    donor_workload: Option<Workload>,
    #[name(self.consumer_workload.is_some())]
    consumer_workload: Option<Workload>,

    thp: bool,
    flamegraph: Option<u32>,
    host_flamegraph: Option<u32>,
    pf_trace: bool,
    host_pf_trace: bool,
    host_kvm_pf_trace: bool,
    bpf_stats: bool,
    host_perf_stat: bool,
    numactl: bool,

    #[timestamp]
    timestamp: Timestamp,
}

/// Paths inside a guest VM. Computed once when the VM is started.
struct GuestPaths {
    wkspc: String,
    workloads_dir: String,
    local_results_dir: String,
    shared_results_dir: String,

    // Result files. These include the VM's domain so that results from
    // different VMs in the same experiment do not collide.
    alloc_data_file: String,
    multi_alloc_file: String,
    spark_file: String,
    spark_time_file: String,
    duckdb_file: String,
    pf_trace_file: String,
    bpf_stats_file: String,
    numactl_file: String,
    flamegraph_file_stem: String,
}

impl GuestPaths {
    fn new(cfg: &Config, domain: &str, guest_home: &str) -> Self {
        let local_results_dir = dir!(guest_home, crate::LOCAL_RESULTS_DIR);
        let file = |ext: &str| {
            dir!(
                &local_results_dir,
                cfg.gen_file_name(&format!("{}.{}", domain, ext))
            )
        };

        GuestPaths {
            wkspc: dir!(guest_home, crate::WKSPC_DIR),
            workloads_dir: dir!(guest_home, crate::WORKLOADS_DIR),
            shared_results_dir: dir!(guest_home, crate::RESULTS_DIR),
            alloc_data_file: file("alloc_data"),
            multi_alloc_file: file("multi_alloc"),
            spark_file: file("spark"),
            spark_time_file: file("spark_time"),
            duckdb_file: file("duckdb"),
            pf_trace_file: file("pf_trace"),
            bpf_stats_file: file("bpf_stats"),
            numactl_file: file("numactl"),
            flamegraph_file_stem: file("flamegraph"),
            local_results_dir,
        }
    }
}

struct VmInfo {
    shell: SshShell,
    domain: String,
    /// PID of this VM's qemu process on the host.
    qemu_pid: u32,
    workload: Workload,
    proc_name: String,
    cmd_prefix: String,
    vm_log_path: String,
    paths: GuestPaths,
}

// Clap does not seem to support multiple different subcommands, so to support
// specifying different workloads for the donor and consumer, we use a manual
// parser for the workloads instead of clap subcommands.
const WORKLOAD_FORMATS: &str = "  alloc_data:<size_gb>\n  \
    multi_alloc:<size_gb>:<procs>\n  \
    spark:<scale_factor>:<executor_mem_gb>\n  \
    duckdb";
fn parse_workloads(s: &str) -> Result<Workload, String> {
    let mut split = s.split(':');
    let workload_name = split.next().unwrap_or("");
    let args: Vec<&str> = split.collect();
    let expect = |n: usize| {
        if args.len() == n {
            Ok(())
        } else {
            Err(format!(
                "expected {} argument(s) for workload '{}', got {}. Expected one of:\n{}",
                n,
                workload_name,
                args.len(),
                WORKLOAD_FORMATS
            ))
        }
    };
    let to_num = |i: usize, param: &str| -> Result<u64, String> {
        args[i]
            .parse::<u64>()
            .map_err(|e| format!("{}: invalid {} '{}': {}", workload_name, param, args[i], e))
    };

    match workload_name {
        "alloc_data" => {
            expect(1)?;
            let size_gb = to_num(0, "size_gb")?;
            Ok(Workload::AllocData(size_gb))
        }
        "multi_alloc" => {
            expect(2)?;
            let size_gb = to_num(0, "size_gb")?;
            let procs = to_num(1, "procs")?;
            Ok(Workload::MultiAlloc { size_gb, procs })
        }
        "spark" => {
            expect(2)?;
            let scale_factor = to_num(0, "scale_factor")?;
            let executor_mem_gb = to_num(1, "executor_mem_gb")?;
            Ok(Workload::Spark {
                scale_factor,
                executor_mem_gb,
            })
        }
        "duckdb" => {
            expect(0)?;
            Ok(Workload::DuckDB)
        }
        _ => Err(format!(
            "unknown workload '{}'. Expected one of:\n{}",
            workload_name, WORKLOAD_FORMATS
        )),
    }
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
            arg!(--host_pf_trace "Collect page fault trace data on host during the experiment")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            arg!(--host_kvm_pf_trace "Collect trace data on host for kvm_mmu page fault()")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            arg!(--bpf_stats "Collect BPF overhead statistics during the experiment")
                .action(clap::ArgAction::SetTrue)
                .requires("pf_trace"),
        )
        .arg(
            arg!(--host_perf_stat "Collect perf stat data on host during the experiment")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            arg!(--numactl "Periodically collect numactl -H data in the guest during the experiment")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            arg!(--donor_workload <workload> "The workload to run in the donor VM")
                .value_parser(parse_workloads)
                .long_help(format!("The workload to run. One of:\n{}", WORKLOAD_FORMATS)),
        )
        .arg(
            arg!(--consumer_workload <workload> "The workload to run in the consumer VM")
                .value_parser(parse_workloads)
                .long_help(format!("The workload to run. One of:\n{}", WORKLOAD_FORMATS)),
        )
        .group(
            clap::ArgGroup::new("workloads")
                .args(["donor_workload", "consumer_workload"])
                .required(true)
                .multiple(true),
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
    let host_pf_trace = sub_m.get_flag("host_pf_trace");
    let host_kvm_pf_trace = sub_m.get_flag("host_kvm_pf_trace");
    let bpf_stats = sub_m.get_flag("bpf_stats");
    let host_perf_stat = sub_m.get_flag("host_perf_stat");
    let numactl = sub_m.get_flag("numactl");
    let donor_workload = sub_m.get_one::<Workload>("donor_workload").cloned();
    let consumer_workload = sub_m.get_one::<Workload>("consumer_workload").cloned();

    // Both VMs mount the same workloads directory, so two Spark workloads
    // would generate their TPC-DS data in the same place.
    if matches!(
        (&donor_workload, &consumer_workload),
        (Some(Workload::Spark { .. }), Some(Workload::Spark { .. }))
    ) {
        return Err(ScailError::new(ScailErrorType::InvalidValueError {
            msg: "spark cannot be the workload for both the donor and consumer VMs: \
                  they share the workloads directory, so their TPC-DS data would conflict"
                .to_string(),
        }));
    }

    let config = Config {
        exp: "eph_exp".to_string(),
        donor_workload,
        consumer_workload,
        thp,
        flamegraph,
        host_flamegraph,
        pf_trace,
        host_pf_trace,
        host_kvm_pf_trace,
        bpf_stats,
        host_perf_stat,
        numactl,
        timestamp: Timestamp::now(),
    };

    run_inner(&login, &config)
}

fn run_inner<A>(login: &Login<A>, cfg: &Config) -> Result<(), ScailError>
where
    A: std::net::ToSocketAddrs + std::fmt::Display + std::fmt::Debug + Clone,
{
    const DONOR_DOMAIN: &str = "donor_vm";
    const CONSUMER_DOMAIN: &str = "consumer_vm";

    // Reboot the machine to start from a fresh slate
    let host_shell = crate::reboot_and_connect(login)?;
    let host_home = get_user_home_dir(&host_shell)?;
    let host_wkspc = dir!(&host_home, crate::WKSPC_DIR);
    let host_results_dir = dir!(&host_home, crate::RESULTS_DIR);

    let (_output_file, params_file, _time_file, _sim_file) = cfg.gen_standard_names();
    let host_flamegraph_file_stem = dir!(&host_results_dir, cfg.gen_file_name("host_flamegraph"));
    // Host pf_trace and kvm_pf_trace trace one process each, so one instance
    // runs per VM, and their output files are named by the VM's domain.
    let host_trace_tmp_file = |tool: &str, vm: &VmInfo| format!("/tmp/{}.{}.out", tool, vm.domain);
    let host_trace_file = |tool: &str, vm: &VmInfo| {
        dir!(
            &host_results_dir,
            cfg.gen_file_name(&format!("{}.{}", vm.domain, tool))
        )
    };
    let host_perf_stat_file = dir!(&host_results_dir, cfg.gen_file_name("host_perf_stat"));

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

    // Reserve enough HugeTLB pages for the VM
    host_shell.run(cmd!(
        "echo {} | sudo tee /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages",
        crate::VM_MEMORY_POOL_SIZE_GB * 512
    ))?;

    // Make sure the directory for the QMP sockets exists
    crate::create_qmp_dir(&host_shell)?;

    // Start a VM for each requested workload. Each started VM gets the next
    // NAT port.
    let mut nat_port = crate::START_NAT_PORT;
    let mut vms = Vec::new();
    for (domain, workload) in [
        (DONOR_DOMAIN, &cfg.donor_workload),
        (CONSUMER_DOMAIN, &cfg.consumer_workload),
    ] {
        if let Some(workload) = workload {
            vms.push(start_vm(
                &host_shell,
                cfg,
                domain,
                &login.host,
                nat_port,
                &mut tctx,
                workload,
            )?);
            nat_port += 1;
        }
    }

    // The background contexts borrow from `vms`, so they are kept in a
    // separate Vec, in the same order, and started only after all VMs are up.
    let bgctxs = vms
        .iter()
        .map(|vm_info| start_vm_bg_tasks(vm_info, cfg))
        .collect::<Result<Vec<_>, _>>()?;

    // Once the VMs are started, we can start the host level profiling.
    // pf_trace and kvm_pf_trace only trace one process at a time, so spawn one
    // instance per VM. They all stop when their stop file is created.
    if cfg.host_pf_trace {
        host_shell.run(cmd!("rm -f {}", crate::STOP_PF_TRACE_FILE))?;
        for vm in &vms {
            host_shell.spawn(cmd!(
                "sudo {}/bpf/pf_trace {} > {}",
                &host_wkspc,
                vm.qemu_pid,
                host_trace_tmp_file("host_pf_trace", vm)
            ))?;
        }
        // Give some time for the BPF programs to be loaded and verified
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    if cfg.host_kvm_pf_trace {
        host_shell.run(cmd!("rm -f {}", crate::STOP_KVM_PF_TRACE_FILE))?;
        for vm in &vms {
            host_shell.spawn(cmd!(
                "sudo {}/bpf/kvm_pf_trace {} > {}",
                &host_wkspc,
                vm.qemu_pid,
                host_trace_tmp_file("host_kvm_pf_trace", vm)
            ))?;
        }
        // Give some time for the BPF programs to be loaded and verified
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    if cfg.host_flamegraph.is_some() {
        host_shell.spawn(cmd!(
            "sudo perf kvm --host --guest record -F 99 -a -g -o {} -p $(pgrep -d, qemu-system)",
            crate::PERF_RECORD_FILE
        ))?;
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    if cfg.host_perf_stat {
        let events = [
            "dtlb_load_misses.miss_causes_a_walk",
            "dtlb_store_misses.miss_causes_a_walk",
            "dtlb_load_misses.walk_duration",
            "dtlb_store_misses.walk_duration",
        ];
        let event_str = events.join(" -e ");
        host_shell.spawn(cmd!(
            "sudo perf stat -a -p $(pgrep -d, qemu-system) -I 75 -e {} -o {} -- sleep 10000",
            event_str,
            host_perf_stat_file
        ))?;
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Run the workloads
    let spawn_handles = vms
        .iter()
        .map(start_vm_workload)
        .collect::<Result<Vec<_>, _>>()?;

    // Wait for the workloads to finish
    for handle in spawn_handles {
        handle.join().1?;
    }

    // If collecting FlameGraph data, process it now, so the results don't
    // include stuff from the cleanup phase.
    if cfg.host_flamegraph.is_some() || cfg.host_perf_stat {
        host_shell.run(cmd!("sudo pkill -INT perf"))?;
    }

    // Cleanup the VMs and collect results
    for bgctx in bgctxs {
        bgctx.notify_and_join_all()?;
    }
    for vm_info in &vms {
        cleanup_vm(cfg, &host_shell, &host_results_dir, vm_info)?;
    }

    // Give the host user permissions to the results directory
    host_shell.run(cmd!("sudo chown -R $USER {}", &host_results_dir))?;
    if cfg.host_pf_trace {
        // Stop every pf_trace instance
        host_shell.run(cmd!("touch {}", crate::STOP_PF_TRACE_FILE))?;
        std::thread::sleep(std::time::Duration::from_secs(1));
        for vm in &vms {
            host_shell.run(cmd!(
                "mv {} {}",
                host_trace_tmp_file("host_pf_trace", vm),
                host_trace_file("host_pf_trace", vm)
            ))?;
        }
    }
    if cfg.host_kvm_pf_trace {
        // Stop every kvm_pf_trace instance
        host_shell.run(cmd!("touch {}", crate::STOP_KVM_PF_TRACE_FILE))?;
        std::thread::sleep(std::time::Duration::from_secs(1));
        for vm in &vms {
            host_shell.run(cmd!(
                "mv {} {}",
                host_trace_tmp_file("host_kvm_pf_trace", vm),
                host_trace_file("host_kvm_pf_trace", vm)
            ))?;
        }
    }
    if let Some(num_splits) = cfg.host_flamegraph {
        generate_flamegraph(
            &host_shell,
            &host_wkspc,
            crate::PERF_RECORD_FILE,
            &host_flamegraph_file_stem,
            num_splits,
        )?;
    }

    println!("RESULTS: {}", dir!(host_results_dir, cfg.gen_file_name("")));
    Ok(())
}

fn start_vm<A>(
    host_shell: &SshShell,
    cfg: &Config,
    domain: &str,
    host: &A,
    host_port: u16,
    tctx: &mut TasksetCtx,
    workload: &Workload,
) -> Result<VmInfo, ScailError>
where
    A: std::net::ToSocketAddrs + std::fmt::Display + std::fmt::Debug + Clone,
{
    let vm_log_path = format!("/var/log/libvirt/qemu/{}.log", domain);

    // libvirt appends to the domain's log across boots. Truncate it so the
    // log we save contains only this run.
    host_shell.run(cmd!("sudo truncate -s 0 {}", &vm_log_path))?;

    // Determine what host CPUs the vCPUs should be pinned to.
    let mut vcpu_map: HashMap<usize, usize> = HashMap::new();
    for i in 0..crate::NUM_VCPUS {
        vcpu_map.insert(i, tctx.next().unwrap());
    }

    let guest_shell =
        crate::start_and_connect_to_vm(host_shell, domain, host, host_port, Some(vcpu_map))?;
    // libvirt writes the qemu PID of each running domain to this file.
    let qemu_pid: u32 = host_shell
        .run(cmd!("sudo cat /run/libvirt/qemu/{}.pid", domain))?
        .stdout
        .trim()
        .parse()
        .map_err(|e| {
            ScailError::new(ScailErrorType::InvalidValueError {
                msg: format!("could not parse qemu PID for {}: {}", domain, e),
            })
        })?;
    let guest_home = get_user_home_dir(&guest_shell)?;
    let paths = GuestPaths::new(cfg, domain, &guest_home);
    let mut cmd_prefix = String::new();

    guest_shell.run(cmd!("mkdir -p {}", &paths.shared_results_dir))?;
    guest_shell.run(cmd!("mkdir -p {}", &paths.local_results_dir))?;
    guest_shell.run(cmd!("mkdir -p {}", &paths.workloads_dir))?;
    crate::mount_guest_results(&guest_shell, &paths.shared_results_dir)?;
    crate::mount_workloads_dir(&guest_shell, &paths.workloads_dir)?;
    // Make sure any old results are cleared out before starting
    guest_shell.run(cmd!("rm -rf {}/*", &paths.local_results_dir))?;

    let proc_name = match workload {
        Workload::AllocData(_) => "alloc_data",
        Workload::MultiAlloc { .. } => "multi_alloc",
        // TODO: Find the actual Spark process name
        // It's probably something like "java" or something
        Workload::Spark { .. } => "TODO",
        Workload::DuckDB => "benchmark_runne",
    };
    if proc_name.len() > 15 {
        return Err(ScailError::new(ScailErrorType::InvalidValueError {
            msg: format!("Process name {} is too long for pgrep", proc_name),
        }));
    }

    if let Workload::Spark { scale_factor, .. } = workload {
        // Generate the TPC-DS database if needed
        generate_spark_db(&guest_shell, *scale_factor, &paths.workloads_dir)?;
    }

    if cfg.thp {
        guest_shell.run(cmd!(
            "echo always | sudo tee /sys/kernel/mm/transparent_hugepage/enabled"
        ))?;
    } else {
        guest_shell.run(cmd!(
            "echo never | sudo tee /sys/kernel/mm/transparent_hugepage/enabled"
        ))?;
    }

    if cfg.flamegraph.is_some() {
        cmd_prefix.push_str(&format!(
            "sudo perf record -F 99 -a -g -o {} ",
            crate::PERF_RECORD_FILE
        ));
    }

    Ok(VmInfo {
        shell: guest_shell,
        domain: domain.to_string(),
        qemu_pid,
        workload: workload.clone(),
        proc_name: proc_name.to_string(),
        cmd_prefix,
        vm_log_path,
        paths,
    })
}

fn start_vm_bg_tasks<'s>(
    vm_info: &'s VmInfo,
    cfg: &Config,
) -> Result<BackgroundContext<'s>, ScailError> {
    let mut bgctx = BackgroundContext::new(&vm_info.shell);
    let paths = &vm_info.paths;

    if cfg.pf_trace {
        vm_info
            .shell
            .run(cmd!("rm -f {}", crate::STOP_PF_TRACE_FILE))?;
        vm_info.shell.spawn(cmd!(
            "sudo {}/bpf/pf_trace {} > {}",
            &paths.wkspc,
            vm_info.proc_name,
            &paths.pf_trace_file
        ))?;
        // Give some time for the BPF program to be loaded and verified
        std::thread::sleep(std::time::Duration::from_secs(5));

        if cfg.bpf_stats {
            vm_info
                .shell
                .run(cmd!("echo 1 | sudo tee /proc/sys/kernel/bpf_stats_enabled"))?;
        }
    }

    if cfg.numactl {
        bgctx.spawn(BackgroundTask {
            name: "numactl",
            period: 1, // Seconds
            cmd: format!(
                "(echo ts $(date +%s.%N); numactl -H) >> {} 2>/dev/null",
                paths.numactl_file
            ),
            ensure_started: paths.numactl_file.clone(),
        })?;
    }

    Ok(bgctx)
}

fn start_vm_workload(vm_info: &VmInfo) -> Result<spurs::SshSpawnHandle, ScailError> {
    let paths = &vm_info.paths;

    let cmd = match vm_info.workload {
        Workload::AllocData(size_gb) => cmd!(
            "{} ./ubmks/alloc_data {} halt | sudo tee {}",
            vm_info.cmd_prefix,
            size_gb,
            &paths.alloc_data_file
        )
        .cwd(&paths.wkspc),
        Workload::MultiAlloc { size_gb, procs } => cmd!(
            "{} ./ubmks/multi_alloc {} {} 2>&1 | sudo tee {}",
            vm_info.cmd_prefix,
            size_gb,
            procs,
            &paths.multi_alloc_file
        )
        .cwd(&paths.wkspc),
        Workload::Spark {
            executor_mem_gb, ..
        } => {
            let query_file = dir!(&paths.workloads_dir, "tpcds", "queries", "all.sql");
            let spark_sql_bin = dir!(&paths.workloads_dir, "spark", "bin", "spark-sql");

            cmd!(
                "TIMEFORMAT=%3R; {{ time {} {} --driver-memory {}g --database tpcds -f {} 2>&1 | sudo tee {} ; }} 2> {}",
                vm_info.cmd_prefix,
                spark_sql_bin,
                executor_mem_gb,
                query_file,
                &paths.spark_file,
                &paths.spark_time_file
            )
            .use_bash()
        }
        Workload::DuckDB => {
            let duckdb_bin = dir!(
                &paths.wkspc,
                "workloads",
                "duckdb",
                "build",
                "release",
                "benchmark",
                "benchmark_runner"
            );
            let queries = "benchmark/large/tpch-sf100/.*";
            cmd!(
                "{} {} --out={} --disable-timeout {}",
                vm_info.cmd_prefix,
                duckdb_bin,
                &paths.duckdb_file,
                queries
            )
        }
    };

    vm_info.shell.spawn(cmd).map_err(|e| e.into())
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
        let flamegraph_file = format!("{}_{}.svg", flamegraph_file_stem, i);
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

fn cleanup_vm(
    cfg: &Config,
    host_shell: &SshShell,
    host_results_dir: &str,
    vm_info: &VmInfo,
) -> Result<(), ScailError> {
    let paths = &vm_info.paths;
    let host_vm_log_file = dir!(
        host_results_dir,
        cfg.gen_file_name(&format!("{}.log", vm_info.domain))
    );

    if let Some(num_splits) = cfg.flamegraph {
        generate_flamegraph(
            &vm_info.shell,
            &paths.wkspc,
            crate::PERF_RECORD_FILE,
            &paths.flamegraph_file_stem,
            num_splits,
        )?;
    }

    if cfg.pf_trace {
        // Collect BPF stats if enabled.
        // This needs to happen before stopping pf_trace.
        if cfg.bpf_stats {
            vm_info
                .shell
                .run(cmd!("echo 0 | sudo tee /proc/sys/kernel/bpf_stats_enabled"))?;
            vm_info.shell.run(cmd!(
                "sudo {}/bpftool/src/bpftool prog show | tee {}",
                &paths.wkspc,
                &paths.bpf_stats_file
            ))?;
        }

        // Stop the pf_trace program
        vm_info
            .shell
            .run(cmd!("touch {}", crate::STOP_PF_TRACE_FILE))?;
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    // We suspect that if virtiofs tries to access pagecache files in donatable
    // memory, which is not sharable, it will hang. Memory hogging applications
    // should be done, so we don't have to worry about their memory, but they
    // may have added a bunch of files to the pagecache. So, to be safe, drop
    // the page cache before copying the results to the host.
    // TODO: confirm and look into fixing this after I defend.
    vm_info
        .shell
        .run(cmd!("sync; echo 3 | sudo tee /proc/sys/vm/drop_caches"))?;
    vm_info.shell.run(cmd!(
        "cp -r {}/. {}",
        &paths.local_results_dir,
        &paths.shared_results_dir
    ))?;

    host_shell.run(cmd!(
        "virsh -c {} shutdown {}",
        crate::LIBVIRT_URI,
        vm_info.domain
    ))?;
    // Wait for the VM to fully stop so the log is complete
    host_shell.run(cmd!(
        "while [ \"$(virsh -c {} domstate {})\" != \"shut off\" ]; do sleep 1; done",
        crate::LIBVIRT_URI,
        vm_info.domain
    ))?;
    host_shell.run(cmd!(
        "sudo cp {} {}",
        &vm_info.vm_log_path,
        &host_vm_log_file
    ))?;

    Ok(())
}

fn generate_spark_db(
    guest_shell: &SshShell,
    scale: u64,
    guest_workloads_dir: &str,
) -> Result<(), ScailError> {
    let user_home = get_user_home_dir(guest_shell)?;
    let tpcds_data_dir = dir!(guest_workloads_dir, "data");
    let tpcds_ddl_dir = dir!(guest_workloads_dir, "tpcds", "ddl");
    let tpcds_tools_dir = dir!(&user_home, "tpcds-kit", "tools");
    let spark_sql_bin = dir!(guest_workloads_dir, "spark", "bin", "spark-sql");
    let scale_factor_file = dir!(&tpcds_data_dir, "SCALE_FACTOR");

    // If we have previosuly generated data at the same scale, skip data generation
    if libscail::file_exists(guest_shell, &scale_factor_file)? {
        let existing_scale: String = guest_shell.run(cmd!("cat {}", &scale_factor_file))?.stdout;
        let existing_scale: u64 = existing_scale.trim().parse().unwrap();
        if existing_scale == scale {
            println!(
                "TPC-DS data already exists at scale {}, skipping generation",
                scale
            );
            return Ok(());
        }
    }

    // Generate TPC-DS data
    guest_shell.run(cmd!("mkdir -p {}", &tpcds_data_dir))?;
    guest_shell.run(
        cmd!(
            "./dsdgen -dir {} -scale {} -terminate N -force Y",
            &tpcds_data_dir,
            scale
        )
        .cwd(&tpcds_tools_dir),
    )?;

    // Tell spark to create the database
    guest_shell.run(cmd!(
        "{} -e \"DROP DATABASE IF EXISTS tpcds CASCADE; CREATE DATABASE tpcds;\"",
        spark_sql_bin
    ))?;
    guest_shell.run(cmd!(
        "{} -d tpcds -f {}/create_tables.sql",
        spark_sql_bin,
        &tpcds_ddl_dir,
    ))?;

    // Make sure we update the scale factor file
    guest_shell.run(cmd!("echo {} > {}", scale, &scale_factor_file))?;
    Ok(())
}
