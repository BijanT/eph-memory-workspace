use std::net::ToSocketAddrs;

/// Configure fresh CloudLab machine, install dependencies, and setup VMs
use clap::{ArgAction, ArgMatches, arg};
use libscail::{
    GitRepo, KernelBaseConfigSource, KernelConfig, KernelSrc, Login, ScailError, clone_git_repo,
    dir, get_user_home_dir,
};
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
    let host_shell = SshShell::with_any_key(login.username, &login.host)?;

    if cfg.resize_root {
        libscail::resize_root_partition(&host_shell)?;
    }

    install_host_dependencies(&host_shell)?;
    clone_research_workspace(&host_shell, cfg)?;

    // The user needs to be in the KVM and libvirt groups to run VMs.
    host_shell.run(cmd!("sudo usermod -aG kvm {}", login.username))?;
    host_shell.run(cmd!("sudo usermod -aG libvirt {}", login.username))?;
    // Reconnect to apply group changes and uninstalled AppArmor.
    let host_shell = crate::reboot_and_connect(login)?;

    setup_guest_vms(&host_shell, &login.host, cfg)?;

    host_shell.run(cmd!("virsh -c {} list --all", crate::LIBVIRT_URI))?;
    host_shell.run(cmd!("echo DONE"))?;

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
        "libz-dev",
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
        "cloud-utils",
        "libvirt-daemon-system",
        "virtiofsd",
        "libdw-dev",
        "libdebuginfod-dev",
        "systemtap-sdt-dev",
        "llvm-dev",
        "liblzma-dev",
        "libbabeltrace-dev",
        "libpfm4-dev",
        "libtraceevent-dev",
    ];
    ushell.run(cmd!("sudo apt install -y {}", apt_packages.join(" ")))?;

    // AppArmor may interfere with some experiments, so uninstall it.
    ushell.run(cmd!("sudo apt remove -y apparmor"))?;

    libscail::install_rust(ushell)?;

    Ok(())
}

fn install_guest_dependencies(ushell: &SshShell) -> Result<(), ScailError> {
    ushell.run(cmd!("sudo apt update; sudo apt upgrade -y"))?;

    let apt_packages = [
        "build-essential",
        "libssl-dev",
        "libelf-dev",
        "libncurses-dev",
        "libevent-dev",
        "libz-dev",
        "dwarves",
        "numactl",
        "linux-tools-common",
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
        "llvm",
        "clang",
        "llvm-dev",
        "libdw-dev",
        "libdebuginfod-dev",
        "systemtap-sdt-dev",
        "liblzma-dev",
        "libbabeltrace-dev",
        "libpfm4-dev",
        "libtraceevent-dev",
    ];
    ushell.run(cmd!("sudo apt install -y {}", apt_packages.join(" ")))?;

    // Clone FlameGraph
    let flamegraph_repo = GitRepo::HttpsPublic {
        repo: "github.com/brendangregg/FlameGraph.git",
    };
    clone_git_repo(ushell, flamegraph_repo, None, None, &[])?;

    Ok(())
}

fn clone_research_workspace(ushell: &SshShell, cfg: &Config) -> Result<(), ScailError> {
    const SUBMODULES: &[&str] = &["bpftool", "libbpf", "libscail"];
    let user_home = get_user_home_dir(ushell)?;
    let wkspc_dir = dir!(user_home, crate::WKSPC_DIR);
    let user = cfg.git_user.unwrap();
    let secret = cfg.secret.unwrap();
    let branch = cfg.wkspc_branch;

    let wkspc_repo = GitRepo::HttpsPrivate {
        repo: "github.com/BijanT/eph-memory-workspace.git",
        username: user,
        secret,
    };

    clone_git_repo(ushell, wkspc_repo, Some(&wkspc_dir), branch, SUBMODULES)?;

    ushell.run(cmd!("make").cwd(dir!(&wkspc_dir, "ubmks")))?;

    Ok(())
}

