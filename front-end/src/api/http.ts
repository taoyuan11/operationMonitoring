export class ApiError extends Error {
  readonly status: number
  readonly code: string | null

  constructor(message: string, status: number, code: string | null = null) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
}

export async function api<T>(path: string, options: RequestInit = {}): Promise<T> {
  const headers = new Headers(options.headers)
  const isFormData = typeof FormData !== 'undefined' && options.body instanceof FormData
  if (options.body && !isFormData && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json')
  }

  const response = await fetch(path, {
    ...options,
    headers,
    credentials: 'include',
  })

  if (!response.ok) {
    const body = await response
      .json()
      .then((body: { message?: string; code?: string }) => body)
      .catch(() => ({ message: response.statusText, code: undefined }))
    throw new ApiError(body.message || response.statusText, response.status, body.code || null)
  }

  if (response.status === 204) return undefined as T

  const contentType = response.headers.get('content-type') || ''
  if (!contentType.includes('application/json')) return undefined as T

  return response.json() as Promise<T>
}
