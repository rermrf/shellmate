//! What the monitor hands to the UI: one [`Snapshot`] holds everything the pages show, with the rates
//! already worked out. Every field is `None` or empty until the first frames have been seen; rates
//! need two samples.

use crate::parse::LoadAvg;

/// How many points the sparklines keep (a stats frame every 2 s makes two minutes).
pub const HISTORY_POINTS: usize = 60;

/// The mount points above this usage (percent) are worth a look.
pub const DISK_ATTENTION_PERCENT: u8 = 80;

/// The facts of the header.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostFacts {
    /// `PRETTY_NAME` of the system.
    pub os_name: Option<String>,
    /// `Linux 5.15.0-91-generic`.
    pub kernel: String,
    /// Seconds since boot, as of the newest frame.
    pub uptime_secs: f64,
    /// The load averages.
    pub load: Option<LoadAvg>,
    /// How many cores are online.
    pub cores: usize,
}

/// The CPU card.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuCard {
    /// Busy time of all cores together, percent (0-100); `None` until there are two samples.
    pub percent: Option<f32>,
    /// Busy time of each core, percent, in the order of the cores.
    pub per_core: Vec<Option<f32>>,
    /// The last [`HISTORY_POINTS`] values of `percent`, the oldest first.
    pub history: Vec<f32>,
}

/// The memory card, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryCard {
    /// Installed memory.
    pub total: u64,
    /// In use by programs (what is left after free memory and the cache).
    pub used: u64,
    /// Buffers and page cache: given back when programs ask for it.
    pub cache: u64,
    /// What programs can still get without swapping.
    pub available: u64,
    /// Swap space (0 if there is none).
    pub swap_total: u64,
    /// Swap space in use.
    pub swap_used: u64,
}

impl MemoryCard {
    /// Used memory in percent of the total.
    pub fn used_percent(&self) -> f32 {
        percent(self.used, self.total)
    }

    /// Used plus cache in percent of the total (the two parts of the bar).
    pub fn used_and_cache_percent(&self) -> f32 {
        percent(self.used + self.cache, self.total).min(100.0)
    }
}

fn percent(part: u64, whole: u64) -> f32 {
    if whole == 0 { 0.0 } else { (part as f64 / whole as f64 * 100.0) as f32 }
}

/// A mounted volume, in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// Where it is mounted.
    pub mount: String,
    /// The device or file system name.
    pub filesystem: String,
    /// Size.
    pub total: u64,
    /// Used.
    pub used: u64,
    /// Available to ordinary users.
    pub available: u64,
    /// How full it is, as `df` says it.
    pub percent: u8,
}

/// The network card: the interface of the default route.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkCard {
    /// `eth0`
    pub interface: String,
    /// Bytes per second received; `None` until there are two samples.
    pub rx_per_sec: Option<f64>,
    /// Bytes per second sent.
    pub tx_per_sec: Option<f64>,
    /// The last [`HISTORY_POINTS`] values of `rx_per_sec`, the oldest first.
    pub rx_history: Vec<f64>,
    /// The last [`HISTORY_POINTS`] values of `tx_per_sec`.
    pub tx_history: Vec<f64>,
}

/// One process.
#[derive(Clone, Debug, PartialEq)]
pub struct Process {
    /// Process id.
    pub pid: u32,
    /// Parent process id (for the tree).
    pub ppid: u32,
    /// Owner: a name if the server knows the id, otherwise the number; empty if `ps` did not say.
    pub user: String,
    /// The executable's short name.
    pub name: String,
    /// The command line (the name if `ps` did not say), cut at 2048 characters.
    pub command: String,
    /// `R` running, `S` sleeping, `D` waiting for I/O, `Z` zombie, `T` stopped, `I` idle kernel thread.
    pub state: char,
    /// CPU in percent of one core (a multi-threaded process can pass 100); `None` for a process that was
    /// not in the previous sample.
    pub cpu_percent: Option<f32>,
    /// Resident memory in percent of the installed memory.
    pub mem_percent: Option<f32>,
    /// Resident memory in bytes.
    pub rss_bytes: Option<u64>,
    /// How many threads.
    pub threads: u32,
    /// The nice value.
    pub nice: i32,
    /// When the process started, in clock ticks after boot: with the process id it tells this process from
    /// another one that got the same number later.
    pub start_time: u64,
}

/// Everything the pages show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The header.
    pub host: HostFacts,
    /// The CPU card.
    pub cpu: Option<CpuCard>,
    /// The memory card.
    pub memory: Option<MemoryCard>,
    /// The mounted volumes that are not pseudo file systems.
    pub disks: Vec<Disk>,
    /// The network card.
    pub network: Option<NetworkCard>,
    /// The CPU temperature in degrees Celsius, if the server has a sensor.
    pub temperature: Option<f32>,
    /// Every process, by process id.
    pub processes: Vec<Process>,
    /// How many of them are zombies.
    pub zombies: usize,
}

impl Snapshot {
    /// The `n` processes that use the most CPU (processes without a rate yet come last).
    pub fn top_by_cpu(&self, n: usize) -> Vec<&Process> {
        let mut processes: Vec<&Process> = self.processes.iter().collect();
        processes.sort_by(|a, b| {
            b.cpu_percent.unwrap_or(-1.0).total_cmp(&a.cpu_percent.unwrap_or(-1.0)).then(a.pid.cmp(&b.pid))
        });
        processes.truncate(n);
        processes
    }

    /// The volumes that are at least `percent` full.
    pub fn disks_over(&self, percent: u8) -> Vec<&Disk> {
        self.disks.iter().filter(|disk| disk.percent >= percent).collect()
    }

    /// The process with this id.
    pub fn process(&self, pid: u32) -> Option<&Process> {
        self.processes.binary_search_by_key(&pid, |p| p.pid).ok().map(|i| &self.processes[i])
    }
}