fn setup_guest_vms<A: ToSocketAddrs>(
    ushell: &SshShell,
    host: A,
    cfg: &Config,
) -> Result<(), ScailError> {
    let user_home = get_user_home_dir(ushell)?;
    // Where the results will be stored on the host
    let results_dir = dir!(&user_home, crate::RESULTS_DIR);
    // Where the guest kernel source code is located
    let guest_kernel_dir = dir!(&user_home, crate::GUEST_KERNEL_DIR);
    // Directory that contains the files used to define the VMs
    let vm_info_dir = dir!(&user_home, crate::WKSPC_DIR, "vms");
    // Directory to store VM disk images
    let imgs_dir = dir!(&user_home, crate::IMGS_DIR);
    // Directory to store the complete VM domain XML files
    let domains_dir = dir!(&user_home, crate::DOMAINS_DIR);
    // List of VM's to setup
    let vms_list = [("balloon_vm", 48), ("hotplug_vm", 48)];
    // Strings to replace in the domain XML templates
    let template_replace_from = [
        "\\[GUEST_KERNEL\\]",
        "\\[GUEST_INITRD\\]",
        "\\[QEMU\\]",
        "\\[GUEST_DISK_IMAGE\\]",
        "\\[CLOUD_INIT_IMAGE\\]",
        "\\[HOST_RESULTS_DIR\\]",
        "\\[GUEST_KERNEL_DIR\\]",
    ];

    let initramfs_path = dir!(&vm_info_dir, "initramfs.cpio");
    let qemu_path = "/usr/bin/qemu-system-x86_64";
    let guest_kernel_bin = build_guest_kernel(ushell, &guest_kernel_dir, cfg)?;

    ushell.run(cmd!("mkdir -p {}", imgs_dir))?;
    ushell.run(cmd!("mkdir -p {}", domains_dir))?;
    ushell.run(cmd!("mkdir -p {}", results_dir))?;

    // Create the VM images
    let cloud_init_img_path = create_cloud_init_img(ushell, &user_home, &vm_info_dir, &imgs_dir)?;
    let ubuntu_img_path = create_ubuntu_img(ushell, &imgs_dir)?;

    // Do setup for each VM
    for (vm_name, _size_gb) in vms_list.iter() {
        // The domain template file for this VM
        let domain_template_path = dir!(&vm_info_dir, format!("{}.xml", vm_name));
        // The final domain XML file for this VM
        let domain_xml_path = dir!(&domains_dir, format!("{}.xml", vm_name));

        // Now that we have all the files we need for the VM, we can create the domain XML
        let template_replace_to = [
            &guest_kernel_bin,
            &initramfs_path,
            qemu_path,
            &ubuntu_img_path,
            &cloud_init_img_path,
            &results_dir,
            &guest_kernel_dir,
        ];
        let sed_cmd = gen_sed_replace_cmd(&template_replace_from, &template_replace_to);
        ushell.run(cmd!(
            "sed '{}' {} | tee {}",
            sed_cmd,
            domain_template_path,
            domain_xml_path
        ))?;

        // Undefine any existing VM with the same name
        ushell.run(cmd!("virsh -c {} undefine {}", crate::LIBVIRT_URI, vm_name).allow_error())?;
        // Define the VM using the generated domain XML
        ushell.run(cmd!(
            "virsh -c {} define {}",
            crate::LIBVIRT_URI,
            domain_xml_path
        ))?;
    }

    // Install dependencies and workloads on the disk image that is shared by
    // all VMs
    let (vm_name, vm_size_gb) = vms_list[0];
    // Reserve enough HugeTLB pages for the VM
    ushell.run(cmd!("echo {} | sudo tee /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages",
        vm_size_gb * 512))?;
    let vm_shell = crate::start_and_connect_to_vm(ushell, vm_name, &host,
        crate::START_NAT_PORT, None)?;
    // Install dependencies and clone workspace inside the VM
    install_guest_dependencies(&vm_shell)?;
    clone_research_workspace(&vm_shell, cfg)?;
    let guest_home = get_user_home_dir(&vm_shell)?;
    let guest_wkspc_dir = dir!(&guest_home, crate::WKSPC_DIR);
    vm_shell.run(cmd!("make").cwd(dir!(&guest_wkspc_dir, "bpftool", "src")))?;
    vm_shell.run(cmd!("make").cwd(dir!(&guest_wkspc_dir, "bpf")))?;

    // Mount the guest kernel source so we can install perf
    let kernel_mnt_dir = "/mnt/guest_kernel";
    let perf_path = dir!(kernel_mnt_dir, "tools", "perf");
    vm_shell.run(cmd!("sudo chown -R $USER /mnt/").allow_error())?;
    vm_shell.run(cmd!("mkdir -p {}", kernel_mnt_dir))?;
    vm_shell.run(cmd!("sudo mount -t virtiofs guest_kernel_dir {}", kernel_mnt_dir))?;
    vm_shell.run(cmd!("sudo chown -R $USER {}", kernel_mnt_dir))?;
    vm_shell.run(cmd!("sudo cp perf /usr/bin/").cwd(&perf_path))?;

    // Shutdown the VM after setup is complete
    ushell.run(cmd!("virsh -c {} shutdown {}", crate::LIBVIRT_URI, vm_name))?;

    // After being shared to the VM, the libvirt user seems to own the
    // kernel directory on the host. Change ownership back to the user, so
    // that this script can run again fine later.
    ushell.run(cmd!("sudo chown -R $USER {}", guest_kernel_dir))?;

    Ok(())
}

