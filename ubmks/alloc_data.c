#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <stdbool.h>

int main(int argc, char *argv[]) {
    struct timespec stop, start;
    size_t size;
    int64_t alloc_time_nsec;
    time_t alloc_time_sec;
    time_t alloc_ms_remainder;
    bool wait = true;

    if (argc != 2 && argc != 3) {
        fprintf(stderr, "Usage: %s <size in GB> [don't wait]\n", argv[0]);
        return -1;
    }
    size = (size_t)atoll(argv[1]) * 1024 * 1024 * 1024;
    if (argc == 3) {
        wait = false;
    }

    // Initial allocations are a bit slow.
    // So do a dummy allocation to get it out of the way.
    void *tmp = mmap(NULL, 1024 * 1024 * 1024, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
    if (tmp == MAP_FAILED) {
        perror("Initial dummy mmap failed");
        return -1;
    }
    munmap(tmp, 1024 * 1024 * 1024);

    clock_gettime(CLOCK_MONOTONIC, &start);
    void *ptr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
    if (ptr == MAP_FAILED) {
        perror("mmap failed");
        return -1;
    }

    clock_gettime(CLOCK_MONOTONIC, &stop);

    alloc_time_nsec = (int64_t)(stop.tv_sec - start.tv_sec) * 1000000000 +
        (stop.tv_nsec - start.tv_nsec);
    alloc_time_sec = alloc_time_nsec / 1000000000;
    alloc_ms_remainder = (alloc_time_nsec % 1000000000) / 1000000;

    // Keep the memory allocated for a while to allow inspection
    printf("Allocated %zu bytes of memory. In %ld.%03ld seconds\n", size,
        alloc_time_sec, alloc_ms_remainder);
    fflush(stdout);
    if (wait) {
        getchar();
    }

    return 0;
}
