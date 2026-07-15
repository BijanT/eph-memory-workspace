#include <csetjmp>
#include <csignal>
#include <cstdint>
#include <cstdlib>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <memory>
#include <mutex>
#include <string>
#include <sys/mman.h>
#include <unistd.h>

#include "libephmem.h"

static std::once_flag libephmem_initialized;
static const char *EPHMFS_DIR_ENV_VAR = "EPHMFS_DIR";
static std::string libephmfs_ephmfs_dir = "/mnt/ephmfs";

struct libephmem_file {
	int fd;
	size_t size;
	void *ptr;
};

struct libephmem_handle {
	void *ptr;
	size_t size;
	std::unique_ptr<struct libephmem_file> file;
};

struct libephmem_attempt_context {
	sigjmp_buf env;
	struct libephmem_handle *handle;
	volatile sig_atomic_t in_attempt;
};
static thread_local struct libephmem_attempt_context cur_attempt_context;

static bool addr_in_range(void *addr, struct libephmem_handle *handle) {
	uintptr_t addr_int = reinterpret_cast<uintptr_t>(addr);
	uintptr_t handle_start = reinterpret_cast<uintptr_t>(handle->ptr);
	return addr_int >= handle_start && addr_int < (handle_start + handle->size);
}

static void libephmem_sig_handler(int signum, siginfo_t *info, void *) {
	bool in_attempt = cur_attempt_context.in_attempt;
	bool sync_sigbus = info->si_code == BUS_MCEERR_AR
		|| info->si_code == BUS_ADRERR || info->si_code == BUS_OBJERR;

	/* Recoverable SIGBUS in attempt context. Jump to the recovery address */
	if (in_attempt && sync_sigbus && addr_in_range(info->si_addr, cur_attempt_context.handle)) {
		siglongjmp(cur_attempt_context.env, 1);
	}

	/* We can't handle the signal. Perform the default action. */
	signal(signum, SIG_DFL);
	return;
}

static void libephmem_init() {
	char *ephmfs_dir;
	struct sigaction sa = {};

	ephmfs_dir = std::getenv(EPHMFS_DIR_ENV_VAR);
	if (ephmfs_dir != nullptr) {
		libephmfs_ephmfs_dir = std::string(ephmfs_dir);
	}

	sa.sa_sigaction = libephmem_sig_handler;
	sa.sa_flags = SA_SIGINFO;
	sigemptyset(&sa.sa_mask);
	if (sigaction(SIGBUS, &sa, nullptr) == -1) {
		perror("Failed to install signal handler");
	}
}

void *libephmem_ptr(struct libephmem_handle *handle) {
	return handle->ptr;
}

size_t libephmem_size(struct libephmem_handle *handle) {
	return handle->size;
}

static std::unique_ptr<struct libephmem_file> libephmem_create_file(size_t size) {
	int fd;
	int open_flags = O_RDWR | O_EXCL | O_TMPFILE;
	std::unique_ptr<struct libephmem_file> file = std::make_unique<struct libephmem_file>();

	fd = open(libephmfs_ephmfs_dir.c_str(), open_flags, 0600);
	if (fd == -1) {
		perror("Failed to create temporary file in EphMFS");
		return nullptr;
	}

	if (ftruncate(fd, size)) {
		perror("Failed to truncate file!\n");
		close(fd);
		return nullptr;
	}

	file->ptr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (file->ptr == MAP_FAILED) {
		perror("mmap failed");
		close(fd);
		return nullptr;
	}

	file->fd = fd;
	file->size = size;
	return file;
}

/*
 * We return a raw pointer here instead of a unique_ptr to be more generic for
 * different programming languages.
 * The pointer will be freed by libephmem_free() when the user is done with it.
 */
struct libephmem_handle *libephmem_alloc(size_t size) {
	struct libephmem_handle *handle;

	std::call_once(libephmem_initialized, libephmem_init);

	handle = new struct libephmem_handle();
	handle->file = libephmem_create_file(size);
	if (handle->file == nullptr) {
		delete handle;
		return nullptr;
	}
	handle->ptr = handle->file->ptr;
	handle->size = size;
	return handle;
}

void libephmem_free(struct libephmem_handle *handle) {
	if (handle == nullptr) {
		return;
	}

	if (handle->file != nullptr) {
		munmap(handle->file->ptr, handle->file->size);
		close(handle->file->fd);
	}

	delete handle;
}

int libephmem_attempt(struct libephmem_handle *handle, libephmem_attempt_fn fn,
		      void *args) {
	if (handle == nullptr || fn == nullptr) {
		fprintf(stderr, "libephmem_attempt: NULL handle or function pointer\n");
		return -1;
	}
	if (cur_attempt_context.in_attempt) {
		fprintf(stderr, "libephmem_attempt: Already in attempt context\n");
		return -1;
	}

	if (!sigsetjmp(cur_attempt_context.env, 1)) {
		cur_attempt_context.handle = handle;
		cur_attempt_context.in_attempt = 1;
		fn(handle->ptr, handle->size, args);

		cur_attempt_context.in_attempt = 0;
		return 0;
	}

	/* The attempt failed */
	cur_attempt_context.in_attempt = 0;
	return 1;
}

int libephmem_put(const void *src, struct libephmem_handle *dst, size_t offset,
		  size_t len) {
	if (offset > dst->size || len > dst->size - offset) {
		fprintf(stderr, "libephmem_put: Attempt to write beyond allocated memory\n");
		return -1;
	}

	return libephmem_attempt(dst, [&offset, &src, &len](void *dst_ptr, size_t) {
		memcpy(static_cast<char *>(dst_ptr) + offset, src, len);
	});
}

int libephmem_get(struct libephmem_handle *src, void *dst, size_t offset,
		  size_t len) {
	if (offset > src->size || len > src->size - offset) {
		fprintf(stderr, "libephmem_get: Attempt to read beyond allocated memory\n");
		return -1;
	}

	return libephmem_attempt(src, [&offset, &dst, &len](void *src_ptr, size_t) {
		memcpy(dst, static_cast<char *>(src_ptr) + offset, len);
	});
}
