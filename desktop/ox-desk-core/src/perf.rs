//! CLOCK_MONOTONIC shares a timebase between the GUI and its probe process.
pub fn monotonic_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC is supported by both desktop targets, and t is writable.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    assert_eq!(rc, 0, "read monotonic clock");
    (t.tv_sec as u64) * 1_000_000_000 + t.tv_nsec as u64
}

/// Kernel process-wide counter, including threads which retired between samples.
pub fn voluntary_switches() -> Option<i64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // A successful getrusage initializes the whole structure at this writable address.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    (result == 0).then(|| unsafe { usage.assume_init() }.ru_nvcsw)
}
