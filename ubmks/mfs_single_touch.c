#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <unistd.h>
#include <stdbool.h>
#include <stdint.h>
#include <fcntl.h>
#include <time.h>

#define PAGE_SIZE 4096

int main(int argc, char *argv[]) {
    struct timeval stop, start;
    size_t size;
    char *mfs_dir;
    time_t alloc_time_usec;
    time_t alloc_time_sec;
    time_t alloc_ms_remainder;
    int fd;
    int open_flags = O_RDWR | O_EXCL | O_TMPFILE;
    int *ptr;
    int rnd;

    if (argc != 2) {
        fprintf(stderr, "Usage: %s <mfs dir>\n", argv[0]);
        return -1;
    }
    size = (size_t)PAGE_SIZE;
    mfs_dir = argv[1];

    srand(time(NULL));
    rnd = rand() % 1000000;
    printf("Random number: %d\n", rnd);

    fd = open(mfs_dir, open_flags, 0600);
    if (fd == -1) {
        perror("Failed to create temporary file in MFS");
        return -1;
    }

    if (ftruncate(fd, size)) {
        perror("Failed to truncate file!\n");
        return -1;
    }

    gettimeofday(&start, NULL);
    if (fallocate(fd, 0, 0, size)) {
        perror("fallocate failed");
        return -1;
    }
    ptr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (ptr == MAP_FAILED) {
        perror("mmap failed");
        return -1;
    }
    *ptr = 0; // Touch the page to ensure it's allocated

    gettimeofday(&stop, NULL);

    alloc_time_usec = (stop.tv_sec - start.tv_sec) * 1000000 +
        (stop.tv_usec - start.tv_usec);
    alloc_time_sec = alloc_time_usec / 1000000;
    alloc_ms_remainder = (alloc_time_usec % 1000000) / 1000;

    // Keep the memory allocated for a while to allow inspection
    printf("Allocated %zu bytes of memory. In %ld.%ld seconds %p. Waiting\n", size,
        alloc_time_sec, alloc_ms_remainder, ptr);

    getchar();

    *ptr = rnd;
    printf("Memory verification: Expected %d, got %d\n", rnd, *ptr);
    return 0;
}
