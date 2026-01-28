import csv

def process_pf_trace(trace_file):
	base_times = []
	huge_times = []
	hugetlb_times = []

	with open(trace_file, 'r') as f:
		reader = csv.reader(f)
		next(reader)  # Skip header
		i = 0
		for row in reader:
			fault_ns = int(row[0])
			fault_type = int(row[2])

			fault_us = fault_ns / 1000.0
			if fault_type == 0:
				base_times.append(fault_us)
			elif fault_type == 1:
				huge_times.append(fault_us)
			elif fault_type == 2:
				hugetlb_times.append(fault_us)

			i += 1
			if i % 100 == 0:
				print(f"Processed {i} events", end='\r')

	return base_times, huge_times, hugetlb_times
