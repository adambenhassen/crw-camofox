/** CRW client — cloud (default), self-hosted HTTP, or local subprocess mode. */
import type { BatchResult, BatchScrapeOptions, Capabilities, ChangeTrackingOptions, ClientOptions, CrawlOptions, CrawlResult, DiffResult, ExtractOptions, ExtractResult, Json, MapOptions, ParseFileOptions, ParseResult, ScrapeOptions, ScrapeResult, SearchOptions, SearchResult } from "./types.js";
export declare const CLOUD_API_URL = "https://api.fastcrw.com";
export declare const DASHBOARD_URL = "https://fastcrw.com/dashboard";
export declare const DOCS_URL = "https://us.github.io/crw";
export declare class CrwClient {
    private apiUrl;
    private apiKey;
    private local;
    /**
     * CRW is cloud-first. With no arguments the client targets the managed cloud
     * (api.fastcrw.com) and needs an API key — sign up for 500 free credits at
     * https://fastcrw.com/dashboard. To self-host locally, set `CRW_LOCAL=1`.
     */
    constructor(opts?: ClientOptions);
    scrape(url: string, opts?: ScrapeOptions): Promise<ScrapeResult>;
    crawl(url: string, opts?: CrawlOptions): Promise<CrawlResult>;
    map(url: string, opts?: MapOptions): Promise<string[]>;
    /**
     * Works in both modes; local mode needs a SearXNG URL configured on the engine.
     */
    search(query: string, opts?: SearchOptions): Promise<SearchResult>;
    /**
     * Parse a document (PDF) into markdown / structured JSON. Works in both modes.
     */
    parseFile(content: Uint8Array, opts?: ParseFileOptions): Promise<ParseResult>;
    /** Structured LLM extraction across URLs (HTTP mode only). */
    extract(opts: ExtractOptions): Promise<ExtractResult>;
    /** Scrape many URLs in one async batch job (HTTP mode only). */
    batchScrape(urls: string[], opts?: BatchScrapeOptions): Promise<BatchResult>;
    /** Feature-detect the engine (HTTP mode only). */
    capabilities(): Promise<Capabilities>;
    /** Diff a page against a prior snapshot (HTTP mode only). */
    changeTrackingDiff(current: Json, previous?: Json, opts?: ChangeTrackingOptions): Promise<DiffResult>;
    /** Shut down the local subprocess if running. */
    close(): void;
    private localTransport;
    private pollLocalCrawl;
    private httpRequest;
    private httpMultipart;
    /** Parse the JSON body; surface a non-2xx body's `error` as CrwApiError. */
    private readJson;
    private httpPost;
    private httpCrawl;
}
