#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char *argv[]) {
    struct timeval stop, start;
    size_t size;
    size_t proc_size;
    pid_t *pids;
    int num_procs = 1;
    time_t alloc_time_usec;
    time_t alloc_time_sec;
    time_t alloc_ms_remainder;

    if (argc != 3) {
        fprintf(stderr, "Usage: %s <size in GB> <procs> \n", argv[0]);
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

    gettimeofday(&start, NULL);

    for (int i = 0; i < num_procs; i++) {
        pids[i] = fork();
        if (pids[i] == 0) {
            // Child
            void *ptr = mmap(NULL, proc_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
            if (ptr == MAP_FAILED) {
                perror("mmap failed");
                return -1;
            }

            return 0;
        } else if (pids[i]) {
            // Parent
            continue;
        } else {
            // Error
            fprintf(stderr, "Error forking! %d\n", errno);
            return -1;
        }
    }

    for (int i = 0; i < num_procs; i++) {
        waitpid(pids[i], NULL, 0);
    }
    gettimeofday(&stop, NULL);

    alloc_time_usec = (stop.tv_sec - start.tv_sec) * 1000000 +
        (stop.tv_usec - start.tv_usec);
    alloc_time_sec = alloc_time_usec / 1000000;
    alloc_ms_remainder = (alloc_time_usec % 1000000) / 1000;

    // Keep the memory allocated for a while to allow inspection
    printf("Allocated %zu bytes of memory. In %ld.%ld seconds\n", size,
        alloc_time_sec, alloc_ms_remainder);

    return 0;
}
