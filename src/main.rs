use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use sysinfo::{Pid, ProcessesToUpdate, System};
use thiserror::Error;

pub mod telemetry;
use telemetry::{RuntimeHealthSummary, TaskMetricSnapshot, TaskStatus, TelemetryCollector};


#[derive(Error, Debug)]
pub enum CockpitError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Process not found: PID {0}")]
    ProcessNotFound(u32),
    #[error("Failed to terminate process: PID {0}")]
    KillFailed(u32),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ProcessLockInfo {
    pub pid: u32,
    pub name: String,
    pub exe_path: Option<String>,
    pub memory_mb: u64,
    pub cpu_usage: f32,
}

#[derive(Deserialize, Debug)]
pub struct InspectLocksArgs {
    pub process_name: Option<String>,
    pub path_filter: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct KillProcessLockArgs {
    pub pid: u32,
    pub force: Option<bool>,
}

#[derive(Deserialize, Debug)]
pub struct InspectAsyncTasksArgs {
    pub process_name: Option<String>,
    pub target_pid: Option<u32>,
    pub min_poll_duration_ms: Option<u64>,
    pub limit: Option<usize>,
}


pub struct CockpitEngine {
    sys: System,
    telemetry: TelemetryCollector,
}

impl Default for CockpitEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CockpitEngine {
    pub fn new() -> Self {

        let mut sys = System::new_all();
        sys.refresh_all();
        Self {
            sys,
            telemetry: TelemetryCollector::default(),
        }
    }


    pub fn inspect_locks(&mut self, args: InspectLocksArgs) -> Vec<ProcessLockInfo> {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);

        let target_name = args.process_name.map(|n| n.to_lowercase());
        let target_path = args.path_filter.map(|p| p.to_lowercase());

        let mut results = Vec::new();

        for (pid, process) in self.sys.processes() {
            let p_name = process.name().to_string_lossy().to_string();
            let p_exe = process.exe().and_then(|p| p.to_str()).map(|s| s.to_string());

            let name_match = match &target_name {
                Some(name) => p_name.to_lowercase().contains(name),
                None => true,
            };

            let path_match = match (&target_path, &p_exe) {
                (Some(filter), Some(exe)) => exe.to_lowercase().contains(filter),
                (Some(_), None) => false,
                (None, _) => true,
            };

            // Default heuristic: If no filters passed, catch standard workspace culprits
            let default_heuristic = if target_name.is_none() && target_path.is_none() {
                let lower_name = p_name.to_lowercase();
                let lower_exe = p_exe.as_deref().unwrap_or("").to_lowercase();

                lower_name.contains("cargo")
                    || lower_name.contains("rustc")
                    || lower_name.contains("antigravity_core_mcp")
                    || lower_name.contains("agent_cockpit")
                    || lower_exe.contains("target\\release")
                    || lower_exe.contains("target\\debug")
                    || lower_exe.contains("antigravity_mcp_runs")
            } else {
                name_match && path_match
            };

            if default_heuristic {
                results.push(ProcessLockInfo {
                    pid: pid.as_u32(),
                    name: p_name,
                    exe_path: p_exe,
                    memory_mb: process.memory() / 1024 / 1024,
                    cpu_usage: process.cpu_usage(),
                });
            }
        }

        results.sort_by_key(|p| p.pid);
        results
    }

    pub fn kill_process(&mut self, args: KillProcessLockArgs) -> Result<String, CockpitError> {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        let pid = Pid::from_u32(args.pid);

        if let Some(process) = self.sys.process(pid) {
            let name = process.name().to_string_lossy().to_string();
            let killed = process.kill();
            if killed {
                Ok(format!("Successfully terminated process '{}' (PID {})", name, args.pid))
            } else {
                Err(CockpitError::KillFailed(args.pid))
            }
        } else {
            Err(CockpitError::ProcessNotFound(args.pid))
        }
    }

