import type { FileOperationResult } from '../types/domain'

export class UploadCancelledError extends Error {
  constructor() {
    super('上传已取消')
    this.name = 'UploadCancelledError'
  }
}

export type UploadHandle = {
  promise: Promise<FileOperationResult>
  abort: () => void
}

export function uploadInstanceFile(
  instanceId: string,
  parent: string,
  file: File,
  overwrite: boolean,
  onProgress: (transferred: number, total: number) => void,
): UploadHandle {
  const params = new URLSearchParams({
    parent,
    name: file.name,
    overwrite: String(overwrite),
  })
  const xhr = new XMLHttpRequest()
  const promise = new Promise<FileOperationResult>((resolve, reject) => {
    xhr.open('PUT', `/api/admin/instances/${encodeURIComponent(instanceId)}/files/upload?${params}`)
    xhr.withCredentials = true
    xhr.setRequestHeader('Content-Type', file.type || 'application/octet-stream')
    xhr.upload.addEventListener('progress', (event) => {
      onProgress(event.loaded, event.lengthComputable ? event.total : file.size)
    })
    xhr.addEventListener('load', () => {
      const body = parseResponse(xhr.responseText)
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve(body as FileOperationResult)
        return
      }
      reject(new UploadHttpError(body.message || xhr.statusText || '上传失败', xhr.status))
    })
    xhr.addEventListener('error', () => reject(new Error('上传连接发生错误')))
    xhr.addEventListener('abort', () => reject(new UploadCancelledError()))
    xhr.send(file)
  })
  return {
    promise,
    abort: () => xhr.abort(),
  }
}

export class UploadHttpError extends Error {
  readonly status: number

  constructor(message: string, status: number) {
    super(message)
    this.name = 'UploadHttpError'
    this.status = status
  }
}

export function instanceFileDownloadUrl(instanceId: string, path: string) {
  const params = new URLSearchParams({ path })
  return `/api/admin/instances/${encodeURIComponent(instanceId)}/files/download?${params}`
}

function parseResponse(value: string): { message?: string } & Record<string, unknown> {
  try {
    return JSON.parse(value) as { message?: string } & Record<string, unknown>
  } catch {
    return {}
  }
}
