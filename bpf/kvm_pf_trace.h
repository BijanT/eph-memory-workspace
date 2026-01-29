#ifndef KVM_PF_TRACE_H
#define KVM_PF_TRACE_H

#ifndef TASK_COMM_LEN
#define TASK_COMM_LEN 16
#endif

enum fault_type {
    FAULT_TYPE_NONE = 0,
    FAULT_TYPE_GUEST_FINAL = 1,
    FAULT_TYPE_GUEST_PAGE = 2,
};

struct kvm_pf_trace_event {
    unsigned long fault_time_ns;
    char fault_type;
};
#endif // KVM_PF_TRACE_H