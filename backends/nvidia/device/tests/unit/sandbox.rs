use super::*;

/// Jump offsets are the whole correctness of a classic BPF program and are computed here, not written out.
#[test]
fn every_jump_lands_inside_the_program() {
    let p = program();
    for (i, insn) in p.iter().enumerate() {
        if insn.code == RET_K {
            continue;
        }
        if insn.code == JEQ_K || insn.code == JSET_K {
            for off in [insn.jt, insn.jf] {
                let target = i + 1 + off as usize;
                assert!(target < p.len(), "instruction {i} jumps to {target}");
            }
        }
    }
}

/// The last instruction has to be the default refusal: a classic BPF program that falls off the end is rejected by the kernel, and a program whose last instruction is an ALLOW is an allowlist that allows everything.
#[test]
fn the_program_ends_in_a_refusal() {
    let p = program();
    let last = p.last().expect("not empty");
    assert_eq!(last.code, RET_K);
    assert_eq!(last.k, errno(libc::EPERM));
}

/// A plain comparison that does not match must reach the default, not the ALLOW that follows the run.
#[test]
fn the_last_plain_comparison_falls_through_to_the_refusal() {
    let p = program();
    let allow_at = p
        .iter()
        .rposition(|i| i.code == RET_K && i.k == RET_ALLOW)
        .expect("a run of comparisons ends in one");
    let last_cmp = &p[allow_at - 1];
    assert_eq!(last_cmp.code, JEQ_K);
    assert_eq!(allow_at + last_cmp.jf as usize, p.len() - 1);
}

#[test]
fn the_calls_that_matter_are_checked_rather_than_listed() {
    let listed = allowed();
    for nr in [
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_clone,
        libc::SYS_socket,
        libc::SYS_socketpair,
    ] {
        assert!(!listed.contains(&nr), "{nr} is allowed unchecked");
    }
}

/// The ones whose absence is the reason for the filter.
#[test]
fn the_dangerous_calls_are_not_in_the_list() {
    let listed = allowed();
    for nr in [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_perf_event_open,
        libc::SYS_bpf,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_keyctl,
        libc::SYS_unshare,
        libc::SYS_mount,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_fork,
        libc::SYS_vfork,
    ] {
        assert!(!listed.contains(&nr), "{nr} is allowed");
    }
}
