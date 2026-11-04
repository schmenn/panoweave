//! Memory information of the computer and of this program (Windows; harmless defaults elsewhere).

#[cfg(windows)]
mod win {
    #[repr(C)]
    pub struct MemoryStatusEx {
        pub length: u32,
        pub memory_load: u32,
        pub total_phys: u64,
        pub avail_phys: u64,
        pub total_page_file: u64,
        pub avail_page_file: u64,
        pub total_virtual: u64,
        pub avail_virtual: u64,
        pub avail_extended_virtual: u64,
    }
    #[repr(C)]
    pub struct ProcessMemoryCounters {
        pub cb: u32,
        pub page_fault_count: u32,
        pub peak_working_set_size: usize,
        pub working_set_size: usize,
        pub quota_peak_paged_pool_usage: usize,
        pub quota_paged_pool_usage: usize,
        pub quota_peak_non_paged_pool_usage: usize,
        pub quota_non_paged_pool_usage: usize,
        pub pagefile_usage: usize,
        pub peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32;
        pub fn GetCurrentProcess() -> isize;
        pub fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
        pub fn K32GetProcessMemoryInfo(process: isize, counters: *mut ProcessMemoryCounters, cb: u32) -> i32;
    }
}

/// Free space in bytes on the drive that holds `path` (the nearest existing folder above it is
/// asked, so a file that does not exist yet is fine). None if unknown.
pub fn disk_free(path: &std::path::Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut dir = path.parent().map(|p| p.to_path_buf()).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::PathBuf::from("."));
        while !dir.exists() {
            if !dir.pop() {
                dir = std::path::PathBuf::from(".");
                break;
            }
        }
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
        // SAFETY: `wide` is a zero-terminated string and the three pointers point to live u64 values
        if unsafe { win::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) } != 0 {
            return Some(free);
        }
    }
    let _ = path;
    None
}

/// Free physical memory in bytes (None if unknown).
pub fn available() -> Option<u64> {
    #[cfg(windows)]
    unsafe {
        let mut s: win::MemoryStatusEx = std::mem::zeroed();
        s.length = std::mem::size_of::<win::MemoryStatusEx>() as u32;
        if win::GlobalMemoryStatusEx(&mut s) != 0 {
            return Some(s.avail_phys);
        }
    }
    None
}

/// (current, peak) memory used by this program in bytes.
pub fn process() -> Option<(u64, u64)> {
    #[cfg(windows)]
    unsafe {
        let mut c: win::ProcessMemoryCounters = std::mem::zeroed();
        c.cb = std::mem::size_of::<win::ProcessMemoryCounters>() as u32;
        if win::K32GetProcessMemoryInfo(win::GetCurrentProcess(), &mut c, c.cb) != 0 {
            return Some((c.working_set_size as u64, c.peak_working_set_size as u64));
        }
    }
    None
}

pub fn gb(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0 * 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gb_converts_bytes() {
        assert_eq!(gb(0), 0.0);
        assert_eq!(gb(1024 * 1024 * 1024), 1.0);
        assert_eq!(gb(3 * 1024 * 1024 * 1024 / 2), 1.5);
    }

    #[cfg(windows)]
    #[test]
    fn memory_info_is_plausible_on_windows() {
        assert!(available().unwrap() > 0);
        let (cur, peak) = process().unwrap();
        assert!(cur > 0 && peak >= cur);
    }

    #[test]
    fn memory_info_does_not_panic() {
        let _ = available();
        let _ = process();
    }
}
