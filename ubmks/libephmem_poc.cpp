#include <algorithm>
#include <atomic>
#include <chrono>
#include <iostream>
#include <random>
#include <map>
#include <sys/mman.h>
#include <thread>
#include <vector>
#include <libephmem.h>

/*
 * Simple proof of concept outlining the usage and benefits of ephemeral memory.
 * This program repeatedly chooses a block of data uniformly at random on which
 * to perform some computation (summing over the block in this case). On
 * startup, the user specified the number of total blocks in the system (T), the
 * max number of blocks to be stored in main memory (M), and the max number of
 * blocks to be stored in ephemeral memory (E), where T >= M + E.
 *
 * When a block is chosen, the program will first check main memory, then
 * ephemeral memory for the block. If it is not found in either, the program
 * will mimic reading the block from disk by sleeping for a user specified
 * amount of time. Then, if main memory is full, the program will evict one
 * block from main memory to ephemeral memory. Likewise, if ephemeral memory
 * is full, the program will evict one block from ephemeral memory.
 */

// Time spent accessing the blocks in us
unsigned long total_time_us = 0;
// Time spent accessing the blocks in us for the last LOG_INTERVAL ms
unsigned long recent_time_us = 0;
// Total number of accesses to the blocks
unsigned long total_accesses = 0;
// Recent accesses to the blocks in the last LOG_INTERVAL ms
unsigned long recent_accesses = 0;
// Per memory type stats in the last LOG_INTERVAL ms
unsigned long main_mem_hit_time = 0;
unsigned long eph_mem_hit_time = 0;
unsigned long miss_time = 0;
unsigned long main_mem_accesses = 0;
unsigned long eph_mem_accesses = 0;
unsigned long miss_accesses = 0;
// Print the recent total and local throughput every LOG_INTERVAL ms
const unsigned long LOG_INTERVAL = 5000;
const int BLOCK_SIZE = 2 * 1024 * 1024; // 2 MB
// Number of bytes of ephemeral memory granted so far, minus the blocks we have
// found to be revoked. Added to from libephmem's internal thread by
// on_eph_mem_grant() and subtracted from by the main thread in
// handle_revoked_ephmem().
std::atomic<size_t> eph_mem_granted_bytes{0};
// Limit on how many ephemeral memory blocks are allocated in one pass of the
// main loop, so a large grant does not stall a single access.
const unsigned long MAX_EPH_ALLOCS_PER_ITER = 32;

/*
 * Called by libephmem whenever ephemeral memory is granted. Runs on
 * libephmem's internal thread, so only record the grant here and let the main
 * loop do the allocation.
 */
void on_eph_mem_grant(size_t size) {
	eph_mem_granted_bytes.fetch_add(size);
}

long compute_block_sum(int *block, size_t block_size) {
	long sum = 0;
	for (size_t i = 0; i < block_size / sizeof(int); i++) {
		sum += block[i];
	}
	return sum;
}

void fill_block(int *block, size_t block_size, int value) {
	for (size_t i = 0; i < block_size / sizeof(int); i++) {
		block[i] = value;
	}
}

/*
 * Mimic reading a block in from disk by waiting for sleep_time_us.
 *
 * sleep_for() has a wakeup overhead of roughly 100us, which swamps the shorter
 * delays we want to sweep. So sleep for all but SPIN_SLACK_US of the target and
 * spin out the remainder against a deadline. SPIN_SLACK_US must stay above the
 * wakeup overhead, or the sleep overshoots the deadline and the spin is a no-op.
 * Targets below SPIN_SLACK_US are spun in their entirety, and so do not yield
 * the CPU the way a real blocking read would.
 * Set this relatively high to prevent the CPU from lowering its frequency while
 * sleeping. Ideally, we would just pin the CPU frequency, but I don't have root
 * privileges on my workstation.
 */
const long SPIN_SLACK_US = 5000;

void mimic_block_read(long sleep_time_us) {
	auto deadline = std::chrono::steady_clock::now() +
		std::chrono::microseconds(sleep_time_us);

	if (sleep_time_us > SPIN_SLACK_US) {
		std::this_thread::sleep_for(
			std::chrono::microseconds(sleep_time_us - SPIN_SLACK_US));
	}

	while (std::chrono::steady_clock::now() < deadline)
		;
}