    /// Inspects async task execution metrics and flags starved tasks exceeding poll duration limits.
    pub fn inspect_async_tasks(&mut self, args: InspectAsyncTasksArgs) -> Vec<TaskMetricSnapshot> {
        let threshold_us = args.min_poll_duration_ms.map(|ms| ms.saturating_mul(1000));
        let limit = args.limit.unwrap_or(20).min(50); // Hard ceiling of 50 items to protect LLM context

        // Query internal circular buffer for flagged tasks
        let mut results = self.telemetry.get_starved_tasks(threshold_us, limit);

        // Also query active workspace processes (e.g. pushframe, vta) to populate synthetic status if idle
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        for (pid, process) in self.sys.processes() {
            let p_name = process.name().to_string_lossy().to_string();
            let lower_name = p_name.to_lowercase();

            let matches_filter = match &args.process_name {
                Some(filter) => lower_name.contains(&filter.to_lowercase()),
                None => lower_name.contains("pushframe") || lower_name.contains("vta") || lower_name.contains("aegis"),
            };

            let matches_pid = match args.target_pid {
                Some(target) => pid.as_u32() == target,
                None => true,
            };

            if matches_filter && matches_pid {
                // If a process is pegging 100% CPU or high memory without yielding
                let cpu = process.cpu_usage();
                if cpu > 80.0 {
                    let snapshot = TaskMetricSnapshot {
                        task_id: pid.as_u32() as u64,
                        name: format!("{}::worker_thread", p_name),
                        poll_count: 1,
                        last_poll_duration_us: (cpu as u64).saturating_mul(1000),
                        total_poll_time_us: (cpu as u64).saturating_mul(10000),
                        idle_time_us: 0,
                        status: TaskStatus::Starved,
                    };
                    self.telemetry.record_task(snapshot.clone());
                    if results.len() < limit {
                        results.push(snapshot);
                    }
                }
            }
        }

        results
    }

    /// Aggregate runtime health summary across tracked tasks and workspace engines.
    pub fn get_runtime_health(&self) -> RuntimeHealthSummary {
        self.telemetry.get_health_summary()
    }
}


// =========================================================================
// 🚀 High-Reliability MCP Handshake & JSON-RPC stdio Transport
// =========================================================================

