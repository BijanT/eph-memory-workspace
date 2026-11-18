#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <stdbool.h>
#include <stdint.h>
#include <fcntl.h>

#define PAGE_SIZE 4096

int main(int argc, char *argv[]) {
    struct timespec stop, start;
    size_t size;
    char *mfs_dir;
    int64_t alloc_time_nsec;
    time_t alloc_time_sec;
    time_t alloc_ms_remainder;
    int fd;
    int open_flags = O_RDWR | O_EXCL | O_TMPFILE;

    if (argc != 3) {
        fprintf(stderr, "Usage: %s <size in pages> <mfs dir>\n", argv[0]);
        return -1;
    }
    size = (size_t)atoll(argv[1]) * PAGE_SIZE;
    mfs_dir = argv[2];

    fd = open(mfs_dir, open_flags, 0600);
    if (fd == -1) {
        perror("Failed to create temporary file in MFS");
        return -1;
    }

    if (ftruncate(fd, size)) {
        perror("Failed to truncate file!\n");
        return -1;
    }

    // Initial allocations are a bit slow.
    // So do a dummy allocation to get it out of the way.
    void *tmp = mmap(NULL, 1024 * 1024 * 1024, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, fd, 0);
    if (tmp == MAP_FAILED) {
        perror("Initial dummy mmap failed");
        return -1;
    }
    munmap(tmp, 1024 * 1024 * 1024);

    clock_gettime(CLOCK_MONOTONIC, &start);
    if (fallocate(fd, 0, 0, size)) {
        perror("fallocate failed");
        return -1;
    }
    void *ptr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
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
    printf("Allocated %zu bytes of memory. In %ld.%03ld seconds %p\n", size,
        alloc_time_sec, alloc_ms_remainder, ptr);

    while (1) {
        for (unsigned long i = 0; i < size; i += 4096) {
            ((uint8_t *)ptr)[i] = (i / 4096) % 256;
        }
        for (unsigned long i = 0; i < size; i += 4096) {
            if (((uint8_t*)ptr)[i] != (i / 4096) % 256) {
                printf("Mismatch at %ld: %d should be %ld\n",
                    i, ((uint8_t*)ptr)[i], (i / 4096) % 256);
            }
        }
        sleep(1);
    }

    return 0;
}
