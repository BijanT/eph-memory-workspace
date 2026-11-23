#!/usr/bin/env python3

# Job stat mapper for eph_exp experiments measuring how quickly a donor can
# revoke ephemeral memory, e.g.:
#   eph_exp {MACHINE} bijan --donor_workload multi_alloc:<size>:<procs> \
#       [--consumer_workload libephmem_test:<size>:<wait>] --donor_delay <secs>

import sys
import json
import re
import statistics

def get_alloc_time(multi_alloc_file):
	with open(multi_alloc_file, "r") as f:
		for line in f:
			m = re.search(r"In (\d+\.\d+) seconds", line)
			if m:
				return float(m.group(1))
	return None

# Returns a list of the values of the given field for each instance of the
# given QEMU trace event in the log file.
def get_trace_field(log_file, event, field):
	pattern = re.compile(r"\b" + event + r"\b.*\b" + field + r"=(-?\d+)")
	values = []
	with open(log_file, "r") as f:
		for line in f:
			m = pattern.search(line)
			if m:
				values.append(int(m.group(1)))
	return values

def fmt(val):
	if val is None:
		return "N/A"
	return str(val)

json_data = None
for line in sys.stdin:
	json_data = json.loads(line)

filename_stub = json_data['results_path']
cmd = json_data['cmd']
jid = json_data['jid']

multi_alloc_file = filename_stub + "donor_vm.multi_alloc"
donor_log_file = filename_stub + "donor_vm.log"
consumer_log_file = filename_stub + "consumer_vm.log"

procs = int(re.findall(r"multi_alloc:\d+:(\d+)", cmd)[0])
consumer_match = re.search(r"--consumer_workload\s+(\S+)", cmd)
consumer_workload = consumer_match.group(1) if consumer_match else None
consumer = consumer_workload is not None

alloc_time = get_alloc_time(multi_alloc_file)

revoke_times = get_trace_field(donor_log_file, "eph_mem_return_capacity", "latency_us")
max_revoke = max(revoke_times) if revoke_times else None
median_revoke = statistics.median(revoke_times) if revoke_times else None

wait_times = get_trace_field(donor_log_file, "eph_mem_wait_for_return", "wait_time_us")
total_wait = sum(wait_times)

median_release = None
if consumer:
	release_times = get_trace_field(consumer_log_file, "cxl_dc_extent_release", "total_us")
	if release_times:
		median_release = statistics.median(release_times)

outdata = {
	"JID": jid,
	"Command": cmd,
	"File": filename_stub,
	"Procs": str(procs),
	"Consumer": fmt(consumer_workload),
	"Alloc Time (s)": fmt(alloc_time),
	"Max Revoke (us)": fmt(max_revoke),
	"Median Revoke (us)": fmt(median_revoke),
	"Total Wait (us)": str(total_wait),
	"Median Consumer Release (us)": fmt(median_release),
}

print(json.dumps(outdata))
