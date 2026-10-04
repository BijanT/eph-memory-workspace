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
#include <sys/socket.h>
#include <sys/un.h>
#include <poll.h>
#include <unistd.h>

#include "json.hpp"
#include "libephmem.h"

using json = nlohmann::json;

static std::once_flag libephmem_initialized;
static const ssize_t MAX_JSON_BUF = 1024;
static const char *EPHMFS_DIR_ENV_VAR = "EPHMFS_DIR";
static const char *EPHEMERALD_SOCK_ENV_VAR = "EPHEMERALD_PATH";
static std::string libephmfs_ephmfs_dir = "/mnt/ephmfs";
static std::string libephmfs_ephemerald_sock = "/run/ephemerald.sock";
static int ephemerald_sock_fd = -1;
static int ephmem_pkey = -1;
static std::mutex ephmem_mutex;
static bool libephmem_setup_succeeded = false;

// Definitions for the wire protocol between libephmem and ephemerald
static const char *EPHEMERALD_JSON_FUNC_KEY = "function";
static const char *EPHEMERALD_JSON_SIZE_KEY = "size";

enum EphemeraldFunction {
	EPHEMERALD_FUNC_REQUEST = 0,
	EPHEMERALD_FUNC_RESPONSE = 1,
	EPHEMERALD_FUN_MAX,
};

static const char *EphemeralFuncStrings[] = {
	"eph-mem-request",
	"eph-mem-response",
};

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

static bool libephmem_using_pkey() {
	return ephmem_pkey != -1;
}

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

static int connect_to_ephemerald(char *path) {
	int sock_fd;
	struct sockaddr_un addr;

	if (strlen(path) >= sizeof(addr.sun_path)) {
		fprintf(stderr, "Socket path is too long: %s\n", path);
		return -1;
	}

	sock_fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (sock_fd == -1) {
		perror("Failed to create socket");
		return -1;
	}

	memset(&addr, 0, sizeof(addr));
	addr.sun_family = AF_UNIX;
	strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);
	addr.sun_path[sizeof(addr.sun_path) - 1] = '\0';

	if (connect(sock_fd, (struct sockaddr *)&addr, sizeof(addr)) == -1) {
		perror("Failed to connect to ephemerald");
		close(sock_fd);
		return -1;
	}

	return sock_fd;
}

static void libephmem_init() {
	char *ephmfs_dir;
	char *ephemerald_sock;
	struct sigaction sa = {};

	ephmfs_dir = std::getenv(EPHMFS_DIR_ENV_VAR);
	if (ephmfs_dir != nullptr) {
		libephmfs_ephmfs_dir = std::string(ephmfs_dir);
	}

	ephemerald_sock = std::getenv(EPHEMERALD_SOCK_ENV_VAR);
	if (ephemerald_sock != nullptr) {
		libephmfs_ephemerald_sock = std::string(ephemerald_sock);
	}

	ephemerald_sock_fd = connect_to_ephemerald(libephmfs_ephemerald_sock.data());
	if (ephemerald_sock_fd == -1) {
		return;
	}

	sa.sa_sigaction = libephmem_sig_handler;
	sa.sa_flags = SA_SIGINFO;
	sigemptyset(&sa.sa_mask);
	if (sigaction(SIGBUS, &sa, nullptr) == -1) {
		perror("Failed to install signal handler");
	}

	/*
	 * Failure means either the CPU does not support protection keys or for
	 * some other reason, this process has run out. I don't think that's
	 * reason enough to fail, but we should at least print something to let
	 * the user know.
	 */
	ephmem_pkey = pkey_alloc(0, PKEY_DISABLE_ACCESS);
	if (ephmem_pkey == -1) {
		perror("Failed to allocate protection key for ephemeral memory");
		fprintf(stderr, "Proceeding without protection keys. Ephemeral memory"
			" will be accessible outside of attempt contexts.\n");
	}

	libephmem_setup_succeeded = true;
}

size_t libephmem_size(struct libephmem_handle *handle) {
	return handle->size;
}

static int libephmem_send(int sock, std::string &req) {
	ssize_t bytes_to_send = req.size();
	ssize_t total_bytes_sent = 0;
	while (bytes_to_send) {
		ssize_t bytes_sent = send(sock, &req.c_str()[total_bytes_sent],
					  bytes_to_send, MSG_NOSIGNAL);
		if (bytes_sent == -1) {
			if (errno == EINTR) {
				continue; // Interrupted by signal, retry
			}
			perror("Failed to send request to ephemerald");
			return -1;
		}
		bytes_to_send -= bytes_sent;
		total_bytes_sent += bytes_sent;
	}
	return 0;
}

