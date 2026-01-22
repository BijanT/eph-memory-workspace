#!/usr/bin/env python3
import sys
import re

if len(sys.argv) != 3 and len(sys.argv) != 4:
	print("Usage: python split_perf_script.py <number_of_splits> <output_file_prefix> [input_file]")
	print("If input_file is not provided, reads from stdin.")
	sys.exit(1)

num_splits = int(sys.argv[1])
output_prefix = sys.argv[2]
if len(sys.argv) == 4:
	input_filename = sys.argv[3]
	input_file = open(input_filename, 'r')
else:
	input_file = sys.stdin

# Because stdin is not seekable, we have to read all lines into memory.
# The traces are probably small enough, and our server's memory large enough
# that this is fine.
input_lines = input_file.readlines()
if input_file is not sys.stdin:
	input_file.close()

# By default `perf script` outputs lines like:
#    <comm> <pid> [<cpu>] <timestamp>: <event>: <details>
# followed by a stacktrace indented by a tab. Each event is separated by a blank line.
#
# We will look at the timestamps of the first and last events, and split the output
# files into `num_splits` equal time intervals.
trace_pattern = re.compile(r'^\S+\s+\d+\s+\[\d+\]\s+(\d+\.\d+):.*')

# Find the first and last timestamps
first_timestamp = None
last_timestamp = None
for line in input_lines:
	match = trace_pattern.match(line)
	if match:
		first_timestamp = float(match.group(1))
		break
for line in reversed(input_lines):
	match = trace_pattern.match(line)
	if match:
		last_timestamp = float(match.group(1))
		break

if first_timestamp is None or last_timestamp is None or first_timestamp >= last_timestamp:
	print("Error: Could not determine valid first and last timestamps from input.")
	sys.exit(1)

interval_len = (last_timestamp - first_timestamp) / num_splits
cur_interval = 0
end_time = first_timestamp + interval_len

output_file = open(f"{output_prefix}_0.perfscript", 'w')
for line in input_lines:
	match = trace_pattern.match(line)
	if match:
		timestamp = float(match.group(1))
		# Check if we need to move to the next output file
		# while loop in case of weird shenanigans where there are large gaps between events
		while timestamp >= end_time and cur_interval < num_splits - 1:
			output_file.close()
			cur_interval += 1
			end_time = first_timestamp + ((cur_interval + 1) * interval_len)
			output_file = open(f"{output_prefix}_{cur_interval}.perfscript", 'w')
	output_file.write(line)