import csv

def process_pf_trace(trace_file):
	base_times = []
	huge_times = []

	with open(trace_file, 'r') as f:
		reader = csv.reader(f)
		next(reader)  # Skip header
		i = 0
		for row in reader:
			fault_ns = int(row[0])
			huge_fault = int(row[2])

			fault_us = fault_ns / 1000.0
			if huge_fault:
				huge_times.append(fault_us)
			else:
				base_times.append(fault_us)

			i += 1
			if i % 100 == 0:
				print(f"Processed {i} events", end='\r')

	return base_times, huge_times
