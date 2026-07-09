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
    const message = await response
      .json()
      .then((body: { message?: string }) => body.message)
      .catch(() => response.statusText)
    throw new Error(message || response.statusText)
  }

  return response.json() as Promise<T>
}
