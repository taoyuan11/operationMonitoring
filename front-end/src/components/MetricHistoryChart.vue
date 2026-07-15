<script setup lang="ts">
import { computed, ref } from 'vue'
import { LoaderCircle } from 'lucide-vue-next'

export type MetricChartPoint = {
  ts: number
  value: number | null
}

type ChartCoordinate = {
  ts: number
  value: number
  x: number
  y: number
}

const props = defineProps<{
  title: string
  points: MetricChartPoint[]
  from: number
  to: number
  bucketSeconds: number
  color: string
  loading: boolean
}>()

const chartWidth = 420
const chartHeight = 132
const chartLeft = 2
const chartRight = 418
const chartTop = 7
const chartBottom = 125
const hoverIndex = ref<number | null>(null)

const coordinates = computed<ChartCoordinate[]>(() => {
  const duration = Math.max(1, props.to - props.from)
  return props.points
    .filter((point): point is { ts: number; value: number } =>
      Number.isFinite(point.ts)
      && typeof point.value === 'number'
      && Number.isFinite(point.value)
      && point.ts >= props.from
      && point.ts <= props.to,
    )
    .sort((left, right) => left.ts - right.ts)
    .map((point) => {
      const value = Math.max(0, Math.min(100, point.value))
      return {
        ...point,
        value,
        x: chartLeft + ((point.ts - props.from) / duration) * (chartRight - chartLeft),
        y: chartBottom - (value / 100) * (chartBottom - chartTop),
      }
    })
})

const linePaths = computed(() => {
  const paths: string[] = []
  let current = ''
  let previous: ChartCoordinate | null = null
  const gapLimit = Math.max(props.bucketSeconds * 2.5, 60)

  for (const point of coordinates.value) {
    if (!previous || point.ts - previous.ts > gapLimit) {
      if (current) paths.push(current)
      current = `M ${point.x.toFixed(2)} ${point.y.toFixed(2)}`
    } else {
      current += ` L ${point.x.toFixed(2)} ${point.y.toFixed(2)}`
    }
    previous = point
  }
  if (current) paths.push(current)
  return paths
})

const activePoint = computed(() => {
  if (!coordinates.value.length) return null
  if (hoverIndex.value !== null && coordinates.value[hoverIndex.value]) {
    return coordinates.value[hoverIndex.value]
  }
  return coordinates.value[coordinates.value.length - 1]
})

const axisLabels = computed(() => [props.from, props.from + (props.to - props.from) / 2, props.to])

function updateHover(event: PointerEvent) {
  if (!coordinates.value.length) return
  const svg = event.currentTarget as SVGSVGElement
  const bounds = svg.getBoundingClientRect()
  const pointerX = ((event.clientX - bounds.left) / Math.max(1, bounds.width)) * chartWidth
  let nearestIndex = 0
  let nearestDistance = Number.POSITIVE_INFINITY
  coordinates.value.forEach((point, index) => {
    const distance = Math.abs(point.x - pointerX)
    if (distance < nearestDistance) {
      nearestDistance = distance
      nearestIndex = index
    }
  })
  hoverIndex.value = nearestIndex
}

function formatValue(value: number | undefined) {
  return typeof value === 'number' ? `${value.toFixed(value >= 10 ? 0 : 1)}%` : '暂无数据'
}

function formatPointTime(timestamp: number | undefined) {
  if (!timestamp) return '所选时段无有效数据'
  return new Date(timestamp * 1000).toLocaleString([], {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  })
}

function formatAxisTime(timestamp: number) {
  const options: Intl.DateTimeFormatOptions = props.to - props.from <= 86400
    ? { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' }
    : { month: '2-digit', day: '2-digit' }
  return new Date(timestamp * 1000).toLocaleString([], options)
}
</script>

<template>
  <article class="metric-history-card" :style="{ '--metric-color': color }">
    <header>
      <div class="metric-history-title">
        <span><slot name="icon"></slot></span>
        <strong>{{ title }}</strong>
      </div>
      <div class="metric-history-current">
        <strong>{{ formatValue(activePoint?.value) }}</strong>
        <small>{{ formatPointTime(activePoint?.ts) }}</small>
      </div>
    </header>

    <div class="metric-chart-shell">
      <svg
        class="metric-chart"
        :class="{ interactive: coordinates.length }"
        :viewBox="`0 0 ${chartWidth} ${chartHeight}`"
        preserveAspectRatio="none"
        role="img"
        :aria-label="`${title}历史使用率折线图`"
        @pointermove="updateHover"
        @pointerleave="hoverIndex = null"
      >
        <line v-for="value in [0, 25, 50, 75, 100]" :key="value" x1="2" x2="418" :y1="chartBottom - (value / 100) * (chartBottom - chartTop)" :y2="chartBottom - (value / 100) * (chartBottom - chartTop)" class="metric-chart-grid" />
        <path
          v-for="(path, index) in linePaths"
          :key="index"
          :d="path"
          class="metric-chart-line"
          vector-effect="non-scaling-stroke"
        />
        <template v-if="activePoint && coordinates.length">
          <line :x1="activePoint.x" :x2="activePoint.x" :y1="chartTop" :y2="chartBottom" class="metric-chart-guide" vector-effect="non-scaling-stroke" />
          <circle :cx="activePoint.x" :cy="activePoint.y" r="4" class="metric-chart-point" vector-effect="non-scaling-stroke" />
        </template>
      </svg>

      <div v-if="loading && !coordinates.length" class="metric-chart-state">
        <LoaderCircle class="spin" :size="17" />正在读取历史数据
      </div>
      <div v-else-if="!coordinates.length" class="metric-chart-state">所选时段暂无数据</div>
    </div>

    <footer>
      <span v-for="label in axisLabels" :key="label">{{ formatAxisTime(label) }}</span>
    </footer>
  </article>
</template>
