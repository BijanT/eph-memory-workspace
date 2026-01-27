#include <stdatomic.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <bpf/libbpf.h>
#include "pf_trace.h"
#include "pf_trace.skel.h"

const char* STOP_FILE = "/tmp/stop_pf_trace";
atomic_bool printing = false;

static int libbpf_print_fn(enum libbpf_print_level level,
                const char *format, va_list args)
{
    return vfprintf(stderr, format, args);
}

static int handle_event(void *ctx, void *data, size_t data_sz)
{
    struct pf_trace_event *event = data;

    printing = true;
    printf("%lu,%x,%u\n",
        event->fault_time_ns,
        event->flags,
        event->huge_fault);
    printing = false;

    return 0;
}

int main(int argc, char **argv)
{
    struct ring_buffer *rb = NULL;
    struct pf_trace_bpf *skel;
    char *comm;
    int pid = 0;
    int err = 0;

    if (argc != 2) {
        fprintf(stderr, "Usage: %s <comm/pid>\n", argv[0]);
        return 1;
    }
    comm = argv[1];

    if (strlen(comm) >= TASK_COMM_LEN) {
        fprintf(stderr, "Command name too long (max %d characters)\n", TASK_COMM_LEN - 1);
        return 1;
    }

    libbpf_set_print(libbpf_print_fn);

    skel = pf_trace_bpf__open();
    if (!skel) {
        fprintf(stderr, "Failed to open BPF skeleton\n");
        return 1;
    }

    pid = atoi(comm);
    if (pid == 0) {
        // Set target command name
        strncpy(skel->rodata->target_comm, comm, TASK_COMM_LEN);
    } else {
        // Set target PID
        skel->bss->trace_tgid = pid;
    }

    err = pf_trace_bpf__load(skel);
    if (err) {
        fprintf(stderr, "Failed to load BPF skeleton\n");
        goto cleanup;
    }

    err = pf_trace_bpf__attach(skel);
    if (err) {
        fprintf(stderr, "Failed to attach BPF skeleton\n");
        goto cleanup;
    }

    // Set up the ring buffer
    rb = ring_buffer__new(bpf_map__fd(skel->maps.rb), handle_event, NULL, NULL);
    if (!rb) {
        err = -1;
        fprintf(stderr, "Failed to create ring buffer\n");
        goto cleanup;
    }

    printf("fault_ns, flags (hex), huge_fault\n");
    while (access(STOP_FILE, F_OK) == -1) {
        err = ring_buffer__poll(rb, 100);
        if (err == -EINTR) {
            break;
        } else if (err < 0) {
            fprintf(stderr, "Error polling ring buffer: %d\n", err);
            break;
        }
    }

    // Wait so we don't interrupt the output
    while (printing) {
        usleep(100);
    }
    fflush(stdout);

cleanup:
    ring_buffer__free(rb);
    pf_trace_bpf__destroy(skel);
    return err;
}