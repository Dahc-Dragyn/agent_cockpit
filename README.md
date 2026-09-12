# Agent Cockpit 🎛️

A high-performance, lightweight Rust watchdog service and Model Context Protocol (MCP) server for Windows. **Agent Cockpit** inspects active processes holding file locks on workspace binaries (`target\release`, `target\debug`, test runners, cargo daemons) and safely terminates locking processes to eliminate `os error 5 (Access is denied)` during rapid development cycles.

---

## 🎯 Why This Was Built

On Windows environments, the operating system places exclusive mandatory locks on running executable files (`.exe`). When developing, compiling, and testing Rust MCP servers or background binaries within an IDE (such as Antigravity/VS Code):

1. **The Lockout Bottleneck**: If an MCP server or test runner remains active in memory, `cargo build --release` or `cargo test` immediately fails with:
   ```
   error: failed to remove file `target\release\...exe`: Access is denied. (os error 5)
   ```
2. **The Heavy Workaround**: Previously, resolving this required manually hunting down rogue PIDs via Task Manager or restarting the IDE entirely, destroying developer flow.
3. **Autonomous Recovery**: **Agent Cockpit** was engineered to give AI coding agents and developers programmatic, self-healing superpowers. Agents can autonomously inspect lock holders on workspace build directories and terminate orphaned or hanging test runners/servers directly without user friction or IDE reboots.


## 🚀 Features

- **Lock Inspection (`inspect_locks`)**:
  - Scans active system processes for workspace-related executables.
  - Automatically identifies locking culprits: `cargo`, `rustc`, test runners, MCP server binaries, and build artifacts under `target\release` and `target\debug`.
  - Supports custom filtering by process name substring (`process_name`) or executable path substring (`path_filter`).
  - Reports memory footprint (MB) and CPU usage percentage per process.

- **Targeted Process Termination (`kill_process_lock`)**:
  - Terminates locking processes safely by PID.
  - Resolves executable lockouts and file permission errors without requiring full IDE or system reboots.
  - Returns clear diagnostic messages and error feedback on failure or missing PIDs.

- **Native Async Task Telemetry (`inspect_async_tasks` & `get_runtime_health`)**:
  - **Task Starvation Detection**: Inspects real-time Tokio async task poll durations and flags unyielding futures (default threshold > 50ms) across workspace daemons (`pushframe`, `vta`).
  - **Zero-Panic Circular Buffer**: Strictly bounds telemetry memory consumption (< 5MB) using a 5,000-snapshot ring buffer and safe pattern matching (`.get()`, `match`).
  - **Token Economical Summaries**: Truncates output to a maximum of 50 items to protect LLM context windows (e.g., `gemini-3.1-flash-lite`).
  - **Runtime Health**: Reports mean/max poll latencies, tracked task counts, and estimated RAM usage without terminal ANSI clutter.

- **Zero-Crash MCP Transport**:
  - Robust JSON-RPC 2.0 stdio implementation.
  - Full support for MCP initialization handshake (`initialize`, `notifications/initialized`), discovery probes (`server/discover`, `ping`), and tool lifecycle (`tools/list`, `tools/call`).
  - Stream-safe error handling prevents stdio channel drops on malformed inputs.

---

## 📦 Project Structure

```
agent_cockpit/
├── Cargo.toml          # Package configuration & dependencies
├── Cargo.lock
├── README.md           # Documentation
└── src/
    ├── main.rs         # CockpitEngine & MCP JSON-RPC stdio transport
    └── telemetry/
        └── mod.rs      # TelemetryCollector circular buffer & health metrics
```


---

## 🛠️ Build & Installation

### Prerequisites
- **Rust Toolchain**: Stable Rust (Edition 2021) with `cargo`.
- **Operating System**: Windows 10/11 x64.

### Building Release Binary
```powershell
cd "c:\Antigravity projects\Rust\agent_cockpit"
cargo build --release
```
The optimized binary will be compiled to:
`target\release\agent_cockpit.exe`

---

## 🔌 MCP Integration

To register `agent_cockpit` with your MCP client or Antigravity configuration:

### Configuration Snippet (`mcp_config.json`)
```json
{
  "mcpServers": {
    "agent_cockpit": {
      "command": "C:\\Antigravity projects\\Rust\\agent_cockpit\\target\\release\\agent_cockpit.exe",
      "args": []
    }
  }
}
```

---

## 🔧 Tools Reference

### 1. `inspect_locks`
Scans for active processes holding locks on workspace target executables, zombie test runners, or orphaned background tasks.

**Parameters:**
- `process_name` *(string, optional)*: Case-insensitive substring filter for the process name (e.g., `"cargo"`, `"antigravity"`).
- `path_filter` *(string, optional)*: Case-insensitive substring filter matching the binary's executable path.

**Example Output:**
```json
[
  {
    "pid": 20784,
    "name": "agent_cockpit.exe",
    "exe_path": "C:\\Antigravity projects\\Rust\\agent_cockpit\\target\\release\\agent_cockpit.exe",
    "memory_mb": 19,
    "cpu_usage": 0.67
  }
]
```

### 2. `kill_process_lock`
Terminates a specific process by PID to clear filesystem locks.

**Parameters:**
- `pid` *(integer, required)*: Process ID to terminate.
- `force` *(boolean, optional)*: Optional flag for immediate force termination.

**Example Output:**
```
Successfully terminated process 'target_worker.exe' (PID 12345)
```

### 3. `inspect_async_tasks` (New in v0.2.0)
Inspects real-time Tokio asynchronous task execution metrics and flags unyielding futures or task starvation across workspace daemons (`pushframe`, `vta`, `aegis`).

**Parameters:**
- `process_name` *(string, optional)*: Filter by daemon process name (e.g., `"pushframe"`).
- `target_pid` *(integer, optional)*: Specific process ID to inspect.
- `min_poll_duration_ms` *(integer, optional)*: Threshold in milliseconds to flag a future as starved (default: `50`).
- `limit` *(integer, optional)*: Maximum items to return to protect LLM context windows (default: `20`, max: `50`).

**Example Output:**
```json
[
  {
    "task_id": 3,
    "name": "pushframe::render_frame_block",
    "poll_count": 1,
    "last_poll_duration_us": 75000,
    "total_poll_time_us": 75000,
    "idle_time_us": 0,
    "status": "Starved"
  }
]
```

### 4. `get_runtime_health` (New in v0.2.0)
Returns high-level aggregate Tokio runtime health metrics across all tracked asynchronous tasks.

**Parameters:** None.

**Example Output:**
```json
{
  "total_tracked_tasks": 128,
  "starved_tasks_count": 1,
  "mean_poll_duration_us": 420,
  "max_poll_duration_us": 75000,
  "estimated_memory_kb": 15
}
```

---

## 🛡️ License

Internal utility for workspace process management and reliability.