fn create_cloud_init_img(
    ushell: &SshShell,
    user_home: &str,
    vm_info_dir: &str,
    imgs_dir: &str,
) -> Result<String, ScailError> {
    let cloud_init_iso_path = dir!(imgs_dir, "cloud_init.iso");
    let user_data_template_path = dir!(vm_info_dir, "cloud.yaml");
    let user_data_path = dir!("/tmp", "cloud-user-data.yaml");
    let net_data_path = dir!(vm_info_dir, "cloud-net.yaml");

    // Replace the SSH public key placeholder in the user data template file
    // with all of the keys in the remote's ~/.ssh/authorized_keys file.
    // Valid keys in the file are of the format "ssh-<key type> <key> <comment>"
    let ssh_keys = ushell
        .run(cmd!("cat {}/.ssh/authorized_keys", user_home))?
        .stdout
        .lines()
        .filter(|line| line.trim().starts_with("ssh"))
        .collect::<Vec<&str>>()
        .join("\\n      - ");
    ushell.run(cmd!(
        "sed 's|\\[SSH_KEY\\]|{}|g' {} | tee {}",
        ssh_keys,
        user_data_template_path,
        user_data_path
    ))?;

    // Validate the generated user data file
    ushell.run(cmd!("cloud-init schema -c {}", user_data_path))?;

    // Delete existing cloud init ISO if it exists
    ushell.run(cmd!("rm -f {}", cloud_init_iso_path))?;
    ushell.run(cmd!(
        "cloud-localds -v --network-config={} {} {}",
        net_data_path,
        cloud_init_iso_path,
        user_data_path
    ))?;

    Ok(cloud_init_iso_path)
}

/// Given a list of strings to replace in a file and a list of their replacements in order,
/// return a `sed` command that performs all the replacements.
fn gen_sed_replace_cmd(replace_from: &[&str], replace_to: &[&str]) -> String {
    let mut sed_cmd = String::new();
    for (from, to) in replace_from.iter().zip(replace_to.iter()) {
        sed_cmd.push_str(&format!("s|{}|{}|g;", from, to));
    }
    sed_cmd
}

fn create_ubuntu_img(
    ushell: &SshShell,
    imgs_dir: &str
) -> Result<String, ScailError> {
    let base_img_path = dir!(imgs_dir, "ubuntu_base.qcow2");
    let img_path = dir!(imgs_dir, "ubuntu_vm.qcow2");
    let ubuntu_img_url =
        "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img";

    // Download the base Ubuntu image if it does not already exist
    if !libscail::file_exists(ushell, &base_img_path)? {
        ushell.run(cmd!("wget -O {} {}", base_img_path, ubuntu_img_url))?;
    }

    // Delete any existing image for this VM
    ushell.run(cmd!("rm -f {}", img_path))?;

    // Create a copy of the base image for this VM
    ushell.run(cmd!("cp {} {}", base_img_path, img_path))?;

    // Increase the size of the image by 64GB
    ushell.run(cmd!("qemu-img resize {} +64G", img_path))?;

    Ok(img_path)
}

