// SPDX-License-Identifier: GPL-2.0-only

#include <linux/module.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/hashtable.h>
#include <linux/rcupdate.h>
#include <linux/slab.h>
#include <linux/sched.h>
#include <linux/signal.h>
#include <linux/tracepoint.h>
#include <linux/uaccess.h>
#include <linux/smp.h>
#include <asm/msr.h>
#include <asm/processor.h>

#define PLATFORM_INFO 0xce
#define MISC_FEATURES 0x140
#define CPUID_FAULT_SUPPORT (1ULL << 31)
#define CPUID_FAULT_ENABLE 1ULL
struct native_query { __u64 ip; __u32 leaf, subleaf, output[4], length, reserved; };
struct capabilities { __u32 abi, flags, online_cpus, reserved; };
#define GET_CAPS _IOR('L', 0, struct capabilities)
#define QUERY_NATIVE _IOWR('L', 1, struct native_query)
#define SET_FAULT _IOW('L', 2, __u32)

struct tracked_task { struct hlist_node node; struct rcu_head retired; struct task_struct *task; };
static DEFINE_HASHTABLE(tasks, 10);
static DEFINE_SPINLOCK(tasks_lock);
static DEFINE_PER_CPU(u64, baseline_misc);
static DEFINE_PER_CPU(bool, active_fault);
static atomic_t probe_failures = ATOMIC_INIT(0);

static void native_values(u32 leaf, u32 subleaf, u32 *a, u32 *b, u32 *c, u32 *d)
{
    asm volatile("cpuid" : "=a"(*a), "=b"(*b), "=c"(*c), "=d"(*d) : "0"(leaf), "2"(subleaf));
}

static bool tracked(struct task_struct *task)
{
    struct tracked_task *entry; bool found = false;
    rcu_read_lock();
    hash_for_each_possible_rcu(tasks, entry, node, (unsigned long)task)
        if (entry->task == task) { found = true; break; }
    rcu_read_unlock();
    return found;
}

/* Caller prevents migration. */
static int apply_fault(bool enable)
{
    int error;
    if (this_cpu_read(active_fault) == enable) return 0;
    error = wrmsrq_safe(MISC_FEATURES, (this_cpu_read(baseline_misc) & ~CPUID_FAULT_ENABLE) | enable);
    if (!error) this_cpu_write(active_fault, enable);
    return error;
}

static void free_entry(struct rcu_head *retired)
{
    kfree(container_of(retired, struct tracked_task, retired));
}

static void remove_task(struct task_struct *task)
{
    struct tracked_task *entry; unsigned long flags;
    spin_lock_irqsave(&tasks_lock, flags);
    hash_for_each_possible(tasks, entry, node, (unsigned long)task) {
        if (entry->task != task) continue;
        hash_del_rcu(&entry->node); call_rcu(&entry->retired, free_entry); break;
    }
    spin_unlock_irqrestore(&tasks_lock, flags);
}

static int add_task(struct task_struct *task, gfp_t allocation)
{
    struct tracked_task *entry, *existing; unsigned long flags;
    entry = kzalloc(sizeof(*entry), allocation);
    if (!entry) return -ENOMEM;
    entry->task = task;
    spin_lock_irqsave(&tasks_lock, flags);
    hash_for_each_possible(tasks, existing, node, (unsigned long)task) {
        if (existing->task == task) {
            spin_unlock_irqrestore(&tasks_lock, flags); kfree(entry); return 0;
        }
    }
    hash_add_rcu(tasks, &entry->node, (unsigned long)task);
    spin_unlock_irqrestore(&tasks_lock, flags);
    return 0;
}

static void switched(void *unused, bool preempt, struct task_struct *previous,
                     struct task_struct *next, unsigned int previous_state)
{
    if (unlikely(apply_fault(tracked(next)))) {
        /* No native-data fallback for a task that requested interception. */
        send_sig(SIGKILL, next, 1);
    }
}
static void forked(void *unused, struct task_struct *parent, struct task_struct *child)
{
    if (tracked(parent) && add_task(child, GFP_ATOMIC)) send_sig(SIGKILL, child, 1);
}
static void exited(void *unused, struct task_struct *task)
{
    remove_task(task);
    preempt_disable(); apply_fault(false); preempt_enable();
}
static void executed(void *unused, struct task_struct *task, pid_t old_pid, struct linux_binprm *binary)
{
    /* Match Linux's exec rule: the dynamic loader starts with CPUID allowed. */
    remove_task(task);
    preempt_disable(); apply_fault(false); preempt_enable();
}

