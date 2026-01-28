#ifndef KVM_PF_TRACE_H
#define KVM_PF_TRACE_H

#ifndef TASK_COMM_LEN
#define TASK_COMM_LEN 16
#endif

struct kvm_pf_trace_event {
    unsigned long fault_time_ns;
    unsigned long error_code;	
};
#endif // KVM_PF_TRACE_H