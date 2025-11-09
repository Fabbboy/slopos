import { McpServer, ResourceTemplate } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import type { CallToolResult, ContentBlock } from '@modelcontextprotocol/sdk/types.js';
import { z } from 'zod';
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import fs from 'node:fs/promises';
import os from 'node:os';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, '../../');
const DEFAULT_TIMEOUT_MS = 60_000;
const DEFAULT_KERNEL_SHELL_TIMEOUT_MS = 120_000;
const DEFAULT_BOOT_TIMEOUT_SEC = 60;
const OUTPUT_CHAR_LIMIT = 40_000;
const OUTPUT_PREVIEW_LIMIT = 6_000;
const LOG_CHAR_LIMIT = 80_000;
const LOG_PAGE_SIZE = 10_000;
const LOG_LIST_PREVIEW_PAGES = 5;
const ANSI_ESCAPE_REGEX = /\u001B\[[0-9;?]*[ -/]*[@-~]/g;
const BOOT_LOG_PATH = path.join(REPO_ROOT, 'test_output.log');
const MESON_LOG_PATH = path.join(REPO_ROOT, 'builddir/meson-logs/meson-log.txt');
const MESON_LOG_LIMIT = 40_000;

const server = new McpServer({
    name: 'slopos-shell',
    version: '0.1.0'
});

const runCommandSchema = z.object({
    command: z.string().min(1, 'command is required'),
    cwd: z.string().optional(),
    timeoutMs: z.number().int().positive().max(300_000).optional(),
    env: z.record(z.string(), z.string()).optional()
});

type RunCommandInput = z.infer<typeof runCommandSchema>;
type ToolInputSchema = Parameters<McpServer['registerTool']>[1]['inputSchema'];
const runCommandInputSchema = runCommandSchema.shape as unknown as ToolInputSchema;

const kernelShellSchema = z.object({
    commands: z.array(z.string().min(1)).nonempty(),
    timeoutMs: z.number().int().positive().max(300_000).optional(),
    bootTimeoutSec: z.number().int().positive().max(600).optional(),
    bootCmdline: z.string().optional()
});

type KernelShellInput = z.infer<typeof kernelShellSchema>;

server.registerTool(
    'shell_run',
    {
        title: 'Run shell command',
        description: 'Execute a non-interactive command inside the SlopOS repository.',
        inputSchema: runCommandInputSchema
    },
    async (rawArgs: unknown) => {
        const args: RunCommandInput = runCommandSchema.parse(rawArgs);
        const cwd = await resolveWorkdir(args.cwd);
        const timeoutMs = args.timeoutMs ?? DEFAULT_TIMEOUT_MS;
        const commandSummary = `bash -lc "${args.command}"`;

        const { stdout, stderr, exitCode, signal, timedOut, durationMs } = await runCommand({
            command: args.command,
            cwd,
            timeoutMs,
            env: args.env
        });

        const summaryLines = [
            `Command: ${args.command}`,
            `cwd: ${cwd}`,
            `timeoutMs: ${timeoutMs}`,
            `exitCode: ${exitCode ?? 'null'}`,
            `signal: ${signal ?? 'none'}`,
            `timedOut: ${timedOut}`,
            `durationMs: ${Math.round(durationMs)}`
        ];

        const content: ContentBlock[] = [
            textBlock(summaryLines.join('\n')),
            ...buildOutputBlocks(stdout, 'stdout'),
            ...buildOutputBlocks(stderr, 'stderr')
        ];

        return {
            content,
            structuredContent: {
                command: commandSummary,
                cwd,
                timeoutMs,
                exitCode,
                signal,
                timedOut,
                durationMs,
                stdout: stdout.text,
                stderr: stderr.text,
                stdoutTruncated: stdout.truncated,
                stderrTruncated: stderr.truncated
            }
        } satisfies CallToolResult;
    }
);

server.registerResource(
    'boot-log',
    'slopos://logs/boot',
    {
        title: 'Latest boot-log capture',
        description: 'Newest slice of test_output.log (same as ?page=latest).'
    },
    async () => {
        const page = await getPaginatedLog(BOOT_LOG_PATH);
        return {
            contents: [
                {
                    uri: 'slopos://logs/boot',
                    text: page.text
                }
            ]
        };
    }
);

const bootLogTemplate = new ResourceTemplate('slopos://logs/boot?page={page}', {
    list: async () => {
        const stats = await getLogStats(BOOT_LOG_PATH);
        const totalPages = stats.totalPages;
        const startPage = Math.max(1, totalPages - (LOG_LIST_PREVIEW_PAGES - 1));
        const resources = [];

        for (let page = startPage; page <= totalPages; page++) {
            resources.push({
                name: `boot-log-page-${page}`,
                uri: buildBootLogUri(page),
                mimeType: 'text/plain',
                description: `Boot log page ${page} of ${totalPages}`
            });
        }

        return { resources };
    }
});

server.registerResource(
    'boot-log-page',
    bootLogTemplate,
    {
        title: 'Boot-log pages',
        description: 'Paginated view of test_output.log; add ?page=N to fetch specific slices.'
    },
    async (uri, variables) => {
        const requestedPage = parsePageVariable(variables);
        const page = await getPaginatedLog(BOOT_LOG_PATH, requestedPage);
        return {
            contents: [
                {
                    uri: uri.href,
                    text: page.text
                }
            ]
        };
    }
);

server.registerTool(
    'kernel_shell',
    {
        title: 'Execute commands inside SlopOS shell',
        description: 'Boots SlopOS via make boot-log, streams provided commands to the serial console, and returns the transcript.',
        inputSchema: kernelShellSchema.shape as unknown as ToolInputSchema
    },
    async (rawArgs: unknown) => {
        const args: KernelShellInput = kernelShellSchema.parse(rawArgs);
        const script = args.commands.join('\n') + '\n';
        const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'slopos-shell-'));
        const scriptPath = path.join(tmpDir, 'shell-script.txt');
        await fs.writeFile(scriptPath, script, 'utf8');

        const envOverrides: Record<string, string> = {};
        envOverrides.BOOT_LOG_TIMEOUT = String(args.bootTimeoutSec ?? DEFAULT_BOOT_TIMEOUT_SEC);
        if (args.bootCmdline) {
            envOverrides.BOOT_CMDLINE = args.bootCmdline;
        }

        const quoted = shellQuote(scriptPath);
        const hostTimeout = args.timeoutMs ?? DEFAULT_KERNEL_SHELL_TIMEOUT_MS;

        try {
            const result = await runCommand({
                command: `cat ${quoted} | make boot-log`,
                cwd: REPO_ROOT,
                timeoutMs: hostTimeout,
                env: envOverrides
            });

            const summary = [
                `Commands piped: ${args.commands.length}`,
                `Host timeoutMs: ${hostTimeout}`,
                `BOOT_LOG_TIMEOUT: ${envOverrides.BOOT_LOG_TIMEOUT ?? '(default)'}`,
                `BOOT_CMDLINE: ${envOverrides.BOOT_CMDLINE ?? '(default)'}`,
                `exitCode: ${result.exitCode ?? 'null'}`,
                `signal: ${result.signal ?? 'none'}`,
                `timedOut: ${result.timedOut}`,
                `durationMs: ${Math.round(result.durationMs)}`
            ];

            return {
                content: [
                    textBlock(summary.join('\n')),
                    textBlock('=== Commands ===\n' + args.commands.join('\n')),
                    ...buildOutputBlocks(result.stdout, 'stdout'),
                    ...buildOutputBlocks(result.stderr, 'stderr')
                ],
                structuredContent: {
                    commands: args.commands,
                    timeoutMs: hostTimeout,
                    bootTimeoutSec: envOverrides.BOOT_LOG_TIMEOUT,
                    bootCmdline: envOverrides.BOOT_CMDLINE,
                    exitCode: result.exitCode,
                    signal: result.signal,
                    timedOut: result.timedOut,
                    durationMs: result.durationMs,
                    stdout: result.stdout.text,
                    stderr: result.stderr.text,
                    stdoutTruncated: result.stdout.truncated,
                    stderrTruncated: result.stderr.truncated
                }
            } satisfies CallToolResult;
        } finally {
            await fs.rm(tmpDir, { recursive: true, force: true });
        }
    }
);

