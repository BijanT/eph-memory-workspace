#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <unistd.h>
#include <stdbool.h>
#include <fcntl.h>

#define PAGE_SIZE 4096

int main(int argc, char *argv[]) {
    struct timeval stop, start;
    size_t size;
    char *mfs_dir;
    time_t alloc_time_usec;
    time_t alloc_time_sec;
    time_t alloc_ms_remainder;
    int fd;
    int open_flags = O_RDWR | O_CREAT | O_TRUNC | O_TMPFILE;

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

    // Initial allocations are a bit slow.
    // So do a dummy allocation to get it out of the way.
    void *tmp = mmap(NULL, 1024 * 1024 * 1024, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, fd, 0);
    if (tmp == MAP_FAILED) {
        perror("Initial dummy mmap failed");
        return -1;
    }
    munmap(tmp, 1024 * 1024 * 1024);

    gettimeofday(&start, NULL);
    void *ptr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, fd, 0);
    if (ptr == MAP_FAILED) {
        perror("mmap failed");
        return -1;
    }

    gettimeofday(&stop, NULL);

    alloc_time_usec = (stop.tv_sec - start.tv_sec) * 1000000 +
        (stop.tv_usec - start.tv_usec);
    alloc_time_sec = alloc_time_usec / 1000000;
    alloc_ms_remainder = (alloc_time_usec % 1000000) / 1000;

    // Keep the memory allocated for a while to allow inspection
    printf("Allocated %zu bytes of memory. In %ld.%ld seconds\n", size,
        alloc_time_sec, alloc_ms_remainder);

    for (unsigned long i = 0; i < size; i += 4096) {
	((char *)ptr)[i] = i % sizeof(char);
    }

    return 0;
}
