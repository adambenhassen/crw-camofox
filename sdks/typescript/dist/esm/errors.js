/** CRW SDK error types. */
export class CrwError extends Error {
    constructor(message) {
        super(message);
        this.name = "CrwError";
    }
}
export class CrwApiError extends CrwError {
    statusCode;
    constructor(message, statusCode) {
        super(message);
        this.name = "CrwApiError";
        this.statusCode = statusCode;
    }
}
export class CrwTimeoutError extends CrwError {
    constructor(message) {
        super(message);
        this.name = "CrwTimeoutError";
    }
}
export class CrwBinaryNotFoundError extends CrwError {
    constructor(message) {
        super(message);
        this.name = "CrwBinaryNotFoundError";
    }
}
