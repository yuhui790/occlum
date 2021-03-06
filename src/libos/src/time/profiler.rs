use super::*;

lazy_static! {
    pub static ref GLOBAL_PROFILER: SgxMutex<GlobalProfiler> = SgxMutex::new(GlobalProfiler::new());
}

/// A profiler that can be used across threads.
// TODO: Use light-weight thread_local storage other than mutex
pub struct GlobalProfiler {
    inner: HashMap<pid_t, ThreadProfiler>,
}

impl GlobalProfiler {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    pub fn thread_enter(&mut self) -> Result<()> {
        let tid = current!().tid();
        if self.inner.insert(tid, ThreadProfiler::new()).is_some() {
            return_errno!(
                EINVAL,
                "global profiler should exit the thread before entering"
            );
        }
        self.inner.get_mut(&tid).unwrap().start()
    }

    pub fn thread_exit(&mut self) -> Result<()> {
        // A thread exits by invoking exit syscall which
        // will never return
        self.syscall_exit(SyscallNum::Exit, false);

        let tid = current!().tid();

        let mut exiting_profiler = self.inner.remove(&tid).ok_or_else(|| {
            errno!(
                EINVAL,
                "global profiler should enter a thread before exit one"
            )
        })?;
        exiting_profiler.stop()?;
        exiting_profiler.display()?;
        Ok(())
    }

    pub fn syscall_enter(&mut self, syscall_num: SyscallNum) -> Result<()> {
        let tid = current!().tid();
        let mut prof = self.inner.get_mut(&tid).unwrap();
        prof.syscall_enter(syscall_num)
    }

    pub fn syscall_exit(&mut self, syscall_num: SyscallNum, is_err: bool) -> Result<()> {
        let tid = current!().tid();
        let mut prof = self.inner.get_mut(&tid).unwrap();
        prof.syscall_exit(syscall_num, is_err)
    }
}
/// A profiler used inside a thread.
// TODO: add support for exception
#[derive(Clone)]
pub struct ThreadProfiler {
    syscall_data: HashMap<SyscallNum, PerfEntry>,
    start_time: ProfileTime,
    status: Status,
}

impl ThreadProfiler {
    pub fn new() -> Self {
        Self {
            syscall_data: HashMap::new(),
            start_time: ProfileTime::TwoTimes {
                real: Duration::new(0, 0),
                cpu: Duration::new(0, 0),
            },
            status: Status::Stopped(TimeSummary::new(
                Duration::new(0, 0),
                Duration::new(0, 0),
                Duration::new(0, 0),
            )),
        }
    }

    fn start(&mut self) -> Result<()> {
        match self.status {
            Status::Stopped(..) => {
                self.status = Status::Running;
                self.start_time.update()
            }
            _ => return_errno!(
                EINVAL,
                "thread profiler can only be started in stopped status"
            ),
        }
    }

    fn stop(&mut self) -> Result<()> {
        if self.status != Status::Running {
            return_errno!(EINVAL, "fail to stop thread profiler");
        }
        let real = time::do_gettimeofday().as_duration() - self.start_time.get_realtime().unwrap();

        let total_cputime =
            time::do_thread_getcpuclock()?.as_duration() - self.start_time.get_cputime().unwrap();
        let sys = self.get_syscall_total_time()?;
        let usr = total_cputime - sys;
        self.status = Status::Stopped(TimeSummary::new(real, usr, sys));
        Ok(())
    }

    fn syscall_enter(&mut self, syscall_num: SyscallNum) -> Result<()> {
        match self.status {
            Status::Running => {
                let mut cur_time = ProfileTime::CpuTime(Default::default());
                cur_time.update()?;
                self.status = Status::InSyscall {
                    start_cpu: cur_time,
                    num: syscall_num,
                };
                self.syscall_data
                    .entry(syscall_num)
                    .or_insert(PerfEntry::new());
                Ok(())
            }
            _ => return_errno!(
                EINVAL,
                "threa profiler should be started before entering syscall"
            ),
        }
    }