// Point the ephemeral memory eviction selector at the current slots
void update_eph_memory_eviction_selector(std::uniform_int_distribution<int> &selector,
					 size_t num_slots) {
	if (num_slots > 0)
		selector = std::uniform_int_distribution<int>(0, num_slots - 1);
	else
		selector = std::uniform_int_distribution<int>(0, 0); // No valid indices
}

void handle_revoked_ephmem(std::vector<struct libephmem_handle*> &eph_memory, std::vector<int> &eph_memory_rmap,
	                   std::map<int, int> &eph_memory_set, int bad_index,
			   std::uniform_int_distribution<int> &eph_memory_eviction_selector) {
	int revoked_block_id = eph_memory_rmap[bad_index];

	// Free the revoked ephemeral memory, and stop counting it as granted.
	libephmem_free(eph_memory[bad_index]);
	eph_mem_granted_bytes.fetch_sub(BLOCK_SIZE);
	// If the revoked slot held a block, that block is gone. Drop its mapping.
	if (revoked_block_id != -1)
		eph_memory_set.erase(revoked_block_id);

	// Remove the revoked slot. Every slot above it shifts down by one.
	eph_memory.erase(eph_memory.begin() + bad_index);
	eph_memory_rmap.erase(eph_memory_rmap.begin() + bad_index);

	// Point the shifted-down slots' blocks at their new indices.
	for (size_t i = bad_index; i < eph_memory.size(); i++) {
		if (eph_memory_rmap[i] != -1)
			eph_memory_set[eph_memory_rmap[i]] = i;
	}

	update_eph_memory_eviction_selector(eph_memory_eviction_selector, eph_memory.size());
}

