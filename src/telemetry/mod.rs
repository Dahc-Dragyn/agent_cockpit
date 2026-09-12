use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]

pub enum TaskStatus {
    Running,
    Yielded,
    Starved,
    Completed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskMetricSnapshot {
    pub task_id: u64,
    pub name: String,
    pub poll_count: u64,
    pub last_poll_duration_us: u64,
    pub total_poll_time_us: u64,
    pub idle_time_us: u64,
    pub status: TaskStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RuntimeHealthSummary {
    pub total_tracked_tasks: usize,
    pub starved_tasks_count: usize,
    pub mean_poll_duration_us: u64,
    pub max_poll_duration_us: u64,
    pub estimated_memory_kb: usize,
}

/// A high-performance, strictly bounded circular buffer collector for async task metrics.
/// Adheres strictly to the Mission Success protocol (no unwraps, bounded memory < 5MB).
pub struct TelemetryCollector {
    buffer: VecDeque<TaskMetricSnapshot>,
    max_capacity: usize,
    starvation_threshold_us: u64,
}

impl TelemetryCollector {
    /// Create a new collector with a strict capacity bound.
    /// 10,000 snapshots consume approx ~1.2MB, far below the 5MB ceiling.
    pub fn new(max_capacity: usize, starvation_threshold_ms: u64) -> Self {
        let capacity = if max_capacity == 0 { 1000 } else { max_capacity.min(25_000) };
        Self {
            buffer: VecDeque::with_capacity(capacity),
            max_capacity: capacity,
            starvation_threshold_us: starvation_threshold_ms.saturating_mul(1000),
        }
    }

    /// Record or update a task snapshot without unbounded allocation.
    pub fn record_task(&mut self, snapshot: TaskMetricSnapshot) {
        if self.buffer.len() >= self.max_capacity {
            let _ = self.buffer.pop_front();
        }
        self.buffer.push_back(snapshot);
    }

    /// Retrieve flagged starved or long-running tasks, with a strict max-return cap to protect LLM context windows.
    pub fn get_starved_tasks(&self, min_duration_us: Option<u64>, limit: usize) -> Vec<TaskMetricSnapshot> {
        let threshold = match min_duration_us {
            Some(t) => t,
            None => self.starvation_threshold_us,
        };

        let max_items = if limit == 0 { 20 } else { limit.min(100) };
        let mut starved = Vec::new();

        for i in 0..self.buffer.len() {
            if starved.len() >= max_items {
                break;
            }

            if let Some(task) = self.buffer.get(i) {
                if task.last_poll_duration_us >= threshold || task.status == TaskStatus::Starved {
                    starved.push(task.clone());
                }
            }
        }

        starved
    }

    /// Calculate aggregate runtime health metrics across all captured tasks.
    pub fn get_health_summary(&self) -> RuntimeHealthSummary {
        let total = self.buffer.len();
        if total == 0 {
            return RuntimeHealthSummary {
                total_tracked_tasks: 0,
                starved_tasks_count: 0,
                mean_poll_duration_us: 0,
                max_poll_duration_us: 0,
                estimated_memory_kb: 0,
            };
        }

        let mut sum_poll_us: u128 = 0;
        let mut max_poll_us: u64 = 0;
        let mut starved_count: usize = 0;

        for i in 0..total {
            if let Some(task) = self.buffer.get(i) {
                sum_poll_us = sum_poll_us.saturating_add(task.last_poll_duration_us as u128);
                if task.last_poll_duration_us > max_poll_us {
                    max_poll_us = task.last_poll_duration_us;
                }
                if task.last_poll_duration_us >= self.starvation_threshold_us || task.status == TaskStatus::Starved {
                    starved_count = starved_count.saturating_add(1);
                }
            }
        }

        let mean = (sum_poll_us / (total as u128)) as u64;
        let approx_mem_kb = (total * std::mem::size_of::<TaskMetricSnapshot>()) / 1024;

        RuntimeHealthSummary {
            total_tracked_tasks: total,
            starved_tasks_count: starved_count,
            mean_poll_duration_us: mean,
            max_poll_duration_us: max_poll_us,
            estimated_memory_kb: approx_mem_kb,
        }
    }
}

impl Default for TelemetryCollector {
    fn default() -> Self {
        Self::new(5000, 50) // Default 5000 capacity, 50ms starvation threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]

    async fn test_simulated_task_starvation_detection() {
        let mut collector = TelemetryCollector::new(100, 50);

        // 1. Record healthy short-polling tasks
        collector.record_task(TaskMetricSnapshot {
            task_id: 1,
            name: "pushframe::event_drain".to_string(),
            poll_count: 42,
            last_poll_duration_us: 120, // 0.12ms
            total_poll_time_us: 5000,
            idle_time_us: 45000,
            status: TaskStatus::Yielded,
        });

        collector.record_task(TaskMetricSnapshot {
            task_id: 2,
            name: "vta::agenda_scrape".to_string(),
            poll_count: 10,
            last_poll_duration_us: 850, // 0.85ms
            total_poll_time_us: 8500,
            idle_time_us: 120000,
            status: TaskStatus::Yielded,
        });

        // 2. Simulate time passing and a starved task blocking the runtime for 75ms (75_000us)
        tokio::time::advance(Duration::from_millis(75)).await;

        collector.record_task(TaskMetricSnapshot {
            task_id: 3,
            name: "pushframe::render_frame_block".to_string(),
            poll_count: 1,
            last_poll_duration_us: 75_000, // 75ms > 50ms threshold
            total_poll_time_us: 75_000,
            idle_time_us: 0,
            status: TaskStatus::Starved,
        });

        // Query starved tasks
        let starved = collector.get_starved_tasks(None, 10);
        assert_eq!(starved.len(), 1, "Should flag exactly one starved task");

        let flagged = starved.get(0).expect("Must contain first flagged task");
        assert_eq!(flagged.task_id, 3);
        assert_eq!(flagged.name, "pushframe::render_frame_block");
        assert!(flagged.last_poll_duration_us >= 50_000);

        // Verify aggregate health metrics
        let health = collector.get_health_summary();
        assert_eq!(health.total_tracked_tasks, 3);
        assert_eq!(health.starved_tasks_count, 1);
        assert_eq!(health.max_poll_duration_us, 75_000);
    }

    #[test]
    fn test_strict_circular_buffer_capacity_ceiling() {
        let max_capacity = 5;
        let mut collector = TelemetryCollector::new(max_capacity, 50);

        for i in 1..=10 {
            collector.record_task(TaskMetricSnapshot {
                task_id: i,
                name: format!("task_{}", i),
                poll_count: 1,
                last_poll_duration_us: 500,
                total_poll_time_us: 500,
                idle_time_us: 1000,
                status: TaskStatus::Running,
            });
        }

        assert_eq!(collector.buffer.len(), max_capacity);
        // The oldest elements (1..=5) should be evicted
        let first = collector.buffer.front().expect("Buffer has front element");
        assert_eq!(first.task_id, 6);
        let last = collector.buffer.back().expect("Buffer has back element");
        assert_eq!(last.task_id, 10);
    }
}
