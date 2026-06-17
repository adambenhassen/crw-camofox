/** CRW client — cloud (default), self-hosted HTTP, or local subprocess mode. */
import { CrwApiError, CrwError, CrwTimeoutError } from "./errors.js";
import { LocalTransport } from "./local.js";
// CRW is cloud-first: with no explicit apiUrl and no CRW_LOCAL opt-in, the client
// talks to the managed cloud. Mirrors the Python SDK + CLI onboarding.
export const CLOUD_API_URL = "https://api.fastcrw.com";
export const DASHBOARD_URL = "https://fastcrw.com/dashboard";
export const DOCS_URL = "https://us.github.io/crw";
const SIGNUP_NUDGE = `No CRW API key found. CRW uses the managed cloud (${CLOUD_API_URL}) by default.\n` +
    `  -> Sign up at ${DASHBOARD_URL} for 500 free credits — no payment, no monthly ` +
    `reset (GitHub/Google, ~10s) — then set CRW_API_KEY (or pass apiKey).\n` +
    `  -> Prefer to self-host? Set CRW_LOCAL=1 to run the local engine. Docs: ${DOCS_URL}`;
function envTruthy(value) {
    return !!value && !["0", "false", "no", ""].includes(value.trim().toLowerCase());
}
function httpOnlyHint(name, reason) {
    return (`${name}() requires HTTP mode (${reason}). It is not available with CRW_LOCAL=1. ` +
        `Use the cloud (set CRW_API_KEY) or pass apiUrl for a self-hosted server.`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export class CrwClient {
    apiUrl;
    apiKey;
    local = null;
    /**
     * CRW is cloud-first. With no arguments the client targets the managed cloud
     * (api.fastcrw.com) and needs an API key — sign up for 500 free credits at
     * https://fastcrw.com/dashboard. To self-host locally, set `CRW_LOCAL=1`.
     */
    constructor(opts = {}) {
        const env = globalThis.process
            ?.env ?? {};
        this.apiKey = opts.apiKey ?? env.CRW_API_KEY;
        if (envTruthy(env.CRW_LOCAL)) {
            // Self-host opt-in: zero-config local engine (subprocess), no key.
            this.apiUrl = null;
            return;
        }
        const explicitUrl = opts.apiUrl ?? env.CRW_API_URL;
        this.apiUrl = explicitUrl ?? CLOUD_API_URL;
        // Only the managed-cloud default requires a key; an explicit self-hosted
        // server may run without auth.
        if (explicitUrl === undefined && !this.apiKey) {
            throw new CrwError(SIGNUP_NUDGE);
        }
    }
    async scrape(url, opts = {}) {
        const { formats, onlyMainContent = true, includeTags, excludeTags, renderJs, renderer, waitFor, jsonSchema, ...rest } = opts;
        const args = { url, onlyMainContent };
        if (formats)
            args.formats = [...formats];
        if (includeTags)
            args.includeTags = includeTags;
        if (excludeTags)
            args.excludeTags = excludeTags;
        if (renderJs !== undefined)
            args.renderJs = renderJs;
        if (renderer !== undefined)
            args.renderer = renderer;
        if (waitFor !== undefined)
            args.waitFor = waitFor;
        if (jsonSchema !== undefined) {
            args.jsonSchema = jsonSchema;
            const f = args.formats ?? [];
            if (!f.includes("json"))
                args.formats = [...f, "json"];
        }
        Object.assign(args, rest);
        if (this.apiUrl)
            return this.httpPost("/v1/scrape", args);
        return this.localTransport().toolCall("crw_scrape", args);
    }
    async crawl(url, opts = {}) {
        const { maxDepth = 2, maxPages = 10, pollInterval = 2, timeout = 300, ...rest } = opts;
        const args = { url, maxDepth, maxPages, ...rest };
        if (this.apiUrl)
            return this.httpCrawl(args, pollInterval, timeout);
        const result = await this.localTransport().toolCall("crw_crawl", args);
        const jobId = result.id;
        if (!jobId)
            throw new CrwError(`Crawl did not return job ID: ${JSON.stringify(result)}`);
        return this.pollLocalCrawl(jobId, pollInterval, timeout);
    }
    async map(url, opts = {}) {
        const { maxDepth = 2, useSitemap = true, ...rest } = opts;
        const args = { url, maxDepth, useSitemap, ...rest };
        if (this.apiUrl) {
            const data = await this.httpPost("/v1/map", args);
            return data.links ?? [];
        }
        const result = await this.localTransport().toolCall("crw_map", args);
        return result.links ?? [];
    }
    /**
     * Works in both modes; local mode needs a SearXNG URL configured on the engine.
     */
    async search(query, opts = {}) {
        const { limit = 5, lang, tbs, sources, categories, engines, scrapeOptions, ...rest } = opts;
        const args = { query, limit };
        if (lang)
            args.lang = lang;
        if (tbs)
            args.tbs = tbs;
        if (sources)
            args.sources = sources;
        if (categories)
            args.categories = categories;
        if (engines)
            args.engines = engines;
        if (scrapeOptions)
            args.scrapeOptions = scrapeOptions;
        Object.assign(args, rest);
        if (this.apiUrl)
            return this.httpPost("/v1/search", args);
        return this.localTransport().toolCall("crw_search", args);
    }
    /**
     * Parse a document (PDF) into markdown / structured JSON. Works in both modes.
     */
    async parseFile(content, opts = {}) {
        const { filename = "document.pdf", formats, jsonSchema, parsers, ...rest } = opts;
        if (this.apiUrl) {
            const options = {};
            if (formats)
                options.formats = [...formats];
            if (jsonSchema !== undefined)
                options.jsonSchema = jsonSchema;
            if (parsers)
                options.parsers = parsers;
            Object.assign(options, rest);
            const form = new FormData();
            form.append("file", new Blob([content]), filename);
            if (Object.keys(options).length)
                form.append("options", JSON.stringify(options));
            return this.httpMultipart("/v2/parse", form);
        }
        const b64 = Buffer.from(content).toString("base64");
        const args = { filename, contentBase64: b64 };
        if (formats)
            args.formats = [...formats];
        if (jsonSchema !== undefined)
            args.jsonSchema = jsonSchema;
        if (parsers)
            args.parsers = parsers;
        Object.assign(args, rest);
        return this.localTransport().toolCall("crw_parse_file", args);
    }
    /** Structured LLM extraction across URLs (HTTP mode only). */
    async extract(opts) {
        if (!this.apiUrl)
            throw new CrwError(httpOnlyHint("extract", "LLM extract job endpoint"));
        const { urls, prompt, schema, systemPrompt, pollInterval = 2, timeout = 120 } = opts;
        const body = { urls: [...urls] };
        if (prompt !== undefined)
            body.prompt = prompt;
        if (schema !== undefined)
            body.schema = schema;
        if (systemPrompt !== undefined)
            body.systemPrompt = systemPrompt;
        const start = await this.httpRequest("POST", "/v2/extract", body, { raw: true });
        const jobId = start.id;
        if (!jobId)
            throw new CrwError(`extract did not return job ID: ${JSON.stringify(start)}`);
        const deadline = Date.now() + timeout * 1000;
        for (;;) {
            if (Date.now() > deadline)
                throw new CrwTimeoutError(`Extract ${jobId} timed out after ${timeout}s`);
            const status = await this.httpRequest("GET", `/v2/extract/${jobId}`, undefined, { raw: true, checkSuccess: false });
            if (status.status === "completed")
                return status.data ?? {};
            if (status.status === "failed")
                throw new CrwError(`Extract failed: ${status.error ?? "unknown"}`);
            await sleep(pollInterval * 1000);
        }
    }
    /** Scrape many URLs in one async batch job (HTTP mode only). */
    async batchScrape(urls, opts = {}) {
        if (!this.apiUrl)
            throw new CrwError(httpOnlyHint("batchScrape", "batch job endpoint"));
        const { formats, pollInterval = 2, timeout = 300, ...rest } = opts;
        const body = { urls: [...urls], ...rest };
        if (formats)
            body.formats = [...formats];
        const start = await this.httpRequest("POST", "/v2/batch/scrape", body, { raw: true });
        const jobId = start.id;
        if (!jobId)
            throw new CrwError(`Batch scrape did not return job ID: ${JSON.stringify(start)}`);
        const deadline = Date.now() + timeout * 1000;
        for (;;) {
            if (Date.now() > deadline)
                throw new CrwTimeoutError(`Batch scrape ${jobId} timed out after ${timeout}s`);
            const status = await this.httpRequest("GET", `/v2/batch/scrape/${jobId}`, undefined, { raw: true, checkSuccess: false });
            if (status.status === "completed")
                return status.data ?? [];
            if (status.status === "failed")
                throw new CrwError(`Batch scrape failed: ${status.error ?? "unknown"}`);
            await sleep(pollInterval * 1000);
        }
    }
    /** Feature-detect the engine (HTTP mode only). */
    async capabilities() {
        if (!this.apiUrl)
            throw new CrwError(httpOnlyHint("capabilities", "server capabilities endpoint"));
        return this.httpRequest("GET", "/v1/capabilities", undefined, { checkSuccess: false });
    }
    /** Diff a page against a prior snapshot (HTTP mode only). */
    async changeTrackingDiff(current, previous, opts = {}) {
        if (!this.apiUrl)
            throw new CrwError(httpOnlyHint("changeTrackingDiff", "diff endpoint"));
        const { modes, schema, prompt, ...rest } = opts;
        const body = { current, modes: modes ? [...modes] : ["gitDiff"] };
        if (previous !== undefined)
            body.previous = previous;
        if (schema !== undefined)
            body.schema = schema;
        if (prompt !== undefined)
            body.prompt = prompt;
        Object.assign(body, rest);
        return this.httpPost("/v1/change-tracking/diff", body);
    }
    /** Shut down the local subprocess if running. */
    close() {
        this.local?.close();
        this.local = null;
    }
    // --- local (subprocess) mode ---
    localTransport() {
        if (!this.local)
            this.local = new LocalTransport();
        return this.local;
    }
    async pollLocalCrawl(jobId, pollInterval, timeout) {
        const deadline = Date.now() + timeout * 1000;
        for (;;) {
            if (Date.now() > deadline)
                throw new CrwTimeoutError(`Crawl ${jobId} timed out after ${timeout}s`);
            const result = await this.localTransport().toolCall("crw_check_crawl_status", { id: jobId });
            if (result.status === "completed")
                return result.data ?? [];
            if (result.status === "failed")
                throw new CrwError(`Crawl failed: ${result.error ?? "unknown"}`);
            await sleep(pollInterval * 1000);
        }
    }
    // --- HTTP mode ---
    async httpRequest(method, path, body, { raw = false, checkSuccess = true } = {}) {
        if (this.apiUrl === null)
            throw new CrwError("internal: httpRequest in local mode");
        const url = `${this.apiUrl.replace(/\/$/, "")}${path}`;
        const headers = { "Content-Type": "application/json" };
        if (this.apiKey)
            headers.Authorization = `Bearer ${this.apiKey}`;
        const resp = await fetch(url, { method, headers, body: body ? JSON.stringify(body) : undefined });
        const result = await this.readJson(resp);
        if (checkSuccess && result.success === false) {
            throw new CrwApiError(result.error ?? "API error", resp.status);
        }
        if (raw)
            return result;
        return result.data ?? result;
    }
    async httpMultipart(path, form) {
        if (this.apiUrl === null)
            throw new CrwError("internal: httpMultipart in local mode");
        const url = `${this.apiUrl.replace(/\/$/, "")}${path}`;
        const headers = {};
        if (this.apiKey)
            headers.Authorization = `Bearer ${this.apiKey}`;
        const resp = await fetch(url, { method: "POST", headers, body: form });
        const result = await this.readJson(resp);
        if (result.success === false)
            throw new CrwApiError(result.error ?? "API error", resp.status);
        return result.data ?? result;
    }
    /** Parse the JSON body; surface a non-2xx body's `error` as CrwApiError. */
    async readJson(resp) {
        const text = await resp.text();
        let parsed;
        try {
            parsed = text ? JSON.parse(text) : {};
        }
        catch {
            if (!resp.ok)
                throw new CrwApiError(`HTTP ${resp.status}: ${resp.statusText}`, resp.status);
            throw new CrwApiError(`Invalid JSON response (HTTP ${resp.status})`, resp.status);
        }
        if (!resp.ok) {
            const message = parsed.error ?? parsed.message ?? `HTTP ${resp.status}`;
            throw new CrwApiError(message, resp.status);
        }
        return parsed;
    }
    httpPost(path, body) {
        return this.httpRequest("POST", path, body);
    }
    async httpCrawl(args, pollInterval, timeout) {
        const result = await this.httpPost("/v1/crawl", args);
        const jobId = result.id;
        if (!jobId)
            throw new CrwError(`Crawl did not return job ID: ${JSON.stringify(result)}`);
        const deadline = Date.now() + timeout * 1000;
        for (;;) {
            if (Date.now() > deadline)
                throw new CrwTimeoutError(`Crawl ${jobId} timed out after ${timeout}s`);
            const status = await this.httpRequest("GET", `/v1/crawl/${jobId}`, undefined, { raw: true });
            if (status.status === "completed")
                return status.data ?? [];
            if (status.status === "failed")
                throw new CrwError(`Crawl failed: ${status.error ?? "unknown"}`);
            await sleep(pollInterval * 1000);
        }
    }
}
