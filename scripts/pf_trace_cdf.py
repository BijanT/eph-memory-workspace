#!/usr/bin/env python3
import numpy as np
import matplotlib.pyplot as plt
import sys
import csv
from helpers import process_pf_trace

def plot_cdf(data, median, max_val, stddev, out_file):
	xlim = min(max_val, median + 5 * stddev)

	plt.figure(figsize=(10, 6))
	plt.hist(data, bins=1500, density=True, histtype='step', cumulative=True)
	plt.xlabel('Page Fault Handling Time (us)')
	plt.ylabel('Cumulative Frequency')
	plt.title('CDF of Page Fault Handling Times')
	plt.grid(True)
	plt.axvline(median, color='r', linestyle='dashed', linewidth=1, label='Median')
	plt.axvline(median + stddev, color='g', linestyle='dashed', linewidth=1, label=r'Median + $\sigma$')
	plt.axvline(median - stddev, color='g', linestyle='dashed', linewidth=1, label=r'Median - $\sigma$')
	plt.xlim(0, xlim)
	plt.legend()
	if out_file:
			plt.savefig(out_file)
	else:
			plt.show()

if __name__ == "__main__":
	if len(sys.argv) != 2 and len(sys.argv) != 3:
		print("Usage: ./pf_trace_cdf.py <trace_file> [plot file]")
		sys.exit(1)

	trace_file = sys.argv[1]
	plot_file = sys.argv[2] if len(sys.argv) == 3 else None

	base_times, huge_times, hugetlb_times = process_pf_trace(trace_file)

	if len(hugetlb_times) > 0:
		times_to_plot = hugetlb_times
	elif len(huge_times) > 0:
		times_to_plot = huge_times
	else:
		times_to_plot = base_times

	median = np.median(times_to_plot)
	max_val = np.max(times_to_plot)
	stddev = np.std(times_to_plot)

	print(f"Median: {median} us, Max: {max_val} us, Stddev: {stddev} us")

	plot_cdf(times_to_plot, median, max_val, stddev, plot_file)