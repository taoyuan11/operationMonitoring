<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { LoaderCircle } from 'lucide-vue-next'
import { LineChart, type LineSeriesOption } from 'echarts/charts'
import {
  AriaComponent,
  GridComponent,
  TooltipComponent,
  type AriaComponentOption,
  type GridComponentOption,
  type TooltipComponentOption,
} from 'echarts/components'
import { init, use, type ComposeOption, type EChartsType } from 'echarts/core'
import { CanvasRenderer } from 'echarts/renderers'

use([LineChart, GridComponent, TooltipComponent, AriaComponent, CanvasRenderer])

export type MetricChartPoint = {
  ts: number
  value: number | null
}

type ValueType = 'percent' | 'milliseconds'
type ChartOption = ComposeOption<
  LineSeriesOption | GridComponentOption | TooltipComponentOption | AriaComponentOption
>
type TooltipEntry = {
  value?: unknown
}

const props = withDefaults(defineProps<{
  title: string
  points: MetricChartPoint[]
  from: number
  to: number
  color: string
  loading: boolean
  valueType?: ValueType
}>(), {
  valueType: 'percent',
})

const chartElement = ref<HTMLDivElement | null>(null)
const hoverIndex = ref<number | null>(null)
const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)')
let chart: EChartsType | null = null
let resizeObserver: ResizeObserver | null = null
let themeObserver: MutationObserver | null = null

const validPoints = computed(() => props.points
  .filter((point): point is { ts: number; value: number } =>
    Number.isFinite(point.ts)
    && typeof point.value === 'number'
    && Number.isFinite(point.value)
    && point.ts >= props.from
    && point.ts <= props.to,
  )
  .sort((left, right) => left.ts - right.ts))

const chartMaxValue = computed(() => {
  if (props.valueType === 'percent') return 100
  const maximum = validPoints.value.reduce((current, point) => Math.max(current, point.value), 0)
  return niceCeiling(Math.max(1, maximum * 1.1))
})

const activePoint = computed(() => {
  if (!validPoints.value.length) return null
  if (hoverIndex.value !== null && validPoints.value[hoverIndex.value]) {
    return validPoints.value[hoverIndex.value]
  }
  return validPoints.value[validPoints.value.length - 1]
})

const axisLabels = computed(() => [props.from, props.from + (props.to - props.from) / 2, props.to])

watch(
  [validPoints, chartMaxValue, () => props.from, () => props.to, () => props.color],
  () => {
    hoverIndex.value = null
    renderChart(true)
  },
)

onMounted(async () => {
  await nextTick()
  if (!chartElement.value) return

  chart = init(chartElement.value, undefined, { renderer: 'canvas' })
  chart.getZr().on('mousemove', updateHover)
  chart.getZr().on('globalout', clearHover)
  resizeObserver = new ResizeObserver(() => chart?.resize())
  resizeObserver.observe(chartElement.value)
  themeObserver = new MutationObserver(() => renderChart(false))
  themeObserver.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ['data-theme'],
  })
  renderChart(false)
})

onBeforeUnmount(() => {
  resizeObserver?.disconnect()
  themeObserver?.disconnect()
  chart?.dispose()
  chart = null
})

function renderChart(animate: boolean) {
  if (!chart) return
  chart.setOption(createChartOption(animate), {
    notMerge: false,
    lazyUpdate: false,
  })
}

