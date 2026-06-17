"use strict";
/** CRW SDK error types. */
Object.defineProperty(exports, "__esModule", { value: true });
exports.CrwBinaryNotFoundError = exports.CrwTimeoutError = exports.CrwApiError = exports.CrwError = void 0;
class CrwError extends Error {
    constructor(message) {
        super(message);
        this.name = "CrwError";
    }
}
exports.CrwError = CrwError;
class CrwApiError extends CrwError {
    statusCode;
    constructor(message, statusCode) {
        super(message);
        this.name = "CrwApiError";
        this.statusCode = statusCode;
    }
}
exports.CrwApiError = CrwApiError;
class CrwTimeoutError extends CrwError {
    constructor(message) {
        super(message);
        this.name = "CrwTimeoutError";
    }
}
exports.CrwTimeoutError = CrwTimeoutError;
class CrwBinaryNotFoundError extends CrwError {
    constructor(message) {
        super(message);
        this.name = "CrwBinaryNotFoundError";
    }
}
exports.CrwBinaryNotFoundError = CrwBinaryNotFoundError;
