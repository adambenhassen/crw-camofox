/** CRW SDK error types. */
export declare class CrwError extends Error {
    constructor(message: string);
}
export declare class CrwApiError extends CrwError {
    statusCode?: number;
    constructor(message: string, statusCode?: number);
}
export declare class CrwTimeoutError extends CrwError {
    constructor(message: string);
}
export declare class CrwBinaryNotFoundError extends CrwError {
    constructor(message: string);
}
