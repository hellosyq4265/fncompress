// The server answers errors as `{ error: { code, message, details } }` with a
// Chinese message meant for the user (ADR 0002); the UI shows it as is.

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly details: Record<string, unknown> = {},
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

/** Builds the error for a non-2xx response body (already read as text). */
export function errorFromBody(status: number, text: string): ApiError {
  try {
    const body = JSON.parse(text)
    const e = body?.error
    if (e && typeof e.code === 'string' && typeof e.message === 'string') {
      const details = e.details && typeof e.details === 'object' ? e.details : {}
      return new ApiError(status, e.code, e.message, details)
    }
  } catch {
    // Not JSON: a proxy or the server being down.
  }
  return new ApiError(status, 'http_error', `请求失败（HTTP ${status}）`)
}

/** What to show for any thrown value. */
export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) return e.message
  if (e instanceof TypeError) return '无法连接到 fncompress 服务'
  if (e instanceof Error) return e.message
  return String(e)
}

export function isApiError(e: unknown, code?: string): e is ApiError {
  return e instanceof ApiError && (code === undefined || e.code === code)
}
