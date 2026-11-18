#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <time.h>
#include <sys/wait.h>
#include <unistd.h>

// Allocate proc_size bytes and report the result to the parent.
// Never returns. The child holds onto its memory until it is killed.
static void child(pid_t parent, int go_fd, int done_fd, size_t proc_size) {
    char status = 0;
    char c;
    int n;

    // Make sure the child does not outlive the parent and hold memory forever
    if (prctl(PR_SET_PDEATHSIG, SIGKILL) == -1) {
        perror("prctl failed");
        _exit(1);
    }
    // The parent may have died before prctl was called
    if (getppid() != parent) {
        _exit(1);
    }

    // Start barrier: the parent closes the write end once all children exist
    if ((n = read(go_fd, &c, 1)) != 0) {
        if (n == -1) {
            perror("read failed");
        } else {
            fprintf(stderr, "Unexpected data on start barrier\n");
        }
        _exit(1);
    }

    void *ptr = mmap(NULL, proc_size, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
    if (ptr == MAP_FAILED) {
        perror("mmap failed");
        status = 1;
    }

    // Report completion and close the pipe so that the parent sees EOF once
    // every child has either reported or died.
    if (write(done_fd, &status, 1) != 1) {
        perror("write failed");
    }
    close(go_fd);
    close(done_fd);

    // Hold onto the memory until the parent kills us
    for (;;) {
        pause();
    }
}

// Kill and reap the first num children in pids
static void kill_children(pid_t *pids, int num) {
    for (int i = 0; i < num; i++) {
        kill(pids[i], SIGKILL);
    }
    for (int i = 0; i < num; i++) {
        waitpid(pids[i], NULL, 0);
    }
}

int main(int argc, char *argv[]) {
    struct timespec stop, start;
    size_t size;
    size_t proc_size;
    pid_t *pids;
    pid_t parent;
    int num_procs = 1;
    int go_pipe[2];
    int done_pipe[2];
    int reported = 0;
    int failed = 0;
    int ret = 0;
    char status;
    ssize_t n;
    int64_t alloc_time_nsec;
    time_t alloc_time_sec;
    time_t alloc_ms_remainder;

    if (argc != 3) {
        fprintf(stderr, "Usage: %s <size in GB> <procs>\n", argv[0]);
        return -1;
    }
    size = (size_t)atoll(argv[1]) * 1024 * 1024 * 1024;
    num_procs = atoi(argv[2]);

    if (num_procs < 1) {
        fprintf(stderr, "procs must be positive\n");
        return -1;
    }

    proc_size = size / num_procs;

    pids = malloc(sizeof(pid_t) * num_procs);
    if (!pids) {
        fprintf(stderr, "malloc error\n");
        return -1;
    }

    if (pipe(go_pipe) == -1 || pipe(done_pipe) == -1) {
        perror("pipe failed");
        return -1;
    }

    parent = getpid();
    // Flush before forking so buffered output is not duplicated in children
    fflush(stdout);

    for (int i = 0; i < num_procs; i++) {
        pids[i] = fork();
        if (pids[i] == 0) {
            // Child
            close(go_pipe[1]);
            close(done_pipe[0]);
            child(parent, go_pipe[0], done_pipe[1], proc_size);
        } else if (pids[i] < 0) {
            fprintf(stderr, "Error forking! %d\n", errno);
            kill_children(pids, i);
            return -1;
        }
    }

    close(go_pipe[0]);
    close(done_pipe[1]);

    // Release all of the children at once
    clock_gettime(CLOCK_MONOTONIC, &start);
    close(go_pipe[1]);

    // Each child writes one status byte. Stop the clock once every child has
    // reported. EOF before then means some children died without reporting.
    while (reported < num_procs && (n = read(done_pipe[0], &status, 1)) == 1) {
        reported++;
        if (status != 0) {
            failed++;
        }
    }
    clock_gettime(CLOCK_MONOTONIC, &stop);
    close(done_pipe[0]);

    if (n < 0) {
        perror("read failed");
        ret = -1;
    } else if (reported != num_procs || failed != 0) {
        fprintf(stderr, "%d of %d children failed to allocate\n",
                num_procs - reported + failed, num_procs);
        ret = -1;
    }

    if (ret == 0) {
        alloc_time_nsec = (int64_t)(stop.tv_sec - start.tv_sec) * 1000000000 +
            (stop.tv_nsec - start.tv_nsec);
        alloc_time_sec = alloc_time_nsec / 1000000000;
        alloc_ms_remainder = (alloc_time_nsec % 1000000000) / 1000000;

        printf("Allocated %zu bytes of memory. In %ld.%03ld seconds\n", size,
            alloc_time_sec, alloc_ms_remainder);
        fflush(stdout);
    }

    kill_children(pids, num_procs);
    free(pids);

    return ret;
}
