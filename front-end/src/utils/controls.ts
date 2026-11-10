export interface SelectOption {
  value: string | number
  label: string
  disabled?: boolean
}

export function stepNumber(value: number | null | undefined, direction: number, step: number, min: number, max: number) {
  const increment = Number.isFinite(step) && step > 0 ? step : 1
  const base = Number.isFinite(min) ? min : 0
  const current = value == null || !Number.isFinite(value) ? base : value
  const position = (current - base) / increment
  const index = direction > 0 ? Math.floor(position + 1e-9) + 1 : Math.ceil(position - 1e-9) - 1
  return Math.max(min, Math.min(max, Number((base + index * increment).toFixed(10))))
}

export function nextEnabledOption(options: readonly SelectOption[], current: number, direction: number) {
  for (let offset = 1; offset <= options.length; offset++) {
    const index = (current + direction * offset + options.length * 2) % options.length
    if (!options[index]?.disabled) return index
  }
  return -1
}

export function calendarDays(year: number, month: number) {
  const first = new Date(year, month, 1)
  const offset = (first.getDay() + 6) % 7
  return Array.from({ length: 42 }, (_, index) => new Date(year, month, index - offset + 1))
}

export function localDateTime(date: Date) {
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}