    fn syscall_exit(&mut self, syscall_num: SyscallNum, is_err: bool) -> Result<()> {
        match self.status {
            Status::InSyscall { start_cpu, num } => {
                if syscall_num != num {
                    return_errno!(EINVAL, "syscall number mismatches");
                }
                self.status = Status::Running;
                let syscall_cpu_time =
                    time::do_thread_getcpuclock()?.as_duration() - start_cpu.get_cputime().unwrap();

                self.syscall_data.entry(num).and_modify(|e| {
                    e.update(syscall_cpu_time, is_err)
                        .expect("fail to update syscall data")
                });
                Ok(())
            }
            _ => return_errno!(
                EINVAL,
                "thread profiler should be in one syscall before exiting the syscall"
            ),
        }
    }

    fn get_syscall_total_time(&self) -> Result<Duration> {
        Ok(self.get_syscall_total()?.0)
    }

    fn get_syscall_total(&self) -> Result<(Duration, u32, u32)> {
        let mut total_time: Duration = Duration::new(0, 0);
        let mut total_calls: u32 = 0;
        let mut total_errors: u32 = 0;
        for entry in self.syscall_data.values() {
            total_time += entry.get_total_time();
            total_calls = entry
                .get_calls()
                .checked_add(total_calls)
                .ok_or_else(|| errno!(EOVERFLOW, "total calls overflow"))?;
            total_errors += entry.get_errors();
        }
        Ok((total_time, total_calls, total_errors))
    }

    fn display(&self) -> Result<()> {
        match self.status {
            Status::Stopped(report) => {
                // Pretty-print the Debug formatting report of the profiled thread
                println!("{:#?}", report);
                // Print the syscall statistics of the profiled thread
                self.display_syscall_stat()
            }
            _ => return_errno!(EINVAL, "thread profiler can report only in stopped status"),
        }
    }

    /// Print the syscall statistics of the profiled thread.
    ///
    /// The statistics consist of:
    /// syscall number, the corresponding percentage of the aggregate time in all the syscalls,
    /// the aggregate time, average execution time of a call, aggregate calls, aggregate errors,
    /// the shortest and longest execution time of the syscall.
    /// A piece of the output is:
    /// syscall             % time     seconds     us/call     calls    errors range(us)
    /// ------------------- ------ ----------- ----------- --------- --------- -----------
    /// SysWritev             0.40    0.000131          26         5         0 [12, 47]
    /// SysMprotect           0.03    0.000009           4         2         0 [4, 4]
    /// ------------------- ------ ----------- ----------- --------- --------- -----------
    fn display_syscall_stat(&self) -> Result<()> {
        println!(
            "{:<19} {:>6} {:>11} {:>11} {:>9} {:>9} {}",
            "syscall", "% time", "seconds", "us/call", "calls", "errors", "range(us)",
        );
        println!(
            "{:-<19} {:-<6} {:-<11} {:-<11} {:-<9} {:-<9} {:-<11}",
            "", "", "", "", "", "", ""
        );

        let (total_time, total_calls, total_errors) = self.get_syscall_total()?;
        let mut syscall_data_ref: Vec<(&SyscallNum, &PerfEntry)> =
            self.syscall_data.iter().collect();
        syscall_data_ref.sort_by(|(_, entry_a), (_, entry_b)| {
            entry_b.get_total_time().cmp(&(entry_a.get_total_time()))
        });

        for (syscall_num, entry) in syscall_data_ref {
            let time_percentage =
                entry.get_total_time().as_secs_f64() / total_time.as_secs_f64() * 100_f64;
            println!(
                "{:<19} {:>6.2} {:?}",
                format!("{:?}", syscall_num),
                time_percentage,
                entry,
            );
        }

        println!(
            "{:-<19} {:-<6} {:-<11} {:-<11} {:-<9} {:-<9} {:-<11}",
            "", "", "", "", "", "", ""
        );

        println!(
            "{} {:>20} {:>11.6} {:>21} {:>9}",
            "total",
            "100",
            total_time.as_secs_f64(),
            total_calls,
            total_errors,
        );
        Ok(())
    }
}

