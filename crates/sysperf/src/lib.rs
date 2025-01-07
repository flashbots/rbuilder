use alloy_primitives::{keccak256, B256};
use rand::Rng;
use rayon::prelude::*;
use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
    path::Path,
    time::Instant,
};
use sysinfo::{Disks, Networks, System};

// Common result structures
#[derive(Debug)]
pub struct BenchmarkResult {
    pub disk_sequential: DiskBenchmarkResult,
    pub disk_random: DiskBenchmarkResult,
    pub memory: MemoryBandwidthResult,
    pub cpu_single: CpuBenchmarkResult,
    pub cpu_parallel: CpuBenchmarkResult,
}

#[derive(Debug)]
pub struct DiskBenchmarkResult {
    pub throughput_mb_s: f64,
    pub total_bytes_written: u64,
    pub duration_secs: f64,
    pub operations_per_second: f64,
}

#[derive(Debug)]
pub struct MemoryBandwidthResult {
    pub bandwidth_gb_s: f64,
    pub total_bytes_copied: u64,
    pub duration_secs: f64,
}

#[derive(Debug)]
pub struct CpuBenchmarkResult {
    pub hashes_per_second: f64,
    pub total_hashes: u64,
    pub duration_secs: f64,
    pub threads_used: usize,
}

#[derive(Debug)]
pub struct SystemInfoResult {
    pub long_os_version: Option<String>,
    pub kernel_version: Option<String>,
    pub cpu_brand: String,
    pub cpu_count: usize,
    pub total_memory_mb: u64,
    pub used_memory_mb: u64,
    pub disk_info: String,
    pub network_info: String,
}

// Disk Benchmarks
pub fn benchmark_sequential_write(
    path: &Path,
    file_size_mb: u64,
    buffer_size_kb: usize,
) -> std::io::Result<DiskBenchmarkResult> {
    let file_size = file_size_mb * 1024 * 1024;
    let buffer_size = buffer_size_kb * 1024;
    let buffer = vec![42u8; buffer_size];

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;

    let start = Instant::now();
    let mut bytes_written = 0u64;

    while bytes_written < file_size {
        let remaining = file_size - bytes_written;
        let write_size = buffer_size.min(remaining as usize);
        file.write_all(&buffer[..write_size])?;
        bytes_written += write_size as u64;
    }

    file.sync_all()?;

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let throughput_mb_s = (bytes_written as f64) / (1024.0 * 1024.0) / duration_secs;
    let operations_per_second = (bytes_written as f64 / buffer_size as f64) / duration_secs;

    Ok(DiskBenchmarkResult {
        throughput_mb_s,
        total_bytes_written: bytes_written,
        duration_secs,
        operations_per_second,
    })
}

pub fn benchmark_random_write(
    path: &Path,
    file_size_mb: u64,
    operation_size_kb: usize,
    num_operations: u32,
) -> std::io::Result<DiskBenchmarkResult> {
    let file_size = file_size_mb * 1024 * 1024;
    let operation_size = operation_size_kb * 1024;
    let mut rng = rand::thread_rng();
    let buffer = vec![42u8; operation_size];

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;

    file.set_len(file_size)?;

    let start = Instant::now();
    let mut bytes_written = 0u64;

    for _ in 0..num_operations {
        let max_pos = file_size.saturating_sub(operation_size as u64);
        let position = rng.gen_range(0..=max_pos);
        file.seek(SeekFrom::Start(position))?;
        file.write_all(&buffer)?;
        bytes_written += operation_size as u64;
    }

    file.sync_all()?;

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let throughput_mb_s = (bytes_written as f64) / (1024.0 * 1024.0) / duration_secs;
    let operations_per_second = num_operations as f64 / duration_secs;

    Ok(DiskBenchmarkResult {
        throughput_mb_s,
        total_bytes_written: bytes_written,
        duration_secs,
        operations_per_second,
    })
}

// Memory Benchmark
pub fn benchmark_memory_bandwidth(size_mb: u64, iterations: u32) -> MemoryBandwidthResult {
    let size = (size_mb * 1024 * 1024) as usize;
    let src = vec![1u8; size];
    let mut dst = vec![0u8; size];

    let start = Instant::now();
    let mut total_bytes = 0u64;

    for _ in 0..iterations {
        dst.copy_from_slice(&src);
        total_bytes += size as u64;
    }

    assert_ne!(dst[0], 0);

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let bandwidth_gb_s = (total_bytes as f64) / (1024.0 * 1024.0 * 1024.0) / duration_secs;

    MemoryBandwidthResult {
        bandwidth_gb_s,
        total_bytes_copied: total_bytes,
        duration_secs,
    }
}

