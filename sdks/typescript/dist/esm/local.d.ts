/**
 * Local (CRW_LOCAL) subprocess transport: speaks MCP JSON-RPC to a `crw-mcp`
 * binary over stdio. Mirrors the Python SDK's subprocess mode.
 *
 * v1 finds the binary via the `CRW_BINARY` env var or on `PATH`; auto-download
 * (as the Python SDK does) is a fast-follow.
 */
import type { Json } from "./types.js";
export declare class LocalTransport {
    private proc;
    private nextId;
    private pending;
    private buffer;
    private resolveBinary;
    private ensureProcess;
    private onData;
    private jsonrpc;
    toolCall(name: string, args: Json): Promise<Json>;
    close(): void;
}
