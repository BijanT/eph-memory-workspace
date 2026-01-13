#ifndef PF_TRACE_H
#define PF_TRACE_H

struct pf_trace_event {
	unsigned long fault_time_ns;
	unsigned long alloc_time_ns;
	unsigned int flags;
	unsigned char huge_fault;
	unsigned char pad[3];
};

#endif // PF_TRACE_H