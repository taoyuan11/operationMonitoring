export function metricPercent(used: number, total: number) {
  if (!total) return 0
  return Math.max(0, Math.min(100, (used / total) * 100))
}

export function average(values: Array<number | null | undefined>) {
  const valid = values.filter((value): value is number => typeof value === 'number')
  if (!valid.length) return 0
  return valid.reduce((sum, value) => sum + value, 0) / valid.length
}

export function formatPercent(value: number | null | undefined) {
  if (typeof value !== 'number' || Number.isNaN(value)) return '未知'
  return `${value.toFixed(0)}%`
}

export function formatBytes(value: number | null | undefined) {
  if (!value || value < 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  let size = value
  let index = 0
  while (size >= 1024 && index < units.length - 1) {
    size /= 1024
    index += 1
  }
  return `${size.toFixed(size >= 10 || index === 0 ? 0 : 1)} ${units[index]}`
}

export function formatDuration(seconds: number | null | undefined) {
  if (!seconds) return '未知'
  const days = Math.floor(seconds / 86400)
  const hours = Math.floor((seconds % 86400) / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)
  if (days > 0) return `${days}天 ${hours}小时`
  if (hours > 0) return `${hours}小时 ${minutes}分`
  return `${minutes}分`
}

export function formatTime(value: number | null | undefined) {
  if (!value) return '从未'
  return new Date(value * 1000).toLocaleString()
}
