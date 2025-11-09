# Task: Wire Serial Input Into SlopOS TTY (MCP Shell Support)

## Background
- The MCP server pipes scripted commands into QEMU over `-serial stdio` (see `mcp/src/server.ts`, `kernel_shell` tool).
- SlopOS Shell currently only consumes PS/2 keyboard input via the TTY pipeline; anything typed on the serial port is ignored.
- MCP-driven tests therefore reach the shell prompt but never echo or execute commands.

## Goal
Make serial input go through the same TTY buffer/readline path as PS/2 keyboard characters so both interactive QEMU sessions and MCP automation can drive the shell.

## Requirements
1. **Serial → TTY bridge**
   - When characters arrive on the serial port (COM1), enqueue them in the TTY buffer just like keyboard input.
   - Reuse existing helpers in `drivers/serial.*` and `drivers/tty.*`; avoid duplicating readline logic.

2. **Interrupt-based wakeups**
   - Ensure TTY waiters (shell task) are notified when serial input arrives (similar to `tty_notify_input_ready` for keyboard IRQs).
   - If needed, enable or adjust serial IRQ routing so the handler actually fires using the IOAPIC setup.

3. **Config sanity**
   - Keep PS/2 behavior unchanged. Both keyboard and serial input should co-exist.
   - Add minimal logging (if any) and guard against buffer overflow (e.g., drop characters when full).

4. **Testing guidance**
   - Document how to verify via MCP (`kernel_shell` tool) and via `make boot` manually.
   - Update `AGENTS.md` or relevant comments if new steps are required.

## Acceptance Criteria
- Running `kernel_shell` with commands like `{"commands":["help","ls"]}` produces echoed output in `test_output.log` and the MCP response.
- Interactive `make boot` still accepts keyboard input.
- No regressions in existing drivers (serial init still works, keyboard path unaffected).