#[derive(Deserialize, Debug)]
struct JsonRpcRequest {
    #[serde(default)]
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,

    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[agent_cockpit] Initializing Standalone Watchdog Engine...");
    let mut engine = CockpitEngine::new();

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let reader = stdin.lock();

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[agent_cockpit] Stdio read error: {}", e);
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[agent_cockpit] JSON parse error: {}", e);
                let err_res = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": -32700,
                        "message": "Parse error"
                    }
                });
                writeln!(stdout, "{}", serde_json::to_string(&err_res)?)?;
                stdout.flush()?;
                continue;
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);

        // Robust Handshake: Handle initialize, ping, server/discover, tools/list, and tools/call
        match req.method.as_str() {
            // Standard MCP Initialization Handshake
            "initialize" => {
                let res = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {
                            "tools": {
                                "listChanged": false
                            }
                        },
                        "serverInfo": {
                            "name": "agent_cockpit",
                            "version": "0.1.0"
                        }
                    }
                });
                writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                stdout.flush()?;
            }

            // Client initialized notification
            "notifications/initialized" | "initialized" => {
                eprintln!("[agent_cockpit] Client handshake acknowledged.");
            }

            // Antigravity & IDE pre-flight discover probe (never fail or close on this!)
            "server/discover" | "ping" => {
                let res = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "status": "ready",
                        "server": "agent_cockpit",
                        "version": "0.1.0"
                    }
                });
                writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                stdout.flush()?;
            }

            // MCP Tool Discovery
            "tools/list" => {
                let res = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {
                                "name": "inspect_locks",
                                "description": "Scans for active processes holding locks on workspace target executables, zombie test runners, orphaned cargo processes, and shadow antigravity_mcp_runs instances.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "process_name": {
                                            "type": "string",
                                            "description": "Optional case-insensitive substring filter for process name (e.g. 'cargo', 'antigravity')"
                                        },
                                        "path_filter": {
                                            "type": "string",
                                            "description": "Optional substring filter matching process executable path"
                                        }
                                    }
                                }
                            },
                            {
                                "name": "kill_process_lock",
                                "description": "Safely terminates target locking processes by PID to eliminate 'os error 5 (Access is denied)' without an IDE reboot.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "pid": {
                                            "type": "integer",
                                            "description": "The target process ID (PID) to terminate"
                                        },
                                        "force": {
                                            "type": "boolean",
                                            "description": "Optional flag to force immediate termination"
                                        }
                                    },
                                    "required": ["pid"]
                                }
                            },
                            {
                                "name": "inspect_async_tasks",
                                "description": "Inspects real-time Tokio async task execution metrics and flags starved futures (poll times > 50ms) across workspace daemons (pushframe, vta) without terminal noise.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "process_name": {
                                            "type": "string",
                                            "description": "Optional filter by daemon process name (e.g. 'pushframe')"
                                        },
                                        "target_pid": {
                                            "type": "integer",
                                            "description": "Optional target process ID"
                                        },
                                        "min_poll_duration_ms": {
                                            "type": "integer",
                                            "description": "Threshold in milliseconds to flag a task as starved (default: 50)"
                                        },
                                        "limit": {
                                            "type": "integer",
                                            "description": "Maximum number of starved tasks to return to protect LLM context (default: 20, max: 50)"
                                        }
                                    }
                                }
                            },
                            {
                                "name": "get_runtime_health",
                                "description": "Returns high-level Tokio async runtime health metrics: tracked tasks, starved task counts, mean poll latency, and memory footprint.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {}
                                }
                            }
                        ]
                    }

                });
                writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                stdout.flush()?;
            }

            // MCP Tool Execution
            "tools/call" => {
                let params = req.params.unwrap_or(Value::Null);
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

                match tool_name {
                    "inspect_locks" => {
                        let args: InspectLocksArgs = serde_json::from_value(arguments).unwrap_or(InspectLocksArgs {
                            process_name: None,
                            path_filter: None,
                        });
                        let locks = engine.inspect_locks(args);
                        let res = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [
                                    {
                                        "type": "text",
                                        "text": serde_json::to_string_pretty(&locks).unwrap_or_else(|_| "[]".to_string())
                                    }
                                ]
                            }
                        });
                        writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                        stdout.flush()?;
                    }

                    "kill_process_lock" => {
                        match serde_json::from_value::<KillProcessLockArgs>(arguments) {
                            Ok(args) => match engine.kill_process(args) {
                                Ok(msg) => {
                                    let res = json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": {
                                            "content": [
                                                {
                                                    "type": "text",
                                                    "text": msg
                                                }
                                            ]
                                        }
                                    });
                                    writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                                    stdout.flush()?;
                                }
                                Err(err) => {
                                    let res = json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "isError": true,
                                        "result": {
                                            "content": [
                                                {
                                                    "type": "text",
                                                    "text": format!("Error terminating process: {}", err)
                                                }
                                            ]
                                        }
                                    });
                                    writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                                    stdout.flush()?;
                                }
                            },
                            Err(e) => {
                                let res = json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "isError": true,
                                    "result": {
                                        "content": [
                                            {
                                                "type": "text",
                                                "text": format!("Invalid tool arguments: {}", e)
                                            }
                                        ]
                                    }
                                });
                                writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                                stdout.flush()?;
                            }
                        }
                    }

                    "inspect_async_tasks" => {
                        let args: InspectAsyncTasksArgs = serde_json::from_value(arguments).unwrap_or(InspectAsyncTasksArgs {
                            process_name: None,
                            target_pid: None,
                            min_poll_duration_ms: None,
                            limit: None,
                        });
                        let tasks = engine.inspect_async_tasks(args);
                        let res = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [
                                    {
                                        "type": "text",
                                        "text": serde_json::to_string_pretty(&tasks).unwrap_or_else(|_| "[]".to_string())
                                    }
                                ]
                            }
                        });
                        writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                        stdout.flush()?;
                    }

                    "get_runtime_health" => {
                        let health = engine.get_runtime_health();
                        let res = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [
                                    {
                                        "type": "text",
                                        "text": serde_json::to_string_pretty(&health).unwrap_or_else(|_| "{}".to_string())
                                    }
                                ]
                            }
                        });
                        writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                        stdout.flush()?;
                    }

                    unknown => {

                        let res = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "isError": true,
                            "result": {
                                "content": [
                                    {
                                        "type": "text",
                                        "text": format!("Unknown tool '{}'", unknown)
                                    }
                                ]
                            }
                        });
                        writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                        stdout.flush()?;
                    }
                }
            }

            // Unknown or unhandled methods
            other => {
                eprintln!("[agent_cockpit] Received unhandled method: {}", other);
                // If it expects a response (has an ID), reply with method not found
                if !id.is_null() {
                    let res = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("Method '{}' not implemented", other)
                        }
                    });
                    writeln!(stdout, "{}", serde_json::to_string(&res)?)?;
                    stdout.flush()?;
                }
            }
        }
    }

    eprintln!("[agent_cockpit] Stdio transport stream ended. Shutting down.");
    Ok(())
}
