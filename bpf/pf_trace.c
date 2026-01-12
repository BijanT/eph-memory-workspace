#include <stdio.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <bpf/libbpf.h>
#include "pf_trace.skel.h"

static int libbpf_print_fn(enum libbpf_print_level level,
			  const char *format, va_list args)
{
    return vfprintf(stderr, format, args);
}

int main(int argc, char **argv)
{
    struct pf_trace_bpf *skel;
    uint8_t *addr;
    int err;

    libbpf_set_print(libbpf_print_fn);

    skel = pf_trace_bpf__open_and_load();
    if (!skel) {
	fprintf(stderr, "Failed to open and load BPF skeleton\n");
	return 1;
    }

    skel->bss->trace_pid = getpid();

    err = pf_trace_bpf__attach(skel);
    if (err) {
	fprintf(stderr, "Failed to attach BPF skeleton\n");
	goto cleanup;
    }

    printf("Tracing handle_mm_fault... Hit Ctrl-C to end.\n");
    while (1) {
	addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
		    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (addr == MAP_FAILED) {
	    perror("mmap");
	    break;
	}
	*addr = 0;  // Trigger a page fault
	munmap(addr, 4096);
	sleep(1);
    }

cleanup:
    pf_trace_bpf__destroy(skel);
    return 0;
}