int main(int argc, char *argv[]) {
	std::random_device rd;
	std::default_random_engine rng(rd());
	std::uniform_int_distribution<int> block_selector;
	std::uniform_int_distribution<int> main_memory_eviction_selector;
	std::uniform_int_distribution<int> eph_memory_eviction_selector;
	// Key: block id, value: index in main_memory or eph_memory vector
	std::map<int, int> main_memory_set;
	std::map<int, int> eph_memory_set;
	std::vector<int*> main_memory;
	std::vector<int> main_memory_rmap;
	std::vector<struct libephmem_handle*> eph_memory;
	std::vector<int> eph_memory_rmap;
	void *ptr;
	unsigned long total_blocks;
	unsigned long max_main_memory_blocks;
	// The number of ephemeral memory blocks we would like to have.
	unsigned long eph_target_blocks;
	int sleep_time_us;

	if (argc != 5) {
		std::cerr << "Usage: " << argv[0] << " <total_blocks> "
			<< "<max_main_memory_blocks> <max_eph_memory_blocks> "
			<< "<sleep_time_us>" << std::endl;
		return 1;
	}

	total_blocks = std::stoul(argv[1]);
	max_main_memory_blocks = std::stoul(argv[2]);
	eph_target_blocks = std::stoul(argv[3]);
	sleep_time_us = std::stoi(argv[4]);

	if (total_blocks == 0 || max_main_memory_blocks == 0) {
		std::cerr << "Error: total_blocks and max_main_memory_blocks must be > 0" << std::endl;
		return 1;
	}
	if (total_blocks < max_main_memory_blocks + eph_target_blocks) {
		std::cerr << "Error: total_blocks must be >= max_main_memory_blocks + eph_target_blocks" << std::endl;
		return 1;
	}

	// Request the ephemeral memory. It is granted asynchronously, so we start
	// with no ephemeral memory and add blocks as grants arrive.
	if (eph_target_blocks > 0) {
		libephmem_register_callback(on_eph_mem_grant);
		int ret = libephmem_request(eph_target_blocks * BLOCK_SIZE);
		if (ret) {
			std::cerr << "Failed to request ephemeral memory: " << ret << std::endl;
			return 1;
		}
	}

	block_selector = std::uniform_int_distribution<int>(0, total_blocks - 1);
	main_memory_eviction_selector = std::uniform_int_distribution<int>(0, max_main_memory_blocks - 1);
	update_eph_memory_eviction_selector(eph_memory_eviction_selector, 0);

	main_memory.reserve(max_main_memory_blocks);
	eph_memory.reserve(eph_target_blocks);
	main_memory_rmap.reserve(max_main_memory_blocks);
	eph_memory_rmap.reserve(eph_target_blocks);

	// Allocate the main memory
	ptr = mmap(nullptr, max_main_memory_blocks * BLOCK_SIZE,
		PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
	if (ptr == MAP_FAILED) {
		perror("mmap failed");
		return 1;
	}
	for (unsigned long i = 0; i < max_main_memory_blocks; i++) {
		main_memory.push_back(static_cast<int*>(ptr) + (i * (BLOCK_SIZE / sizeof(int))));
		main_memory_rmap.push_back(-1);
	}

	auto print_deadline = std::chrono::steady_clock::now()
		+ std::chrono::milliseconds(LOG_INTERVAL);
	// Main Loop
	while (true) {
		// Turn any granted ephemeral memory that we do not yet have a block for
		// into blocks. Granted memory that replaces revoked memory shows up here
		// too. This is done before starting the access timer so it does not
		// count towards access latency.
		unsigned long granted_blocks = eph_mem_granted_bytes.load() / BLOCK_SIZE;
		unsigned long wanted_blocks = std::min(granted_blocks, eph_target_blocks);
		if (wanted_blocks > eph_memory.size()) {
			unsigned long new_blocks = std::min(wanted_blocks - eph_memory.size(),
							    MAX_EPH_ALLOCS_PER_ITER);
			struct libephmem_handle *new_handles[MAX_EPH_ALLOCS_PER_ITER];
			// We may get fewer handles than we asked for. The rest will be
			// requested again on a later pass.
			ssize_t num_allocated = libephmem_alloc_multi(BLOCK_SIZE, new_blocks, new_handles);
			for (ssize_t i = 0; i < num_allocated; i++) {
				eph_memory.push_back(new_handles[i]);
				eph_memory_rmap.push_back(-1);
			}
			if (num_allocated > 0) {
				update_eph_memory_eviction_selector(eph_memory_eviction_selector,
								    eph_memory.size());
			}
		}

		auto start_time = std::chrono::steady_clock::now();
		int block_id = block_selector(rng);
		auto eph_it = eph_memory_set.find(block_id);
		int main_mem_index = -1;
		int attempt_result;
		int *block;
		long sum;
		bool main_mem_hit = false;
		bool eph_mem_hit = false;

		// Check if the block is in main memory
		auto main_it = main_memory_set.find(block_id);
		if (main_it != main_memory_set.end()) {
			block = main_memory[main_it->second];
			sum = compute_block_sum(block, BLOCK_SIZE);
			asm volatile("" : : "r,m"(sum) : "memory"); // Prevent compiler optimization
			main_mem_hit = true;
			goto time_keeping;
		}

		// Not in main memory, check ephemeral memory
		if (eph_it != eph_memory_set.end()) {
			struct libephmem_handle *handle = eph_memory[eph_it->second];
			attempt_result = libephmem_attempt(handle, [&sum](void *ptr, size_t size) {
				int *block = static_cast<int*>(ptr);
				sum = compute_block_sum(block, size);
				asm volatile("" : : "r,m"(sum) : "memory"); // Prevent compiler optimization
			});
			if (attempt_result == 1) {
				// Ephemeral memory was revoked, handle it
				handle_revoked_ephmem(eph_memory, eph_memory_rmap, eph_memory_set,
					eph_it->second, eph_memory_eviction_selector);
			} else if (attempt_result == -1) {
				std::cerr << "Invalid arguments for libephmem_attempt" << std::endl;
				return 1;
			} else {
				// Successfull ephemeral memory access
				eph_mem_hit = true;
				goto time_keeping;
			}
		}

		// Not in ephemeral memory either.
		// Do we have to evict from main memory?
		if (main_memory_set.size() >= max_main_memory_blocks && max_main_memory_blocks > 0) {
			int eph_mem_index = -1;
			int evict_block_id;
			main_mem_index = main_memory_eviction_selector(rng);

			evict_block_id = main_memory_rmap[main_mem_index];
			// This should not happen, but just in case.
			if (evict_block_id == -1) {
				std::cerr << "Failed to find block to evict from main memory" << std::endl;
				return 1;
			}

			// Find an index in ephemeral memory to evict to
			if (eph_memory_set.size() >= eph_memory.size() && eph_memory.size() > 0) {
				eph_mem_index = eph_memory_eviction_selector(rng);
				int eph_evict_block_id = eph_memory_rmap[eph_mem_index];

				// This should not happen, but just in case.
				if (eph_evict_block_id == -1) {
					std::cerr << "Failed to find block to evict from ephemeral memory" << std::endl;
					return 1;
				}

				// Remove the evicted block from ephemeral memory set
				eph_memory_set.erase(eph_evict_block_id);
				eph_memory_rmap[eph_mem_index] = -1;
			} else if (eph_memory.size() > 0) {
				// Space in ephemeral memory, use the next available index
				eph_mem_index = eph_memory_set.size();
			}

			// Evict the block from main memory
			main_memory_set.erase(evict_block_id);
			main_memory_rmap[main_mem_index] = -1;
			// Move the block from main memory to ephemeral memory if there is space
			if (eph_mem_index != -1) {
				struct libephmem_handle *handle = eph_memory[eph_mem_index];
				int *block_to_evict = main_memory[main_mem_index];
				attempt_result = libephmem_put(block_to_evict, handle, 0, BLOCK_SIZE);
				if (attempt_result == 1) {
					// Ephemeral memory was revoked, handle it
					handle_revoked_ephmem(eph_memory, eph_memory_rmap, eph_memory_set,
						eph_mem_index, eph_memory_eviction_selector);
				} else if (attempt_result == -1) {
					std::cerr << "Invalid arguments for libephmem_put" << std::endl;
					return 1;
				} else {
					eph_memory_set[evict_block_id] = eph_mem_index;
					eph_memory_rmap[eph_mem_index] = evict_block_id;
				}
			}
		} else {
			// Space in main memory, use the next available index
			main_mem_index = main_memory_set.size();
		}

		// Mimic reading the block from disk
		mimic_block_read(sleep_time_us);
		// Fill the block with some data
		block = main_memory[main_mem_index];
		fill_block(block, BLOCK_SIZE, block_id);
		main_memory_set[block_id] = main_mem_index;
		main_memory_rmap[main_mem_index] = block_id;
		sum = compute_block_sum(block, BLOCK_SIZE);
		asm volatile("" : : "r,m"(sum) : "memory"); // Prevent compiler optimization

time_keeping:
		auto end_time = std::chrono::steady_clock::now();
		auto duration = std::chrono::duration_cast<std::chrono::microseconds>(end_time - start_time);
		total_time_us += duration.count();
		recent_time_us += duration.count();
		total_accesses++;
		recent_accesses++;
		if (main_mem_hit) {
			main_mem_hit_time += duration.count();
			main_mem_accesses++;
		} else if (eph_mem_hit) {
			eph_mem_hit_time += duration.count();
			eph_mem_accesses++;
		} else {
			miss_time += duration.count();
			miss_accesses++;
		}
		if (std::chrono::steady_clock::now() >= print_deadline) {
			std::cout << "Total accesses: " << total_accesses
				<< ", Total tput (ops/sec): " << static_cast<double>(total_accesses) / (total_time_us / 1000000.0)
				<< ", Recent tput (ops/sec): " << static_cast<double>(recent_accesses) / (recent_time_us / 1000000.0)
				<< std::endl;
			std::cout << "Main memory accesses: " << main_mem_accesses
				<< ", Main memory hit latency (us): " << (main_mem_accesses > 0 ? static_cast<double>(main_mem_hit_time) / main_mem_accesses : 0)
				<< std::endl;
			std::cout << "Ephemeral memory accesses: " << eph_mem_accesses
				<< ", Ephemeral memory hit latency (us): " << (eph_mem_accesses > 0 ? static_cast<double>(eph_mem_hit_time) / eph_mem_accesses : 0)
				<< std::endl;
			std::cout << "Ephemeral memory blocks: " << eph_memory.size() << "/" << eph_target_blocks
				<< ", Ephemeral bytes granted (net of revoked blocks): " << eph_mem_granted_bytes.load()
				<< std::endl;
			std::cout << "Miss accesses: " << miss_accesses
				<< ", Miss latency (us): " << (miss_accesses > 0 ? static_cast<double>(miss_time) / miss_accesses : 0)
				<< std::endl;
			std::cout << std::endl;
			recent_time_us = 0;
			recent_accesses = 0;
			main_mem_hit_time = 0;
			main_mem_accesses = 0;
			eph_mem_hit_time = 0;
			eph_mem_accesses = 0;
			miss_time = 0;
			miss_accesses = 0;
			print_deadline = std::chrono::steady_clock::now()
				+ std::chrono::milliseconds(LOG_INTERVAL);
		}
	}
}