server.registerResource(
    'meson-log',
    'slopos://build/meson-log',
    {
        title: 'Meson build log',
        description: 'Tail of builddir/meson-logs/meson-log.txt for recent build diagnostics.'
    },
    async () => {
        const text = await readSimpleLogTail(MESON_LOG_PATH, MESON_LOG_LIMIT, 'Meson build log');
        return {
            contents: [
                {
                    uri: 'slopos://build/meson-log',
                    text
                }
            ]
        };
    }
);

async function resolveWorkdir(requested?: string): Promise<string> {
    const target = requested
        ? path.isAbsolute(requested)
            ? requested
            : path.resolve(REPO_ROOT, requested)
        : REPO_ROOT;

    const normalized = path.normalize(target);
    const relative = path.relative(REPO_ROOT, normalized);

    if (relative.startsWith('..') || path.isAbsolute(relative)) {
        throw new Error('Requested cwd must stay inside the SlopOS repository.');
    }

    const stats = await fs.stat(normalized).catch(() => undefined);
    if (!stats || !stats.isDirectory()) {
        throw new Error(`cwd does not exist or is not a directory: ${normalized}`);
    }

    return normalized;
}

type OutputBlockOptions = {
    sanitize?: boolean;
    previewLimit?: number;
};

function buildOutputBlocks(output: TrackedOutput, label: 'stdout' | 'stderr', options?: OutputBlockOptions): ContentBlock[] {
    if (!output.text) {
        return [];
    }

    const sanitize = options?.sanitize ?? true;
    const previewLimit = options?.previewLimit ?? OUTPUT_PREVIEW_LIMIT;
    let body = sanitize ? stripAnsi(output.text) : output.text;
    let previewNote = '';

    if (previewLimit > 0 && body.length > previewLimit) {
        body = body.slice(-previewLimit);
        previewNote = `(last ${previewLimit} chars)`;
    }

    const headerParts = [`--- ${label}`];
    if (sanitize) {
        headerParts.push('[ansi stripped]');
    }
    if (output.truncated) {
        headerParts.push('(truncated)');
    }
    if (previewNote) {
        headerParts.push(previewNote);
    }

    const header = headerParts.join(' ');
    return [textBlock(`${header}\n${body}`)];
}

