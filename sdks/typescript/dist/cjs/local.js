"use strict";
/**
 * Local (CRW_LOCAL) subprocess transport: speaks MCP JSON-RPC to a `crw-mcp`
 * binary over stdio. Mirrors the Python SDK's subprocess mode.
 *
 * v1 finds the binary via the `CRW_BINARY` env var or on `PATH`; auto-download
 * (as the Python SDK does) is a fast-follow.
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.LocalTransport = void 0;
const node_child_process_1 = require("node:child_process");
const errors_js_1 = require("./errors.js");
const BINARY_NAME = process.platform === "win32" ? "crw-mcp.exe" : "crw-mcp";
class LocalTransport {
    proc = null;
    nextId = 0;
    pending = new Map();
    buffer = "";
    resolveBinary() {
        const env = process.env.CRW_BINARY;
        if (env)
            return env;
        // Rely on PATH resolution by spawning the bare name; if it ENOENTs the
        // error handler surfaces a clear install hint.
        return BINARY_NAME;
    }
    ensureProcess() {
        if (this.proc && this.proc.exitCode === null)
            return this.proc;
        const bin = this.resolveBinary();
        const proc = (0, node_child_process_1.spawn)(bin, [], { stdio: ["pipe", "pipe", "ignore"] });
        proc.on("error", (err) => {
            const failure = err.code === "ENOENT"
                ? new errors_js_1.CrwBinaryNotFoundError(`crw-mcp binary not found on PATH. Install it (e.g. \`npm i -g crw-mcp\` or ` +
                    `\`cargo install crw-mcp\`) or set CRW_BINARY to its path.`)
                : new errors_js_1.CrwError(`crw-mcp failed to start: ${err.message}`);
            for (const p of this.pending.values())
                p.reject(failure);
            this.pending.clear();
        });
        proc.stdout.setEncoding("utf8");
        proc.stdout.on("data", (chunk) => this.onData(chunk));
        proc.on("exit", () => {
            for (const p of this.pending.values())
                p.reject(new errors_js_1.CrwError("crw-mcp process closed unexpectedly"));
            this.pending.clear();
        });
        this.proc = proc;
        return proc;
    }
    onData(chunk) {
        this.buffer += chunk;
        let idx;
        while ((idx = this.buffer.indexOf("\n")) >= 0) {
            const line = this.buffer.slice(0, idx).trim();
            this.buffer = this.buffer.slice(idx + 1);
            if (!line)
                continue;
            let msg;
            try {
                msg = JSON.parse(line);
            }
            catch {
                continue;
            }
            const id = msg.id;
            if (id === undefined || !this.pending.has(id))
                continue;
            const p = this.pending.get(id);
            this.pending.delete(id);
            if (msg.error) {
                const err = msg.error;
                p.reject(new errors_js_1.CrwApiError(err.message ?? JSON.stringify(msg.error)));
            }
            else {
                p.resolve(msg.result ?? {});
            }
        }
    }
    jsonrpc(method, params) {
        const proc = this.ensureProcess();
        const id = ++this.nextId;
        return new Promise((resolve, reject) => {
            this.pending.set(id, { resolve, reject });
            proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
        });
    }
    async toolCall(name, args) {
        const result = await this.jsonrpc("tools/call", { name, arguments: args });
        const content = result.content?.[0];
        if (!content)
            throw new errors_js_1.CrwError(`Empty response from ${name}`);
        if (result.isError)
            throw new errors_js_1.CrwApiError(content.text ?? "Unknown error");
        return JSON.parse(content.text ?? "{}");
    }
    close() {
        if (this.proc && this.proc.exitCode === null) {
            this.proc.stdin.end();
            this.proc.kill();
        }
        this.proc = null;
    }
}
exports.LocalTransport = LocalTransport;
