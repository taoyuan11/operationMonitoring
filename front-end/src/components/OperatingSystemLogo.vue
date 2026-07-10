<script setup lang="ts">
import { computed, type CSSProperties } from 'vue'
import { Server } from 'lucide-vue-next'
import {
  siAlmalinux,
  siAlpinelinux,
  siAndroid,
  siApple,
  siArchlinux,
  siArtixlinux,
  siBsd,
  siCentos,
  siDebian,
  siDeepin,
  siElementary,
  siEndeavouros,
  siFedora,
  siFreebsd,
  siGarudalinux,
  siGentoo,
  siKalilinux,
  siLinux,
  siLinuxmint,
  siManjaro,
  siNetbsd,
  siNixos,
  siOpenbsd,
  siOpensuse,
  siPopos,
  siRaspberrypi,
  siRedhat,
  siRockylinux,
  siSlackware,
  siSolus,
  siSuse,
  siUbuntu,
  siVoidlinux,
  siZorin,
} from 'simple-icons'
import type { SimpleIcon } from 'simple-icons'

const props = defineProps<{
  os: string
}>()

type LogoIcon = Pick<SimpleIcon, 'title' | 'path' | 'hex'>

type LogoMatch = {
  aliases: readonly string[]
  icon: LogoIcon
  label?: string
  color?: string
}

const windowsLogo: LogoIcon = {
  title: 'Windows',
  hex: '00A4EF',
  path: 'M1 2h9.5v9.5H1V2Zm11.5 0H23v9.5H12.5V2ZM1 12.5h9.5V22H1v-9.5Zm11.5 0H23V22H12.5v-9.5Z',
}

const logoMatches: readonly LogoMatch[] = [
  { aliases: ['windows', 'win32', 'win64', 'mingw', 'msys'], icon: windowsLogo },
  { aliases: ['macos', 'mac os', 'osx', 'os x', 'darwin'], icon: siApple, label: 'macOS', color: '#e8ecef' },
  { aliases: ['ubuntu', 'kubuntu', 'lubuntu', 'xubuntu', 'ubuntu mate'], icon: siUbuntu },
  { aliases: ['centos'], icon: siCentos, color: '#9b4f96' },
  { aliases: ['debian'], icon: siDebian },
  { aliases: ['fedora'], icon: siFedora },
  { aliases: ['rhel', 'red hat', 'redhat'], icon: siRedhat, label: 'Red Hat Enterprise Linux' },
  { aliases: ['alpine', 'alpine linux'], icon: siAlpinelinux },
  { aliases: ['arch', 'arch linux', 'archlinux'], icon: siArchlinux },
  { aliases: ['artix', 'artix linux'], icon: siArtixlinux },
  { aliases: ['rocky', 'rocky linux'], icon: siRockylinux },
  { aliases: ['alma', 'alma linux', 'almalinux'], icon: siAlmalinux, color: '#e8ecef' },
  { aliases: ['opensuse', 'opensuse leap', 'opensuse tumbleweed'], icon: siOpensuse },
  { aliases: ['suse', 'sles'], icon: siSuse },
  { aliases: ['kali', 'kali linux'], icon: siKalilinux },
  { aliases: ['gentoo'], icon: siGentoo },
  { aliases: ['nixos', 'nix os'], icon: siNixos },
  { aliases: ['linux mint', 'linuxmint', 'mint'], icon: siLinuxmint },
  { aliases: ['manjaro'], icon: siManjaro },
  { aliases: ['pop', 'pop os', 'popos'], icon: siPopos, label: 'Pop!_OS' },
  { aliases: ['elementary', 'elementary os'], icon: siElementary },
  { aliases: ['raspbian', 'raspberry pi os'], icon: siRaspberrypi, label: 'Raspberry Pi OS' },
  { aliases: ['slackware'], icon: siSlackware },
  { aliases: ['solus'], icon: siSolus },
  { aliases: ['deepin'], icon: siDeepin },
  { aliases: ['endeavouros', 'endeavour os'], icon: siEndeavouros },
  { aliases: ['garuda', 'garuda linux'], icon: siGarudalinux },
  { aliases: ['void', 'void linux'], icon: siVoidlinux },
  { aliases: ['zorin', 'zorin os'], icon: siZorin },
  { aliases: ['android'], icon: siAndroid },
  { aliases: ['freebsd'], icon: siFreebsd },
  { aliases: ['openbsd'], icon: siOpenbsd },
  { aliases: ['netbsd'], icon: siNetbsd },
  { aliases: ['bsd'], icon: siBsd },
  {
    aliases: ['linux', 'gnu linux', 'amazon linux', 'amzn', 'oracle linux', 'ol', 'clear linux'],
    icon: siLinux,
  },
]

const normalizedOs = computed(() => normalizeOs(props.os))
const logo = computed(() => logoMatches.find((candidate) =>
  candidate.aliases.some((alias) => containsOsName(normalizedOs.value, alias)),
))

const logoLabel = computed(() => logo.value?.label || logo.value?.icon.title || props.os.trim() || '未知系统')
const logoStyle = computed<CSSProperties>(() => ({
  color: logo.value?.color || brandColor(logo.value?.icon.hex),
}))

function normalizeOs(value: string) {
  return value
    .trim()
    .toLocaleLowerCase()
    .replace(/[^a-z0-9]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
}

function containsOsName(value: string, alias: string) {
  const normalizedAlias = normalizeOs(alias)
  return ` ${value} `.includes(` ${normalizedAlias} `)
}

function brandColor(hex: string | undefined) {
  if (!hex) return '#76d4b7'

  const numeric = Number.parseInt(hex, 16)
  const red = (numeric >> 16) & 0xff
  const green = (numeric >> 8) & 0xff
  const blue = numeric & 0xff
  if (Math.max(red, green, blue) < 72) return '#e8ecef'
  return `#${hex}`
}
</script>

<template>
  <span
    class="os-logo"
    :style="logoStyle"
    role="img"
    :aria-label="`${logoLabel} 系统`"
    :title="logoLabel"
  >
    <svg v-if="logo" class="brand-logo" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
      <path :d="logo.icon.path" />
    </svg>
    <Server v-else :size="19" aria-hidden="true" />
  </span>
</template>