static int libephmem_recv(int sock, int timeout_ms, char *buf, size_t buf_size) {
	struct pollfd pfd;
	size_t total_bytes_received = 0;

	pfd.fd = sock;
	pfd.events = POLLIN;


	while (total_bytes_received < buf_size - 1) {
		char *pos = buf + total_bytes_received;
		size_t remaining = buf_size - 1 - total_bytes_received;

		int poll_result = poll(&pfd, 1, timeout_ms);
		if (poll_result == -1) {
			if (errno == EINTR) {
				continue; // Interrupted by signal, retry
			}
			perror("Failed to poll socket");
			return -1;
		} else if (poll_result == 0) {
			fprintf(stderr, "Timeout while waiting for response from ephemerald\n");
			return -1;
		}

		if (!(pfd.revents & POLLIN)) {
			fprintf(stderr, "Unexpected poll event: %d\n", pfd.revents);
			return -1;
		}

		ssize_t n = recv(sock, pos, remaining, MSG_NOSIGNAL);
		if (n == -1) {
			// This shouldn't happen since we polled for
			// readability, but handle it just in case.
			if (errno == EINTR) {
				continue; // Interrupted by signal, retry
			}
			perror("Failed to receive response from ephemerald");
			return -1;
		} else if (n == 0) {
			fprintf(stderr, "Connection closed by ephemerald\n");
			return -1;
		}
		size_t bytes_received = static_cast<size_t>(n);

		// Search for newline character to determine end of message
		for (size_t i = 0; i < bytes_received; ++i) {
			if (pos[i] == '\n') {
				total_bytes_received += i;
				buf[total_bytes_received] = '\0'; // Null-terminate the string
				return 0;
			}
		}
		// Newline not found. Keep on reading
		total_bytes_received += bytes_received;
	}

	fprintf(stderr, "Buffer overflow while receiving response from ephemerald\n");
	return -1;
}

static int libephmem_drain(int sock) {
	char buf[MAX_JSON_BUF];
	while (true) {
		ssize_t n = recv(sock, buf, sizeof(buf), MSG_DONTWAIT | MSG_NOSIGNAL);
		if (n > 0) // Keep draining
			continue;
		else if (n == 0) { // Connection closed
			fprintf(stderr, "libephmem_drain: Connection closed by ephemerald\n");
			return -1;
		} else if (errno == EINTR) { // Interrupted by signal, retry
			continue;
		}
		else if (errno == EAGAIN || errno == EWOULDBLOCK) { // No more data to read
			break;
		}

		perror("Failed to drain socket");
		return -1;
	}
	return 0;
}

