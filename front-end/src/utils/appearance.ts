import type { AppearanceResponse, ResolvedTheme, ThemeMode } from '../types/domain'

const APPEARANCE_CACHE_KEY = 'operation-monitoring.appearance.v1'
export const DEFAULT_ACCENT_COLOR = '#3bbf9b'

type CachedAppearance = Pick<AppearanceResponse, 'theme_mode' | 'accent_color'>

export function readCachedAppearance(): CachedAppearance {
  try {
    const cached = JSON.parse(window.localStorage.getItem(APPEARANCE_CACHE_KEY) || 'null') as Partial<CachedAppearance> | null
    return {
      theme_mode: isThemeMode(cached?.theme_mode) ? cached.theme_mode : 'auto',
      accent_color: normalizeAccentColor(cached?.accent_color) || DEFAULT_ACCENT_COLOR,
    }
  } catch {
    return { theme_mode: 'auto', accent_color: DEFAULT_ACCENT_COLOR }
  }
}

export function cacheAppearance(appearance: CachedAppearance) {
  try {
    window.localStorage.setItem(APPEARANCE_CACHE_KEY, JSON.stringify({
      theme_mode: appearance.theme_mode,
      accent_color: normalizeAccentColor(appearance.accent_color) || DEFAULT_ACCENT_COLOR,
    }))
  } catch {
    // Appearance caching is an optimization; server state remains authoritative.
  }
}

export function bootstrapCachedAppearance() {
  const appearance = readCachedAppearance()
  const systemDark = window.matchMedia('(prefers-color-scheme: dark)').matches
  applyDocumentAppearance(
    appearance.theme_mode === 'auto' ? (systemDark ? 'dark' : 'light') : appearance.theme_mode,
    appearance.theme_mode,
    appearance.accent_color,
  )
}

export function applyDocumentAppearance(
  resolvedTheme: ResolvedTheme,
  themeMode: ThemeMode,
  accentColor: string,
) {
  const root = document.documentElement
  const variables = createAccentVariables(accentColor, resolvedTheme)
  root.dataset.theme = resolvedTheme
  root.dataset.themeMode = themeMode
  root.style.colorScheme = resolvedTheme
  Object.entries(variables).forEach(([name, value]) => root.style.setProperty(name, value))

  const metaThemeColor = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]')
  metaThemeColor?.setAttribute('content', resolvedTheme === 'dark' ? '#0b0e11' : '#f4f7f6')
}

export function normalizeAccentColor(value: unknown): string | null {
  if (typeof value !== 'string') return null
  const normalized = value.trim().toLocaleLowerCase()
  return /^#[0-9a-f]{6}$/.test(normalized) ? normalized : null
}

function isThemeMode(value: unknown): value is ThemeMode {
  return value === 'auto' || value === 'light' || value === 'dark'
}

function createAccentVariables(accentColor: string, theme: ResolvedTheme): Record<string, string> {
  const normalized = normalizeAccentColor(accentColor) || DEFAULT_ACCENT_COLOR
  const background = theme === 'dark' ? '#171c21' : '#ffffff'
  const adjustment = theme === 'dark' ? '#ffffff' : '#000000'
  const accent = ensureContrast(normalized, background, 3, adjustment)
  const accentStrong = ensureContrast(normalized, background, 4.5, adjustment)
  const accentContrast = contrastRatio(accent, '#07120e') >= contrastRatio(accent, '#ffffff')
    ? '#07120e'
    : '#ffffff'

  return {
    '--accent-base': normalized,
    '--accent': accent,
    '--accent-strong': accentStrong,
    '--accent-contrast': accentContrast,
  }
}

function ensureContrast(color: string, background: string, target: number, adjustment: string) {
  if (contrastRatio(color, background) >= target) return color
  for (let percentage = 0.08; percentage <= 1; percentage += 0.08) {
    const candidate = mixColors(color, adjustment, percentage)
    if (contrastRatio(candidate, background) >= target) return candidate
  }
  return adjustment
}

function contrastRatio(left: string, right: string) {
  const light = Math.max(relativeLuminance(left), relativeLuminance(right))
  const dark = Math.min(relativeLuminance(left), relativeLuminance(right))
  return (light + 0.05) / (dark + 0.05)
}

function relativeLuminance(color: string) {
  const [red, green, blue] = hexChannels(color).map((channel) => {
    const normalized = channel / 255
    return normalized <= 0.04045
      ? normalized / 12.92
      : ((normalized + 0.055) / 1.055) ** 2.4
  })
  return 0.2126 * red + 0.7152 * green + 0.0722 * blue
}

function mixColors(left: string, right: string, rightWeight: number) {
  const leftChannels = hexChannels(left)
  const rightChannels = hexChannels(right)
  const channels = leftChannels.map((channel, index) =>
    Math.round(channel * (1 - rightWeight) + rightChannels[index] * rightWeight),
  )
  return `#${channels.map((channel) => channel.toString(16).padStart(2, '0')).join('')}`
}

function hexChannels(color: string): [number, number, number] {
  return [
    Number.parseInt(color.slice(1, 3), 16),
    Number.parseInt(color.slice(3, 5), 16),
    Number.parseInt(color.slice(5, 7), 16),
  ]
}