// CPU Benchmarks
pub fn benchmark_cpu_single(batch_size: u64, data_size: usize) -> CpuBenchmarkResult {
    let test_data = vec![0u8; data_size];
    let start = Instant::now();
    let mut hash_result = B256::default();

    for _ in 0..batch_size {
        hash_result = keccak256(&test_data);
    }

    assert_ne!(hash_result[0], 123);

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();

    CpuBenchmarkResult {
        hashes_per_second: batch_size as f64 / duration_secs,
        total_hashes: batch_size,
        duration_secs,
        threads_used: 1,
    }
}

pub fn benchmark_cpu_parallel(
    batch_size: u64,
    data_size: usize,
    threads: Option<usize>,
) -> CpuBenchmarkResult {
    if let Some(thread_count) = threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .build_global()
            .unwrap();
    }

    let actual_threads = rayon::current_num_threads();
    let chunks = actual_threads * 100;
    let chunk_size = batch_size / chunks as u64;
    let test_data = vec![0u8; data_size];

    let start = Instant::now();

    let results: Vec<[u8; 32]> = (0..chunks)
        .into_par_iter()
        .map(|_| {
            let mut last_hash = B256::default();
            for _ in 0..chunk_size {
                last_hash = keccak256(&test_data);
            }
            last_hash.0
        })
        .collect();

    assert!(!results.is_empty());

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let total_hashes = chunk_size * chunks as u64;

    CpuBenchmarkResult {
        hashes_per_second: total_hashes as f64 / duration_secs,
        total_hashes,
        duration_secs,
        threads_used: actual_threads,
    }
}

// Main benchmark runner
pub fn run_all_benchmarks(
    disk_path: &Path,
    file_size_mb: u64,
    memory_size_mb: u64,
    cpu_batch_size: u64,
) -> std::io::Result<BenchmarkResult> {
    // Run all benchmarks
    let disk_seq = benchmark_sequential_write(disk_path, file_size_mb, 64)?;
    let disk_rand = benchmark_random_write(disk_path, file_size_mb, 4, 10000)?;
    let memory = benchmark_memory_bandwidth(memory_size_mb, 100);
    let cpu_single = benchmark_cpu_single(cpu_batch_size, 1024);
    let cpu_parallel = benchmark_cpu_parallel(cpu_batch_size, 1024, None);

    Ok(BenchmarkResult {
        disk_sequential: disk_seq,
        disk_random: disk_rand,
        memory,
        cpu_single,
        cpu_parallel,
    })
}

pub fn gather_system_info() -> SystemInfoResult {
    let mut sys = System::new_all();
    sys.refresh_all();

    let long_os_version = System::long_os_version();
    let kernel_version = System::kernel_version();
    let cpu_brand = if !sys.cpus().is_empty() {
        sys.cpus()[0].brand().to_string()
    } else {
        "Unknown CPU".to_string()
    };

    let cpu_count = sys.cpus().len();
    let total_memory_mb = sys.total_memory() / 1024;
    let used_memory_mb = sys.used_memory() / 1024;

    let networks = Networks::new_with_refreshed_list();
    let disks = Disks::new_with_refreshed_list();

    // Build the disk_info string
    let mut disk_info = String::from("Disks:\n");
    for disk in disks.list() {
        disk_info.push_str(&format!(
            "  {:?}: {}B total, {}B available\n",
            disk.name(),
            disk.total_space(),
            disk.available_space()
        ));
    }

    // Build the network_info string
    let mut network_info = String::from("Networks:\n");
    for (name, data) in networks.list() {
        network_info.push_str(&format!(
            "  {}: received={}B, transmitted={}B\n",
            name,
            data.total_received(),
            data.total_transmitted()
        ));
    }

    SystemInfoResult {
        long_os_version,
        kernel_version,
        cpu_brand,
        cpu_count,
        total_memory_mb,
        used_memory_mb,
        disk_info,
        network_info,
    }
}