type RunResult = {
    stdout: TrackedOutput;
    stderr: TrackedOutput;
    exitCode: number | null;
    signal: NodeJS.Signals | null;
    timedOut: boolean;
    durationMs: number;
};

type TrackedOutput = { text: string; truncated: boolean };

type RunCommandOptions = {
    command: string;
    cwd: string;
    timeoutMs: number;
    env?: Record<string, string>;
};

async function runCommand(options: RunCommandOptions): Promise<RunResult> {
    const { command, cwd, timeoutMs, env } = options;
    const stdout: TrackedOutput = { text: '', truncated: false };
    const stderr: TrackedOutput = { text: '', truncated: false };
    const start = performance.now();

    const child = spawn('bash', ['-lc', command], {
        cwd,
        env: buildEnv(env),
        stdio: ['ignore', 'pipe', 'pipe']
    });

    const stdoutStream = child.stdout;
    const stderrStream = child.stderr;

    if (!stdoutStream || !stderrStream) {
        child.kill('SIGTERM');
        throw new Error('Failed to capture child process output.');
    }

    stdoutStream.setEncoding('utf8');
    stderrStream.setEncoding('utf8');

    stdoutStream.on('data', chunk => captureText(stdout, chunk));
    stderrStream.on('data', chunk => captureText(stderr, chunk));

    let timedOut = false;
    const killTimeout = setTimeout(() => {
        timedOut = true;
        child.kill('SIGTERM');
        setTimeout(() => child.kill('SIGKILL'), 5_000).unref();
    }, timeoutMs).unref();

    const { exitCode, signal } = await new Promise<{ exitCode: number | null; signal: NodeJS.Signals | null }>((resolve, reject) => {
        child.once('error', reject);
        child.once('close', (code, sig) => resolve({ exitCode: code, signal: sig }));
    }).finally(() => {
        clearTimeout(killTimeout);
    });

    const durationMs = performance.now() - start;

    return {
        stdout,
        stderr,
        exitCode,
        signal,
        timedOut,
        durationMs
    };
}

function captureText(target: TrackedOutput, chunk: string) {
    if (!chunk) {
        return;
    }

    if (target.text.length >= OUTPUT_CHAR_LIMIT) {
        target.truncated = true;
        return;
    }

    const remaining = OUTPUT_CHAR_LIMIT - target.text.length;
    if (chunk.length > remaining) {
        target.text += chunk.slice(0, remaining);
        target.truncated = true;
    } else {
        target.text += chunk;
    }
}