#[derive(Copy, Clone)]
struct PerfEntry {
    calls: u32,
    total_time: Duration,
    peak: Duration,
    bottom: Duration,
    errors: u32,
}

impl PerfEntry {
    fn new() -> Self {
        Self {
            calls: 0,
            total_time: Duration::new(0, 0),
            peak: Duration::new(0, 0),
            bottom: Duration::new(u64::MAX, 1_000_000_000 - 1),
            errors: 0,
        }
    }

    fn update(&mut self, time: Duration, is_err: bool) -> Result<()> {
        self.calls = self
            .calls
            .checked_add(1)
            .ok_or_else(|| errno!(EOVERFLOW, "single syscallduration addition overflow"))?;
        self.total_time += time;

        if time > self.peak {
            self.peak = time;
        }

        if time < self.bottom {
            self.bottom = time;
        }

        if is_err {
            self.errors += 1;
        }
        Ok(())
    }

    fn get_average(&self) -> Duration {
        if self.calls == 0 {
            Duration::new(0, 0)
        } else {
            self.total_time / self.calls
        }
    }

    fn get_calls(&self) -> u32 {
        self.calls
    }

    fn get_total_time(&self) -> Duration {
        self.total_time
    }

    fn get_errors(&self) -> u32 {
        self.errors
    }
}

/// Used for the display of ThreadProfiler.
/// The total execution time in secs, average execution time in microseconds,
/// total calls, total errors, the shortest and longest execution time.
/// The output looks like:
/// 0.000009           4         2         0 [4, 4]
impl fmt::Debug for PerfEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:>11.6} {:>11} {:>9} {:>9} [{}, {}]",
            self.total_time.as_secs_f64(),
            self.get_average().as_micros(),
            self.calls,
            self.errors,
            self.bottom.as_micros(),
            self.peak.as_micros()
        )
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ProfileTime {
    RealTime(Duration),
    CpuTime(Duration),
    TwoTimes { real: Duration, cpu: Duration },
}

impl ProfileTime {
    fn get_realtime(&self) -> Option<Duration> {
        match *self {
            ProfileTime::RealTime(t) => Some(t),
            ProfileTime::CpuTime(t) => None,
            ProfileTime::TwoTimes { real, cpu } => Some(real),
        }
    }

    fn get_cputime(&self) -> Option<Duration> {
        match *self {
            ProfileTime::RealTime(t) => None,
            ProfileTime::CpuTime(t) => Some(t),
            ProfileTime::TwoTimes { real, cpu } => Some(cpu),
        }
    }

    fn update(&mut self) -> Result<()> {
        match self {
            ProfileTime::RealTime(ref mut t) => *t = time::do_gettimeofday().as_duration(),
            ProfileTime::CpuTime(ref mut t) => *t = time::do_thread_getcpuclock()?.as_duration(),
            ProfileTime::TwoTimes {
                ref mut real,
                ref mut cpu,
            } => {
                *real = time::do_gettimeofday().as_duration();
                *cpu = time::do_thread_getcpuclock()?.as_duration();
            }
        }
        Ok(())
    }
}

/// The timing statistics about one thread.
/// These statistics consist of:
/// (i) the elapsed real time between invocation and termination
/// (ii) the CPU time running in user space
/// (iii) the CPU time running in libos
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct TimeSummary {
    real: Duration,
    usr: Duration,
    sys: Duration,
}

impl TimeSummary {
    fn new(real: Duration, usr: Duration, sys: Duration) -> Self {
        Self { real, usr, sys }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Status {
    Running,
    Stopped(TimeSummary),
    InSyscall {
        start_cpu: ProfileTime,
        num: SyscallNum,
    },
}
