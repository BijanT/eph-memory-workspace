#!/usr/bin/env python3
import argparse
import matplotlib.pyplot as plt
import re

def parse_numactl(numactl_file):
	"""
	Parses the output of the periodic `numactl -H` collection from eph_exp.
	Returns (timestamps, used) where timestamps is a list of seconds since the
	first sample and used maps node id -> list of used memory in MB.
	"""
	timestamps = []
	sizes = {}
	used = {}
	cur_ts = None
	cur_free = {}

	def flush():
		if cur_ts is None or not cur_free:
			return
		timestamps.append(cur_ts)
		for node, free in cur_free.items():
			used.setdefault(node, []).append(sizes[node] - free)

	with open(numactl_file, 'r') as f:
		for line in f:
			m = re.match(r'ts (\d+\.\d+)', line)
			if m:
				flush()
				cur_ts = float(m.group(1))
				cur_free = {}
				sizes = {}
				continue

			m = re.match(r'node (\d+) size: (\d+) MB', line)
			if m:
				sizes[int(m.group(1))] = int(m.group(2))
				continue

			m = re.match(r'node (\d+) free: (\d+) MB', line)
			if m:
				cur_free[int(m.group(1))] = int(m.group(2))
	flush()

	start = timestamps[0] if timestamps else 0
	return [t - start for t in timestamps], used

if __name__ == "__main__":
	parser = argparse.ArgumentParser(description="Plot used memory per NUMA node from a numactl -H trace")
	parser.add_argument("numactl_file", help="Input numactl file")
	parser.add_argument("-o", "--out", help="Output file for the plot (shows the plot interactively if omitted)")
	parser.add_argument("-w", "--workload", help="Workload name to display in the title")
	args = parser.parse_args()

	timestamps, used = parse_numactl(args.numactl_file)

	plt.figure(figsize=(10, 6))
	for node in sorted(used):
		plt.plot(timestamps, [u / 1024.0 for u in used[node]], label=f"Node {node}")
	plt.xlabel('Time (s)')
	plt.ylabel('Used Memory (GB)')
	title = 'Used Memory Per NUMA Node'
	if args.workload:
		title += f' ({args.workload})'
	plt.title(title)
	plt.legend()
	plt.grid(True)

	if args.out:
		plt.savefig(args.out)
	else:
		plt.show()