fn build_guest_kernel(
    ushell: &SshShell,
    guest_kernel_dir: &str,
    cfg: &Config,
) -> Result<String, ScailError> {
    let user = cfg.git_user.unwrap();
    let secret = cfg.secret.unwrap();
    let branch = cfg.guest_kernel_branch.unwrap_or("main");

    let guest_kernel_repo = GitRepo::HttpsPrivate {
        repo: "github.com/BijanT/linux_eph_memory.git",
        username: user,
        secret,
    };

    clone_git_repo(
        ushell,
        guest_kernel_repo,
        Some(guest_kernel_dir),
        Some(branch),
        &[],
    )?;

    let kernel_src = KernelSrc::Git {
        repo_path: guest_kernel_dir.to_string(),
        commitish: branch.into(),
    };
    // Most of these options are probably already set in the default config, but just to be sure.
    let config_options = [
        ("CONFIG_BLK_DEV_INITRD", true),
        ("CONFIG_PCI", true),
        ("CONFIG_BINFMT_ELF", true),
        ("CONFIG_SERIAL_8250", true),
        ("CONFIG_SERIAL_8250_CONSOLE", true),
        ("CONFIG_NET", true),
        ("CONFIG_PACKET", true),
        ("CONFIG_UNIX", true),
        ("CONFIG_INET", true),
        ("CONFIG_WIRELESS", false),
        ("CONFIG_WLAN", false),
        ("CONFIG_ATA", true),
        ("CONFIG_NETDEVICES", true),
        ("CONFIG_8139TOO", true),
        ("CONFIG_DEVTMPFS", true),
        ("CONFIG_TMPFS", true),
        ("CONFIG_HUGETLBFS", true),
        ("CONFIG_TRANSPARENT_HUGEPAGE", true),
        ("CONFIG_ISO9660_FS", true),
        ("CONFIG_EXT4_FS", true),
        ("CONFIG_VIRTIO", true),
        ("CONFIG_VIRTIO_PCI", true),
        ("CONFIG_VIRTIO_BALLOON", true),
        ("CONFIG_VIRTIO_NET", true),
        ("CONFIG_VIRTIO_BLK", true),
        ("CONFIG_FUSE_FS", true),
        ("CONFIG_VIRTIO_FS", true),
        ("CONFIG_MEMORY_HOTPLUG", true),
        ("CONFIG_MEMORY_HOTREMOVE", true),
        ("CONFIG_ACPI_HOTPLUG_MEMORY", true),
        ("CONFIG_MHP_DEFAULT_ONLINE_TYPE_ONLINE_MOVABLE", true),
        ("CONFIG_CXL_BUS", true),
        ("CONFIG_CXL_PCI", true),
        ("CONFIG_CXL_MEM", true),
        ("CONFIG_CXL_ACPI", true),
        ("CONFIG_CXL_FEATURES", true),
        ("CONFIG_BPF_JIT", true),
        ("CONFIG_BPF_SYSCALL", true),
        ("CONFIG_DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT", true),
        ("CONFIG_DEBUG_INFO_BTF", true),
    ];
    let kernel_config = KernelConfig {
        base_config: KernelBaseConfigSource::Defconfig,
        extra_options: &config_options,
    };

    let git_hash = libscail::get_git_hash(ushell, guest_kernel_dir)?;
    let local_version = libscail::gen_local_version(branch, &git_hash);

    let kernel_artifacts = libscail::build_kernel(
        ushell,
        kernel_src,
        kernel_config,
        Some(&local_version),
        libscail::KernelPkgType::BzImage,
        None,
        false,
    )?;

    // Compile perf tool
    let perf_path = dir!(guest_kernel_dir, "tools", "perf");
    ushell.run(cmd!("make").cwd(&perf_path))?;

    Ok(kernel_artifacts.pkg_path)
}
