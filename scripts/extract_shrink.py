#!/usr/bin/env python3

import sys
import os
import json
import re

def get_alloc_time(alloc_file):
	f = open(alloc_file, "r")
	line = f.readline()

	alloc_time = re.findall("In (\d+\.\d+) seconds", line)[0]
	return float(alloc_time)

def get_shrink_time(shrink_file):
	f = open(shrink_file, "r")
	line = f.readline()

	shrink_time = float(line)
	return shrink_time

json_data = None
for line in sys.stdin:
	json_data = json.loads(line)

filename_stub = json_data['results_path']
cmd = json_data['cmd']
jid = json_data['jid']

alloc_data_file = filename_stub + "alloc_data"
shrink_time_file = filename_stub + "shrink_time"

if "--balloon" in cmd:
	strategy = "balloon"
elif "--hotunplug" in cmd:
	strategy = "hotunplug"
else:
	print("Unknown strategy in command:", cmd, file=sys.stderr)
	exit(1)

alloc_size = int(re.findall("--alloc_size (\d+)", cmd)[0])
shrink_size = int(re.findall("--shrink_size (\d+)", cmd)[0])
reclaimed_size = alloc_size - shrink_size

alloc_time = get_alloc_time(alloc_data_file)
shrink_time = get_shrink_time(shrink_time_file)

alloc_tput = alloc_size / alloc_time
shrink_tput = reclaimed_size / shrink_time

outdata = {
	"JID": jid,
	"Command": cmd,
	"Strategy": strategy,
	"File": filename_stub,
	"Alloc Size (GB)": str(alloc_size),
	"Reclaim Size (GB)": str(reclaimed_size),
	"Alloc Time (s)": str(alloc_time),
	"Alloc TPut (GB/s)": str(round(alloc_tput, 2)),
	"Shrink Time (s)": str(shrink_time),
	"Shrink TPut (GB/s)": str(round(shrink_tput, 2)),
}

print(json.dumps(outdata))