size_t libephmem_reserve(size_t amount) {
	char buffer[MAX_JSON_BUF];
	std::call_once(libephmem_initialized, libephmem_init);

	if (!libephmem_setup_succeeded) {
		fprintf(stderr, "libephmem_reserve: Initialization failed\n");
		return 0;
	}

	if (ephemerald_sock_fd == -1) {
		fprintf(stderr, "libephmem_reserve: Not connected to ephemerald\n");
		return 0;
	}

	// Can early out if the amount is zero
	if (amount == 0) {
		return 0;
	}

	// Round up the amount to the nearest multiple of LIBEPHMEM_RESERVATION_GRANULARITY
	// But first, check for overflow
	size_t granularity_mask = LIBEPHMEM_RESERVATION_GRANULARITY - 1;
	if (amount > SIZE_MAX - granularity_mask) {
		fprintf(stderr, "libephmem_reserve: Amount too large, would overflow\n");
		return 0;
	}
	amount = (amount + granularity_mask) & ~granularity_mask;

	// Ask ephemerald to reserve ephemeral memory for this process.
	json request = {
		{EPHEMERALD_JSON_FUNC_KEY, EphemeralFuncStrings[EPHEMERALD_FUNC_REQUEST]},
		{EPHEMERALD_JSON_SIZE_KEY, amount}
	};

	std::string request_str = request.dump();
	// Messages are newline delimited
	request_str.push_back('\n');

	// Take the mutex so message streams don't get mixed up.
	// Ephemerald only accepts one reserve request from a client at a time
	// anyway.
	std::lock_guard<std::mutex> lock(ephmem_mutex);

	// Drain any pending messages from ephemerald before sending a new
	// request. This is fragile because a late message can still come in
	// after we drain. A more robust solution would be to have a unique
	// ID per request, which is returned in the reponse.
	// We should probably eventually do that, but for now, this should work
	// with the current implementation of ephemerald.
	if (libephmem_drain(ephemerald_sock_fd) == -1) {
		return 0;
	}

	if (libephmem_send(ephemerald_sock_fd, request_str) == -1) {
		perror("Failed to send request to ephemerald");
		return 0;
	}

	// Wait for a response from ephemerald
	int result = libephmem_recv(ephemerald_sock_fd, 1000, buffer, MAX_JSON_BUF);
	if (result == -1) {
		return 0;
	}

	// Process the response from ephemerald.
	json response = json::parse(buffer, nullptr, false);
	if (response.is_discarded()) {
		fprintf(stderr, "Failed to parse response from ephemerald\n");
		return 0;
	}

	auto func_it = response.find(EPHEMERALD_JSON_FUNC_KEY);
	auto size_it = response.find(EPHEMERALD_JSON_SIZE_KEY);
	if (func_it == response.end() || size_it == response.end()) {
		fprintf(stderr, "Invalid response from ephemerald: missing required keys\n");
		return 0;
	} else if (!func_it->is_string()) {
		fprintf(stderr, "Invalid response from ephemerald: %s is not a string\n",
			EPHEMERALD_JSON_FUNC_KEY);
		return 0;
	} else if (!size_it->is_number_unsigned()) {
		fprintf(stderr, "Invalid response from ephemerald: %s is not an unsigned number\n",
			EPHEMERALD_JSON_SIZE_KEY);
		return 0;
	} else if (func_it->get<std::string>() != EphemeralFuncStrings[EPHEMERALD_FUNC_RESPONSE]) {
		fprintf(stderr, "Invalid response from ephemerald: unexpected function value %s\n",
			func_it->get<std::string>().c_str());
		return 0;
	}

	// We don't currently have a mechanism to reject requests, so just log
	// if the response size is larger than requested.
	size_t reserved_size = size_it->get<size_t>();
	if (reserved_size > amount) {
		fprintf(stderr, "Warning: ephemerald reserved more memory than requested: %zu >%zu\n",
			reserved_size, amount);
		// Maintain the contract that we return up to the requested amount
		reserved_size = amount;
	}

	return reserved_size;
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

	if (libephmem_using_pkey()) {
		int ret = pkey_mprotect(file->ptr, size, PROT_READ | PROT_WRITE,
			ephmem_pkey);
		if (ret == -1) {
			perror("pkey_mprotect failed");
			munmap(file->ptr, size);
			close(fd);
			return nullptr;
		}
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
	if (!libephmem_setup_succeeded) {
		fprintf(stderr, "libephmem_alloc: Initialization failed\n");
		return nullptr;
	}

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
	bool using_pkeys;

	if (handle == nullptr || fn == nullptr) {
		fprintf(stderr, "libephmem_attempt: NULL handle or function pointer\n");
		return -1;
	}
	if (cur_attempt_context.in_attempt) {
		fprintf(stderr, "libephmem_attempt: Already in attempt context\n");
		return -1;
	}

	using_pkeys = libephmem_using_pkey();
	if (using_pkeys) {
		if (pkey_set(ephmem_pkey, 0) == -1) {
			perror("Failed to enable access to ephemeral memory");
			return -1;
		}
	}

	if (!sigsetjmp(cur_attempt_context.env, 1)) {
		cur_attempt_context.handle = handle;
		cur_attempt_context.in_attempt = 1;

		fn(handle->ptr, handle->size, args);

		cur_attempt_context.in_attempt = 0;
		if (using_pkeys) {
			if (pkey_set(ephmem_pkey, PKEY_DISABLE_ACCESS) == -1) {
				perror("Failed to disable access to ephemeral memory");
				return -1;
			}
		}
		return 0;
	}

	/* The attempt failed */
	cur_attempt_context.in_attempt = 0;
	if (using_pkeys) {
		if (pkey_set(ephmem_pkey, PKEY_DISABLE_ACCESS) == -1) {
			perror("Failed to disable access to ephemeral memory");
			return -1;
		}
	}
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
