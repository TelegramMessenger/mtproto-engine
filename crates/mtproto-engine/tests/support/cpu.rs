#![allow(unsafe_code)]

/// CPU time the process has used, user and system.
#[cfg(unix)]
pub fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

/// From the process's cycle count: the times Windows keeps advance a whole clock tick of 15.6 ms at
/// once, too coarse for what the tests measure. A busy loop tells how many cycles make a second.
#[cfg(windows)]
pub fn cpu_seconds() -> f64 {
    use std::ffi::c_void;
    use std::time::{Duration, Instant};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetCurrentThread() -> *mut c_void;
        fn QueryProcessCycleTime(process: *mut c_void, cycles: *mut u64) -> i32;
        fn QueryThreadCycleTime(thread: *mut c_void, cycles: *mut u64) -> i32;
    }
    static CYCLES_PER_SECOND: std::sync::OnceLock<f64> = std::sync::OnceLock::new();

    let thread_cycles = || {
        let mut cycles = 0;
        unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
        cycles
    };
    let rate = *CYCLES_PER_SECOND.get_or_init(|| {
        let spin = || {
            let (started, from) = (Instant::now(), thread_cycles());
            while started.elapsed() < Duration::from_millis(20) {
                std::hint::spin_loop();
            }
            (thread_cycles() - from) as f64 / started.elapsed().as_secs_f64()
        };
        (0..3).map(|_| spin()).fold(0.0, f64::max)
    });
    let mut cycles = 0;
    unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut cycles) };
    cycles as f64 / rate
}
