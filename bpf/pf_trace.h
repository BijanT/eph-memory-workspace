#ifndef PF_TRACE_H
#define PF_TRACE_H

#ifndef TASK_COMM_LEN
#define TASK_COMM_LEN 16
#endif

enum pf_type {
    PF_TYPE_BASE = 0,
    PF_TYPE_THP = 1,
    PF_TYPE_HUGETLB = 2,
};

struct pf_trace_event {
    unsigned long fault_time_ns;
//    unsigned long alloc_time_ns;
//    unsigned long zero_time_ns;
    unsigned int flags;
    enum pf_type type;
    unsigned char pad[3];
};

#endif // PF_TRACE_H