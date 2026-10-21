import { describe, expect, it } from 'vitest'
import { ApiError, errorFromBody, errorMessage, isApiError } from './errors'

describe('errors', () => {
  it('reads the unified error shape', () => {
    const body = { error: { code: 'already_processed', message: '这个文件已被处理过', details: { marker: { profile: 'x' } } } }
    const e = errorFromBody(409, JSON.stringify(body))
    expect(e).toBeInstanceOf(ApiError)
    expect(e.status).toBe(409)
    expect(e.code).toBe('already_processed')
    expect(e.message).toBe('这个文件已被处理过')
    expect(e.details).toEqual({ marker: { profile: 'x' } })
    expect(isApiError(e, 'already_processed')).toBe(true)
    expect(isApiError(e, 'task_exists')).toBe(false)
  })

  it('falls back for other bodies', () => {
    const e = errorFromBody(502, '<html>Bad gateway</html>')
    expect(e.code).toBe('http_error')
    expect(e.message).toBe('请求失败（HTTP 502）')
    expect(errorFromBody(400, '{"error":{}}').code).toBe('http_error')
  })

  it('has a message for anything thrown', () => {
    expect(errorMessage(new ApiError(404, 'not_found', '任务 #3 不存在'))).toBe('任务 #3 不存在')
    expect(errorMessage(new TypeError('Failed to fetch'))).toBe('无法连接到 fncompress 服务')
    expect(errorMessage(new Error('x'))).toBe('x')
    expect(errorMessage('y')).toBe('y')
  })
})