function createChartOption(animate: boolean): ChartOption {
  const styles = getComputedStyle(document.documentElement)
  const animationEnabled = animate && !reduceMotion.matches
  return {
    animation: animationEnabled,
    animationDuration: animationEnabled ? 320 : 0,
    animationDurationUpdate: animationEnabled ? 520 : 0,
    animationEasing: 'cubicOut',
    animationEasingUpdate: 'cubicOut',
    aria: {
      enabled: true,
      description: `${props.title}历史折线图，共 ${validPoints.value.length} 个数据点`,
    },
    grid: {
      left: 2,
      right: 2,
      top: 7,
      bottom: 7,
      containLabel: false,
    },
    tooltip: {
      trigger: 'axis',
      confine: true,
      transitionDuration: reduceMotion.matches ? 0 : 0.16,
      backgroundColor: cssVariable(styles, '--surface-menu', '#191f23'),
      borderColor: cssVariable(styles, '--line-strong', 'rgba(255, 255, 255, .15)'),
      borderWidth: 1,
      padding: [7, 9],
      textStyle: {
        color: cssVariable(styles, '--text-strong', '#dce2df'),
        fontFamily: styles.fontFamily,
        fontSize: 10,
      },
      extraCssText: 'border-radius: 5px; box-shadow: 0 8px 24px rgba(0, 0, 0, .22);',
      axisPointer: {
        type: 'line',
        snap: true,
        lineStyle: {
          color: cssVariable(styles, '--line-strong', 'rgba(255, 255, 255, .15)'),
          type: 'dashed',
          width: 1,
        },
      },
      formatter: formatTooltip,
    },
    xAxis: {
      type: 'time',
      min: props.from * 1000,
      max: props.to * 1000,
      boundaryGap: [0, 0],
      show: false,
    },
    yAxis: {
      type: 'value',
      min: 0,
      max: chartMaxValue.value,
      splitNumber: 4,
      axisLabel: { show: false },
      axisLine: { show: false },
      axisTick: { show: false },
      splitLine: {
        show: true,
        lineStyle: {
          color: cssVariable(styles, '--line', 'rgba(255, 255, 255, .085)'),
          width: 1,
        },
      },
    },
    series: [{
      id: 'metric-history',
      name: props.title,
      type: 'line',
      data: validPoints.value.map((point) => [point.ts * 1000, clampValue(point.value)]),
      showSymbol: validPoints.value.length === 1,
      symbol: 'circle',
      symbolSize: 7,
      smooth: 0.22,
      smoothMonotone: 'x',
      connectNulls: false,
      clip: true,
      lineStyle: {
        color: props.color,
        width: 2,
        cap: 'round',
        join: 'round',
        shadowBlur: 6,
        shadowColor: colorWithAlpha(props.color, 0.25),
      },
      itemStyle: {
        color: cssVariable(styles, '--surface-inset', '#0e1215'),
        borderColor: props.color,
        borderWidth: 2,
      },
      emphasis: {
        focus: 'none',
        scale: true,
      },
    }],
  }
}

function updateHover(event: { offsetX: number }) {
  if (!chart || !validPoints.value.length) return
  const timestamp = Number(chart.convertFromPixel({ xAxisIndex: 0 }, event.offsetX)) / 1000
  if (!Number.isFinite(timestamp)) return

  let nearestIndex = 0
  let nearestDistance = Number.POSITIVE_INFINITY
  validPoints.value.forEach((point, index) => {
    const distance = Math.abs(point.ts - timestamp)
    if (distance < nearestDistance) {
      nearestDistance = distance
      nearestIndex = index
    }
  })
  hoverIndex.value = nearestIndex
}

function clearHover() {
  hoverIndex.value = null
}

function formatTooltip(params: unknown) {
  const entry = (Array.isArray(params) ? params[0] : params) as TooltipEntry | undefined
  const values = Array.isArray(entry?.value) ? entry.value : []
  const timestamp = Number(values[0]) / 1000
  const value = Number(values[1])
  if (!Number.isFinite(timestamp) || !Number.isFinite(value)) return ''
  return `${escapeHtml(formatPointTime(timestamp))}<br><strong style="color:${props.color}">${escapeHtml(formatValue(value))}</strong>`
}

function clampValue(value: number) {
  return Math.max(0, Math.min(chartMaxValue.value, value))
}

function formatValue(value: number | undefined) {
  if (typeof value !== 'number') return '暂无数据'
  const formatted = value.toFixed(value >= 10 ? 0 : 1)
  return props.valueType === 'milliseconds' ? `${formatted} ms` : `${formatted}%`
}

function niceCeiling(value: number) {
  const exponent = Math.floor(Math.log10(value))
  const magnitude = 10 ** exponent
  const fraction = value / magnitude
  const niceFraction = fraction <= 1 ? 1 : fraction <= 2 ? 2 : fraction <= 5 ? 5 : 10
  return niceFraction * magnitude
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

function cssVariable(styles: CSSStyleDeclaration, name: string, fallback: string) {
  return styles.getPropertyValue(name).trim() || fallback
}

function colorWithAlpha(color: string, alpha: number) {
  const match = /^#([\da-f]{2})([\da-f]{2})([\da-f]{2})$/i.exec(color)
  if (!match) return color
  return `rgba(${Number.parseInt(match[1], 16)}, ${Number.parseInt(match[2], 16)}, ${Number.parseInt(match[3], 16)}, ${alpha})`
}

function escapeHtml(value: string) {
  return value.replace(/[&<>'"]/g, (character) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    "'": '&#39;',
    '"': '&quot;',
  })[character] || character)
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
      <div
        ref="chartElement"
        class="metric-chart"
        :class="{ interactive: validPoints.length }"
        role="img"
        :aria-label="`${title}历史折线图`"
      ></div>

      <div v-if="loading && !validPoints.length" class="metric-chart-state">
        <LoaderCircle class="spin" :size="17" />正在读取历史数据
      </div>
      <div v-else-if="!validPoints.length" class="metric-chart-state">所选时段暂无数据</div>
    </div>

    <footer>
      <span v-for="label in axisLabels" :key="label">{{ formatAxisTime(label) }}</span>
    </footer>
  </article>
</template>
