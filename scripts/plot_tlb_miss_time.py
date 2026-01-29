#!/usr/bin/env python3
import sys
import re
import matplotlib.pyplot as plt
import numpy as np

def read_perf_stat_file(filename):
	# perf stat output is in the format:
	# <time> <counts> <event>
	perf_pattern = re.compile(r'^\s*([\d,]+\.\d+)\s+([\d,]+)\s+(.+)$')
	misses_event = "dtlb_load_misses.miss_causes_a_walk"
	walk_time_event = "dtlb_load_misses.walk_duration"
	times = []
	misses = []
	walk_times = []

	for line in open(filename, 'r'):
		match = perf_pattern.match(line)
		if match:
			time_str = match.group(1).replace(',', '')
			counts_str = match.group(2).replace(',', '')
			event = match.group(3).strip()

			time = float(time_str)
			counts = int(counts_str)

			if len(times) == 0 or times[-1] != time:
				times.append(time)

			if event == misses_event:
				misses.append(counts)
			elif event == walk_time_event:
				walk_times.append(counts)

	return times, misses, walk_times

if len(sys.argv) != 2 and len(sys.argv) != 3:
	print("Usage: python plot_tlb_miss_time.py <perf_stat_file> [output_file]")
	sys.exit(1)

perf_stat_file = sys.argv[1]
output_file = None
if len(sys.argv) == 3:
	output_file = sys.argv[2]
times, misses, walk_times = read_perf_stat_file(perf_stat_file)
cycles_per_miss = np.array(walk_times) / np.array(misses)

plt.figure(figsize=(10, 6))
plt.plot(times, cycles_per_miss, label='DTLB Cycles Per Miss', color='blue')
plt.xlabel('Time (s)')
plt.ylabel('Cycles Per DTLB Miss')
plt.title('DTLB Miss Latency Over Time')
plt.legend()
plt.grid(True)

if output_file:
	plt.savefig(output_file)
else:
	plt.show()