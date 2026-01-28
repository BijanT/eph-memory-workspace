#!/usr/bin/env python3
import matplotlib.pyplot as plt
import sys
import os
from helpers import process_pf_trace

if __name__ == "__main__":
	if len(sys.argv) != 2 and len(sys.argv) != 3:
		print("Usage: python pf_trace_time.py <trace_file> [plot_file]")
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

	plt.figure(figsize=(10, 6))
	plt.plot(times_to_plot, marker='o', linestyle='None', markersize=0.5)
	plt.xlabel('Page Fault Event Index')
	plt.ylabel('Page Fault Time (us)')
	plt.title('Page Fault Handling Times Over Events')
	plt.grid(True)

	if plot_file:
		plt.savefig(plot_file)
	else:
		plt.show()