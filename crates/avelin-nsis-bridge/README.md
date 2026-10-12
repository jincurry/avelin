# M0 Unicode NSIS bridge

Target: `i686-pc-windows-msvc`, matching the locked NSIS x86 Unicode executor.
This DLL owns no installation operations. Its pipe implementation is shared
with `apps/avelin-app/src/transport.rs` to keep framing and validation identical.

## NSIS API

```nsi
!addplugindir /x86-unicode "path-to-built-dll"
avelin_nsis_bridge::Connect "\\.\pipe\avelin-m0-example" "0123456789abcdef0123456789abcdef" "task-1"
Pop $0 ; OK or error:<category>:<detail>
avelin_nsis_bridge::Emit "running" "1" "3"
Pop $0
avelin_nsis_bridge::CheckPoint
Pop $0 ; continue, cancel, or disconnected
avelin_nsis_bridge::Finish "Succeeded" "0"
Pop $0
```

Plugin arguments are popped left to right. Every function pushes one result.
`LastError` takes no arguments and returns a bounded diagnostic string.
`WaitForCancel "timeout_ms"` is M0-only test instrumentation (maximum 5000 ms),
returning `cancel`, `disconnected`, or `timeout`. Enable it only at an explicitly
requested test checkpoint; production progress never waits for a server ACK.
All input fields are bounded by the NSIS Unicode stack capacity; no license or
language-pack content belongs in this interface.

## Transport contract

- JSONL, UTF-8, maximum 16 KiB per message excluding newline.
- Every packet has `protocol_version: 1`, `task_id`, per-direction `seq`, `kind`.
- Each direction starts at sequence 1 and requires exact next sequence.
- Handshake: bridge `hello` with token; server `hello_ack` with the same token.
- Bridge: `progress` with phase/completed/total; `finish` with result/code.
- Coordinator: `cancel`. Transport consumes `ping`/`pong` heartbeats internally.
- Current-user protected DACL, first pipe instance, remote clients rejected.
- Pipe names must start `\\.\pipe\avelin-m0-`; suffix is ASCII identifier text.
- Tokens contain 32–256 ASCII alphanumeric, hyphen or underscore characters.
- Current-user scope only. The coordinator may additionally validate peer PID.
- Nonblocking native I/O: write timeout 2 s; handshake 5 s in the plugin;
  heartbeat interval 500 ms, stale peer timeout 5 s.
- The coordinator must continue polling `recv_timeout`; it cannot block its
  event loop waiting synchronously for NSIS to exit while the bridge is active.
- No reconnect, impersonation, command execution, resource deletion or rollback.

An I/O thread handles cancellation during long NSIS instructions. Only NSIS
script checkpoints act on it. `Finish` and the registered NSIS unload callback
stop and join that thread before the DLL can unload. Error returns are explicit;
scripts must inspect them and must not equate pipe delivery with committed install.

## Validation

Run the transport's real Windows pipe tests through the app or this crate.
Then load the built DLL from an actual Unicode NSIS package and verify success,
cancel, malformed token, coordinator death during an instruction, and final exit.
The ignored `real_nsis_process_loads_dll_and_reports_finish` test compiles and
runs a no-installation NSIS executable through a real handshake, progress,
deterministic cancellation and terminal message, then verifies prompt exit.
Build the DLL first and run this test explicitly with `--ignored`; its fixtures
are stored in this crate's ignored `.smoke/` directory.
The plugin exports are `cdecl`; the NSIS registration function pointer is
`stdcall`, as specified by the locally installed NSIS plugin headers.

This M0 implementation deliberately uses bounded synchronous polling, not native
asynchronous I/O. CPU use and progress throughput must be measured; production
I/O can move to overlapped operations without changing the wire protocol.
[Microsoft pipe modes](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-type-read-and-wait-modes)
documents partial byte-pipe writes and the compatibility scope of `PIPE_NOWAIT`.
