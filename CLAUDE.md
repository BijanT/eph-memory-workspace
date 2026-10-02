This repository represents the bulk of the code for a research project called "Ephemeral Memory."
It is composed of the following directories (excluding submodules):
    * bpf: Code for various eBPF helper/test programs.
    * eph_orchestrator: The ephemeral memory orchestrator, which coordinates between "consumer" VMs who want ephemeral memory and "donor" VMs whose unused memory is stolen for ephemeral memory.
    * eph_proto: Wire protocol for communication between eph_orchestrator and ephemerald (and eventually ephemerald and libephmem)
    * ephemerald: Daemon run by consumer VMs to act as a centralized location to communicate with the orchestrator
    * libephmem: Software library to make it easier for applications to use ephemeral memory
    * runner: Experiment runner program
    * scripts: Various experiment result processing and plotting scripts
    * ubmks: Microbenchmarks to testing ephemeral memory
    * vms: Files for defining libvirt VMs, and manually running VMs
    * workloads: Potential workloads with which to evaluate ephemeral memory

The submodules are bpftool, libbpf, and libscail.
You should not edit the submodules unless very explicitly asked.

If you want more context on the design of ephemeral memory, read design.md

Agents will commonly be asked to review commits via the /code-review skill.
Please list all findings clearly, and give each finding a short identifier that is unique to each invocation of the /code-review skill so that findings can be referenced easily (though they can repeat across invocations).
The identifiers should be increasing numbers, beginning with 1, if only reviewing one commit.
If reviewing multiple commits, the findings for each commit should begin with a capital letter, ascending in commit order and they should end with the finding number, beginning with 1 for each commit (e.g., the first finding of the oldest commit is A1, and the third finding of the second oldest commit is B3).
List findings per commit, in commit order.

When reviewing changes to Rust source, verify that `cargo fmt --check` and `cargo clippy` come back clean.

Do not edit files unless explicitly asked to do so.
Suggest changes as text first and wait for approval before editing.