function textBlock(text: string): ContentBlock {
    return {
        type: 'text',
        text
    };
}

function stripAnsi(input: string): string {
    return input.replace(ANSI_ESCAPE_REGEX, '');
}

async function readSimpleLogTail(filePath: string, limit: number, label: string): Promise<string> {
    const stats = await readSanitizedWindow(filePath, limit);
    const lines: string[] = [];
    if (stats.truncated) {
        lines.push(`... truncated to last ${limit} chars of ${stats.totalChars} total ...`);
    }
    lines.push(`${label} tail (${Math.min(limit, stats.windowed.length)} chars shown)`);
    lines.push('');
    lines.push(stats.windowed || '[no data]');
    return lines.join('\n');
}

async function getPaginatedLog(filePath: string, requestedPage?: number) {
    const stats = await getLogStats(filePath);
    const page = clamp(requestedPage ?? stats.totalPages, 1, stats.totalPages);
    const start = (page - 1) * LOG_PAGE_SIZE;
    const slice = stats.windowed.slice(start, Math.min(start + LOG_PAGE_SIZE, stats.windowed.length));
    const headerLines: string[] = [];
    if (stats.truncated) {
        headerLines.push(`... truncated to last ${LOG_CHAR_LIMIT} chars of ${stats.totalChars} total ...`);
    }
    headerLines.push(`Showing page ${page}/${stats.totalPages} (${LOG_PAGE_SIZE} chars per page)`);
    headerLines.push('');
    const textBody = slice.length ? slice : '[no data]';
    return {
        text: `${headerLines.join('\n')}${textBody}`,
        page,
        totalPages: stats.totalPages
    };
}

async function getLogStats(filePath: string) {
    const stats = await readSanitizedWindow(filePath, LOG_CHAR_LIMIT);
    const totalPages = Math.max(1, Math.ceil(Math.max(stats.windowed.length, 1) / LOG_PAGE_SIZE));
    return {
        ...stats,
        totalPages
    };
}

async function readSanitizedWindow(filePath: string, limit: number) {
    let rawText: string;
    try {
        rawText = await fs.readFile(filePath, 'utf8');
    } catch (error) {
        const reason = error instanceof Error ? error.message : String(error);
        rawText = `Unable to read ${filePath}: ${reason}`;
    }

    const sanitized = stripAnsi(rawText);
    const totalChars = sanitized.length;
    let windowed = sanitized;
    let truncated = false;

    if (windowed.length > limit) {
        truncated = true;
        windowed = windowed.slice(-limit);
    }

    return { windowed, totalChars, truncated };
}

function parsePageVariable(variables: Record<string, unknown> | undefined): number | undefined {
    if (!variables) {
        return undefined;
    }
    const raw = variables['page'];
    if (typeof raw !== 'string') {
        return undefined;
    }
    const parsed = Number.parseInt(raw, 10);
    if (!Number.isFinite(parsed) || parsed < 1) {
        return undefined;
    }
    return parsed;
}

function buildBootLogUri(page?: number) {
    if (!page) {
        return 'slopos://logs/boot';
    }
    const params = new URLSearchParams({ page: page.toString() });
    return `slopos://logs/boot?${params.toString()}`;
}

function clamp(value: number, min: number, max: number) {
    return Math.min(Math.max(value, min), max);
}

function shellQuote(value: string) {
    return `'${value.replace(/'/g, `'\\''`)}'`;
}

function buildEnv(overrides?: Record<string, string>) {
    if (!overrides) {
        return process.env;
    }

    const sanitizedEntries = Object.entries(overrides)
        .filter(([key, value]) => Boolean(key) && typeof value === 'string')
        .map(([key, value]) => [key, value]);

    return {
        ...process.env,
        ...Object.fromEntries(sanitizedEntries)
    };
}

let keepAliveHandle: ReturnType<typeof setInterval> | null = null;

async function main() {
    const transport = new StdioServerTransport();
    await server.connect(transport);
    /* Bun exits when the event loop is idle, so keep a dummy timer running */
    keepAliveHandle = setInterval(() => {}, 1 << 30);
}

main().catch(error => {
    console.error('[slopos-mcp] fatal error:', error);
    process.exitCode = 1;
});

process.on('SIGINT', () => {
    if (keepAliveHandle) {
        clearInterval(keepAliveHandle);
    }
    server.close().finally(() => process.exit(0));
});