pub fn format_results(result: &BenchmarkResult, sysinfo: &SystemInfoResult) -> String {
    format!(
        "System Information:\n\
         OS Version: {}\n\
         Kernel Version: {}\n\
         CPU: {} ({} cores)\n\
         Total Memory: {} MB\n\
         Used Memory: {} MB\n\
         \nHardware Benchmark Results:\n\
         \nDisk Performance:\
         \n  Sequential Write: {:.2} MB/s ({:.2} ops/s)\
         \n  Random Write: {:.2} MB/s ({:.2} ops/s)\
         \n\nMemory Performance:\
         \n  Bandwidth: {:.2} GB/s\
         \n\nCPU Performance:\
         \n  Single-threaded: {:.2} hashes/s\
         \n  Multi-threaded: {:.2} hashes/s (using {} threads)\
         \n\n{}\
         \n{}",
        sysinfo
            .long_os_version
            .clone()
            .unwrap_or_else(|| "Unknown".to_string()),
        sysinfo
            .kernel_version
            .clone()
            .unwrap_or_else(|| "Unknown".to_string()),
        sysinfo.cpu_brand,
        sysinfo.cpu_count,
        sysinfo.total_memory_mb,
        sysinfo.used_memory_mb,
        result.disk_sequential.throughput_mb_s,
        result.disk_sequential.operations_per_second,
        result.disk_random.throughput_mb_s,
        result.disk_random.operations_per_second,
        result.memory.bandwidth_gb_s,
        result.cpu_single.hashes_per_second,
        result.cpu_parallel.hashes_per_second,
        result.cpu_parallel.threads_used,
        sysinfo.disk_info,
        sysinfo.network_info,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_all_benchmarks() {
        let test_path = Path::new("benchmark_test.tmp");
        let result = run_all_benchmarks(test_path, 100, 100, 1000).unwrap();

        // Basic sanity checks
        assert!(result.disk_sequential.throughput_mb_s > 0.0);
        assert!(result.disk_random.throughput_mb_s > 0.0);
        assert!(result.memory.bandwidth_gb_s > 0.0);
        assert!(result.cpu_single.hashes_per_second > 0.0);
        assert!(result.cpu_parallel.hashes_per_second > 0.0);

        // Cleanup
        fs::remove_file(test_path).unwrap();
    }

    #[test]
    fn test_memory_bandwidth() {
        let result = benchmark_memory_bandwidth(100, 10);

        assert!(result.bandwidth_gb_s > 0.0);
        assert_eq!(result.total_bytes_copied, 100 * 1024 * 1024 * 10);
        assert!(result.duration_secs > 0.0);
    }

    #[test]
    fn test_memory_bandwidth_different_sizes() {
        for size_mb in [50, 100, 200] {
            let result = benchmark_memory_bandwidth(size_mb, 5);
            assert!(result.bandwidth_gb_s > 0.0);
            assert_eq!(result.total_bytes_copied, size_mb * 1024 * 1024 * 5);
        }
    }

    #[test]
    fn test_sequential_write() {
        let test_path = Path::new("test_seq_write.tmp");
        let result = benchmark_sequential_write(test_path, 100, 64).unwrap();

        assert!(result.throughput_mb_s > 0.0);
        assert_eq!(result.total_bytes_written, 100 * 1024 * 1024);
        assert!(result.duration_secs > 0.0);
        assert!(result.operations_per_second > 0.0);

        fs::remove_file(test_path).unwrap();
    }

    #[test]
    fn test_random_write() {
        let test_path = Path::new("test_rand_write.tmp");
        let result = benchmark_random_write(test_path, 100, 4, 1000).unwrap();

        assert!(result.throughput_mb_s > 0.0);
        assert_eq!(result.total_bytes_written, 1000 * 4 * 1024);
        assert!(result.duration_secs > 0.0);
        assert!(result.operations_per_second > 0.0);

        fs::remove_file(test_path).unwrap();
    }

    #[test]
    fn test_random_write_different_operation_sizes() {
        let test_path = Path::new("test_rand_write_ops.tmp");

        // Test with different operation sizes
        for op_size_kb in [2, 4, 8] {
            let result = benchmark_random_write(test_path, 50, op_size_kb, 500).unwrap();
            assert!(result.throughput_mb_s > 0.0);
            assert_eq!(result.total_bytes_written, 500 * op_size_kb as u64 * 1024);
            fs::remove_file(test_path).unwrap();
        }
    }
}
