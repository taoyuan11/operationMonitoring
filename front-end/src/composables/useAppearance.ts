import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import type { AppearanceResponse, ResolvedTheme } from '../types/domain'
import {
  applyDocumentAppearance,
  cacheAppearance,
  DEFAULT_ACCENT_COLOR,
  normalizeAccentColor,
  readCachedAppearance,
} from '../utils/appearance'

export function useAppearance() {
  const cachedAppearance = readCachedAppearance()
  const systemPrefersDark = ref(window.matchMedia('(prefers-color-scheme: dark)').matches)
  const themeMode = ref(cachedAppearance.theme_mode)
  const accentColor = ref(cachedAppearance.accent_color)
  const backgroundImageUrl = ref<string | null>(null)
  const colorSchemeQuery = window.matchMedia('(prefers-color-scheme: dark)')

  const resolvedTheme = computed<ResolvedTheme>(() => {
    if (themeMode.value === 'auto') return systemPrefersDark.value ? 'dark' : 'light'
    return themeMode.value
  })

  const appearanceStyle = computed<Record<string, string>>(() => {
    if (!backgroundImageUrl.value) return {} as Record<string, string>
    return { '--site-background-image': `url("${backgroundImageUrl.value}")` }
  })

  watch(
    [resolvedTheme, themeMode, accentColor],
    ([resolved, mode, accent]) => applyDocumentAppearance(resolved, mode, accent),
    { immediate: true },
  )

  onMounted(() => colorSchemeQuery.addEventListener('change', updateSystemTheme))
  onBeforeUnmount(() => colorSchemeQuery.removeEventListener('change', updateSystemTheme))

  function applyAppearance(appearance: AppearanceResponse) {
    themeMode.value = appearance.theme_mode
    accentColor.value = normalizeAccentColor(appearance.accent_color) || DEFAULT_ACCENT_COLOR
    backgroundImageUrl.value = appearance.background_image_url
    cacheAppearance({ theme_mode: themeMode.value, accent_color: accentColor.value })
  }

  function updateSystemTheme(event: MediaQueryListEvent) {
    systemPrefersDark.value = event.matches
  }

  return {
    themeMode,
    resolvedTheme,
    accentColor,
    backgroundImageUrl,
    appearanceStyle,
    applyAppearance,
  }
}
