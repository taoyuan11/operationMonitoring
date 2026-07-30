export type ArtifactTargetInference = 'matched' | 'needs_target' | 'needs_architecture'

export type InferredArtifactTarget = {
  os: string
  native_arch: string
  inference: ArtifactTargetInference
}

const architectureSuffix = /(?:^|[_-])(x86_64[-_]musl|x86_64|amd64|x64|aarch64|arm64|armv?7|arm|i[3-6]86|x86)$/

function normalizeArchitecture(os: string, architecture: string) {
  if (os === 'linux') {
    if (architecture === 'x86_64-musl' || architecture === 'x86_64_musl') return 'x86_64-musl'
    if (['x86_64', 'amd64', 'x64'].includes(architecture)) return 'x86_64'
    if (['aarch64', 'arm64'].includes(architecture)) return 'aarch64'
    if (['arm', 'arm7', 'armv7'].includes(architecture)) return 'arm'
    if (/^(?:x86|i[3-6]86)$/.test(architecture)) return 'x86'
  }

  if (os === 'windows') {
    if (['x86_64', 'amd64', 'x64'].includes(architecture)) return 'x64'
    if (['aarch64', 'arm64'].includes(architecture)) return 'arm64'
    if (/^(?:x86|i[3-6]86)$/.test(architecture)) return 'x86'
  }

  if (os === 'macos') {
    if (['aarch64', 'arm64'].includes(architecture)) return 'arm64'
    if (['x86_64', 'amd64', 'x64'].includes(architecture)) return 'x86_64'
  }

  return ''
}

export function inferArtifactTarget(fileName: string): InferredArtifactTarget {
  const executableName = fileName.toLowerCase().replace(/\.sha256$/, '')
  const stem = executableName.replace(/\.(?:bin|exe)$/, '')
  const os = executableName.endsWith('.exe')
    ? 'windows'
    : /(?:^|[._-])(?:macos|darwin|osx)(?=[._-]|$)/.test(stem)
      ? 'macos'
      : /(?:^|[._-])linux(?=[._-]|$)/.test(stem)
        ? 'linux'
        : /(?:^|[._-])(?:windows|win)(?=[._-]|$)/.test(stem)
          ? 'windows'
          : ''
  const architectureAlias = stem.match(architectureSuffix)?.[1] || ''
  const architecture = normalizeArchitecture(os, architectureAlias)

  return {
    os,
    native_arch: architecture,
    inference: os && architecture ? 'matched' : os ? 'needs_architecture' : 'needs_target',
  }
}