static int cpuid_length(unsigned long ip)
{
    unsigned int at; u8 byte;
    for (at = 0; at < 14; ++at) {
        if (get_user(byte, (u8 __user *)(ip + at))) return -EFAULT;
        if ((byte >= 0x40 && byte <= 0x4f) || byte == 0x66 || byte == 0x67 ||
            byte == 0x26 || byte == 0x2e || byte == 0x36 || byte == 0x3e ||
            byte == 0x64 || byte == 0x65 || byte == 0xf2 || byte == 0xf3) continue;
        if (byte != 0x0f) return -ENXIO;
        if (get_user(byte, (u8 __user *)(ip + at + 1))) return -EFAULT;
        return byte == 0xa2 ? at + 2 : -ENXIO;
    }
    return -ENXIO;
}

static long control(struct file *file, unsigned int command, unsigned long argument)
{
    void __user *user = (void __user *)argument;
    if (command == GET_CAPS) {
        struct capabilities caps = {1, 1, num_online_cpus(), 0};
        return copy_to_user(user, &caps, sizeof(caps)) ? -EFAULT : 0;
    }
    if (command == QUERY_NATIVE) {
        struct native_query query; int length = 0;
        if (copy_from_user(&query, user, sizeof(query))) return -EFAULT;
        if (query.reserved) return -EINVAL;
        if (query.ip) {
            length = cpuid_length(query.ip);
            if (length < 0) return length;
        }
        query.length = length;
        preempt_disable();
        native_values(query.leaf, query.subleaf, &query.output[0], &query.output[1],
                    &query.output[2], &query.output[3]);
        preempt_enable();
        return copy_to_user(user, &query, sizeof(query)) ? -EFAULT : 0;
    }
    if (command == SET_FAULT) {
        __u32 enable; int error;
        if (get_user(enable, (__u32 __user *)user)) return -EFAULT;
        if (enable > 1 || !current->mm) return -EINVAL;
        if (enable) { error = add_task(current, GFP_KERNEL); if (error) return error; }
        else remove_task(current);
        preempt_disable(); error = apply_fault(enable); preempt_enable();
        if (error) { remove_task(current); return -EIO; }
        return 0;
    }
    return -ENOTTY;
}
static const struct file_operations operations = { .owner = THIS_MODULE, .unlocked_ioctl = control };
static struct miscdevice device = { .minor = MISC_DYNAMIC_MINOR, .name = "legion_cpuid", .fops = &operations, .mode = 0666 };

static void inspect_cpu(void *unused)
{
    u64 platform, value, after;
    if (rdmsrq_safe(PLATFORM_INFO, &platform) || !(platform & CPUID_FAULT_SUPPORT) ||
        rdmsrq_safe(MISC_FEATURES, &value) || (value & CPUID_FAULT_ENABLE)) goto failed;
    if (wrmsrq_safe(MISC_FEATURES, value | CPUID_FAULT_ENABLE)) goto failed;
    if (rdmsrq_safe(MISC_FEATURES, &after) || !(after & CPUID_FAULT_ENABLE)) {
        wrmsrq_safe(MISC_FEATURES, value); goto failed;
    }
    if (wrmsrq_safe(MISC_FEATURES, value)) goto failed;
    this_cpu_write(baseline_misc, value); this_cpu_write(active_fault, false); return;
failed:
    atomic_inc(&probe_failures);
}

static struct tracepoint *points[4];
static const char *const names[] = {"sched_switch", "sched_process_fork", "sched_process_exit", "sched_process_exec"};
static void *const callbacks[] = {switched, forked, exited, executed};
static void find_point(struct tracepoint *point, void *unused)
{
    unsigned int i;
    for (i = 0; i < ARRAY_SIZE(names); ++i) if (!strcmp(point->name, names[i])) points[i] = point;
}
static int __init start(void)
{
    u32 a,b,c,d; unsigned int i; int error;
    native_values(0x40000000, 0, &a,&b,&c,&d);
    if (b != 0x4b4d564b || c != 0x564b4d56 || d != 0x4d) return -ENODEV;
    on_each_cpu(inspect_cpu, NULL, 1);
    if (atomic_read(&probe_failures)) { pr_err("legion_cpuid: virtual CPUID-fault MSRs unavailable on %d CPUs\n", atomic_read(&probe_failures)); return -ENODEV; }
    for_each_kernel_tracepoint(find_point, NULL);
    for (i = 0; i < ARRAY_SIZE(points); ++i) if (!points[i]) return -ENOENT;
    for (i = 0; i < ARRAY_SIZE(points); ++i) {
        error = tracepoint_probe_register(points[i], callbacks[i], NULL);
        if (error) goto cleanup;
    }
    error = misc_register(&device);
    if (!error) { pr_info("legion_cpuid: guest-only task CPUID faults, native-query ABI 1, CPUs=%u\n", num_online_cpus()); return 0; }
cleanup:
    while (i) { --i; tracepoint_probe_unregister(points[i], callbacks[i], NULL); }
    tracepoint_synchronize_unregister(); return error;
}
module_init(start);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Disposable KVM guest task-local CPUID interception prototype");
