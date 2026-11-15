/* Agent2API · 报表数据契约、读数口径与热力图布局 */

import { t } from '../i18n'
import { DATE_RANGES, readCustomRange as readDates, saveCustomRange as saveDates, type CustomDates } from '../../../ui-shared/date-range'
export { dayKey, defaultCustomRange, DAY_KEY_RE } from '../../../ui-shared/date-range'

/* ─── 数据形状（后端 /api/stats/summary，字段都可缺省） ─── */

export type StatsSlice = {
  id?: string; model?: string; label?: string; tokens?: number; requests?: number
  /** 积分消耗（`null` / 缺省 = 这一格没有读数，与「0 积分」不同） */
  credit?: number | null
}
export type StatsDay = {
  date?: string; tokens?: number; requests?: number
  models?: StatsSlice[]; clients?: StatsSlice[]; apiKeys?: StatsSlice[]
  /** 当天积分消耗之和（`null` = 这天没有任何积分读数） */
  credit?: number | null
  /** 当天上报了积分的请求条数（`0` = 没有读数） */
  creditRows?: number
  /** 当天「每百万 token 多少积分」（`null` = 算不出：没有积分或没有用量） */
  ppm?: number | null
}
export type StatsHour = {
  hour?: string; rate?: number; totalTokens?: number; hitTokens?: number; inputTokens?: number
  models?: StatsSlice[]
}
/** 排行 / 环形图共用的一条：模型维度用 model 当身份，提供商与账号维度用 id */
export type StatsGroup = {
  id?: string; label?: string; model?: string
  requests?: number; success?: number; failures?: number; totalTokens?: number; tokens?: number
  successRate?: number | null; promptTokens?: number | null; completionTokens?: number | null
  cacheReadTokens?: number | null; cacheRate?: number | null; lastUsedAt?: number | null
  tokenDetailsComplete?: boolean
  source?: string; filterValue?: string; state?: 'enabled' | 'disabled' | 'deleted'
  /** 积分消耗（排行按它排序时才用得上；`null` = 这一维没有读数） */
  credit?: number | null
  creditRows?: number
  /** 每百万 token 多少积分（后端已算好，前端不重复算） */
  ppm?: number | null
}
export type StatsOverview = {
  requests?: number; successful?: number; tokens?: number; activeDays?: number; streak?: number
  topModel?: { model?: string; tokens?: number; percentage?: number } | null
}
export type CacheRateWindow = { inputTokens?: number; hitTokens?: number; rate?: number }

/** 积分预测的原料（近 7 天，固定窗口，与报表区间筛选解耦） */
export type CreditForecast = {
  windowDays?: number
  /** 窗口内有积分读数的天数（日均按它平均，不按 windowDays） */
  creditDays?: number
  points?: StatsDay[]
  /** 近 7 天加权单价（每百万 token 多少积分） */
  weightedPpm?: number | null
  perMillion?: number | null
  perThreeMillion?: number | null
  /** 有读数那几天的日均消耗（`null` = 窗口内没有读数） */
  dailyAvg?: number | null
}

/** `/api/stats/summary` 的 `credits` 键（新增，缺失则整块不渲染） */
export type StatsCredits = {
  total?: number | null
  /** 上报了积分的请求条数 */
  rows?: number
  requests?: number
  successful?: number
  /** 覆盖率 = rows / successful，0–1 */
  coverage?: number
  /** coverage === 1 的布尔投影 */
  complete?: boolean
  /**
   * 多于一家在报读数。`true` 时合计没有单位意义（Cline 的 `usage.cost`
   * 是美元，与积分共用同一列），界面应改成「分家表格」而不是单一总数。
   */
  mixed?: boolean
  tokens?: number
  inputTokens?: number
  /** 每百万 token 多少积分 */
  ppm?: number | null
  /** 每百万输入 token 多少积分 */
  ppmInput?: number | null
  byProvider?: StatsGroup[]
  byAccount?: StatsGroup[]
  byModel?: StatsGroup[]
  range?: { start?: string; end?: string }
  forecast?: CreditForecast
}

export type StatsSummary = {
  range?: string; startDate?: string; endDate?: string
  overview?: StatsOverview
  providers?: StatsGroup[]
  accounts?: StatsGroup[]
  models?: StatsGroup[]
  clients?: StatsGroup[]
  apiKeys?: StatsGroup[]
  apiKeyDetail?: { filterValue?: string; label?: string; requests?: number; totalTokens?: number; detailsComplete?: boolean; clients?: StatsGroup[]; models?: StatsGroup[] } | null
  heatmap?: StatsDay[]
  cacheRates?: Record<string, CacheRateWindow>
  cacheTrend24h?: StatsHour[]
  dailyTrend?: StatsDay[]
  credits?: StatsCredits
}

/* ─── 时间范围（档位与后端白名单逐字一致） ─── */

export const RANGES: readonly string[] = DATE_RANGES
export const DEFAULT_RANGE = '7'
export const CUSTOM_RANGE = 'custom'
export const RANGE_KEY = 'workbuddy-desktop-report-range'
export const CUSTOM_RANGE_KEY = 'workbuddy-desktop-report-custom-range'
export const RANGE_LABEL: Record<string, string> = {
  today: t('今天'), yesterday: t('昨天'), 7: t('近 7 天'), 30: t('近 30 天'), month: t('本月'),
  all: t('全部'), custom: t('自定义区间'),
}
export const RANGE_OPTION_LABEL: Record<string, string> = {
  today: t('今天'), yesterday: t('昨天'), 7: t('7 天'), 30: t('30 天'), month: t('本月'),
  all: t('全部'), custom: t('自定义'),
}

/** 只有明确存过合法值才采纳；无值 / 读取抛错 / 值被改坏都回落默认的 7 天 */
export function readRange(): string {
  try {
    const saved = localStorage.getItem(RANGE_KEY)
    return saved !== null && RANGES.includes(saved) ? saved : DEFAULT_RANGE
  } catch {
    return DEFAULT_RANGE
  }
}

export type CustomRange = CustomDates

export function readCustomRange(): CustomRange {
  return readDates(CUSTOM_RANGE_KEY)
}

export function saveCustomRange(range: CustomRange): void {
  saveDates(CUSTOM_RANGE_KEY, range)
}

export const WEEKDAY_ROWS: readonly (readonly [number, string])[] = [[0, t('一')], [2, t('三')], [4, t('五')]]

/* ─── 读数口径 ─────────────────────────────── */

type UnitsBridge = {
  formatInt?: (value: unknown) => string
  formatTokens?: (value: unknown) => string
  formatAxis?: (value: number, max: number) => string
}

function units(): UnitsBridge {
  return (window as unknown as { wbUnits?: UnitsBridge }).wbUnits || {}
}

export function formatInt(value: unknown): string {
  const api = units()
  return api.formatInt ? api.formatInt(value) : (Number(value) || 0).toLocaleString('zh-CN')
}

/** 每次调用都读单位桥，设置页切换中文量级 / 英文缩写后整页口径保持一致。 */
export function formatTokens(value: unknown): string {
  const api = units()
  return api.formatTokens ? api.formatTokens(value) : String(Number(value) || 0)
}

export function axisText(value: number, max: number): string {
  const api = units()
  return api.formatAxis ? api.formatAxis(value, max) : String(value)
}

/** 后端 rate 是比值而非百分数，且合法地可能超过 1。 */
export function formatPercent(rate: unknown): string {
  return `${((Number(rate) || 0) * 100).toFixed(1)}%`
}

export function round1(value: number): number {
  return Math.round(value * 10) / 10
}

/** 积分读数的展示文本：最多两位小数，去掉无意义的尾零。 */
export function formatCredit(value: unknown): string {
  const number = Number(value)
  if (!Number.isFinite(number)) return '—'
  // 大额积分（几十万）不需要小数位，小额（0.03）需要 —— 按量级选精度
  if (Math.abs(number) >= 1000) return formatInt(Math.round(number))
  if (Math.abs(number) >= 1) return number.toFixed(2).replace(/\.?0+$/, '')
  return number.toFixed(4).replace(/\.?0+$/, '')
}

/**
 * 积分单价的展示文本。
 *
 * 与 `formatCredit` 分开：单价的量级通常是个位到几十（「每百万 token 花
 * 9.99 积分」），固定两位小数比「去掉尾零」更好读 —— 一排单价里有的写
 * `10` 有的写 `9.99`，纵向对不齐。
 */
export function formatCreditRate(value: unknown): string {
  const number = Number(value)
  if (!Number.isFinite(number)) return '—'
  return number >= 100 ? formatInt(Math.round(number)) : number.toFixed(2)
}

export function textWidth(text: string): number {
  return [...text].reduce((sum, ch) => sum + (/[\u2e80-\u9fff]/.test(ch) ? 9.7 : 5.4), 0)
}

/** YYYY-MM-DD 按本地零点解析，避免 UTC 解析改变星期与日界线。 */
export function parseDay(key: unknown): Date {
  const [y, m, d] = String(key || '').split('-').map(Number)
  return new Date(y || 1970, (m || 1) - 1, d || 1)
}

const WEEKDAY_TEXT: readonly string[] = [t('日'), t('一'), t('二'), t('三'), t('四'), t('五'), t('六')]

export function dayLabel(key: unknown): string {
  const date = parseDay(key)
  return t('{m}月{d}日 周{wd}', {
    m: date.getMonth() + 1, d: date.getDate(), wd: WEEKDAY_TEXT[date.getDay()],
  })
}

export function hourText(key: unknown): string {
  const time = String(key || '').slice(11, 13)
  return time ? `${time}:00` : '—'
}

/* ─── 纵轴刻度 ─────────────────────────────── */

const STEP_LADDER = [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10]

function niceStep(value: number): number {
  if (!(value > 0)) return 1
  const pow = 10 ** Math.floor(Math.log10(value))
  const norm = value / pow
  const step = STEP_LADDER.find(item => item >= norm - 1e-9) ?? 10
  return step * pow
}

/** 先选好读的步长再乘四段，避免峰值四等分后产生难读的刻度。 */
export function axisMax(peak: number): number {
  return niceStep(peak / 4) * 4
}

/* ─── 板块一：统计概览 ────────────────────── */

export type OverviewCell = { key: string; label: string; value: string; sub: string; mono: boolean }

export function overviewCells(overview: StatsOverview | undefined, range: string, days: number): OverviewCell[] {
  const data = overview || {}
  const top = data.topModel
  const requests = Number(data.requests) || 0
  const successful = Number(data.successful) || 0
  // 请求数为 0 时成功率没有意义（0/0），给破折号而不是 0%
  const successRate = requests ? `${((successful / requests) * 100).toFixed(1)}%` : '—'

  const cells: OverviewCell[] = [
    { key: 'tokens', label: t('总 Token'), value: formatTokens(data.tokens), sub: t('输入 + 输出'), mono: false },
    { key: 'successful', label: t('成功请求数'), value: formatInt(successful), sub: t('成功率 {rate}', { rate: successRate }), mono: false },
    { key: 'requests', label: t('总请求数'), value: formatInt(requests), sub: RANGE_LABEL[range] || '', mono: false },
    { key: 'activeDays', label: t('活跃天数'), value: formatInt(data.activeDays), sub: t('区间共 {n} 天', { n: days }), mono: false },
    { key: 'streak', label: t('当前连续天数'), value: formatInt(data.streak), sub: t('含今天在内往前数'), mono: false },
  ]
  cells.push(top
    ? {
      key: 'topModel', label: t('Top 模型'), value: String(top.model ?? ''),
      sub: t('{tokens} tokens · 占 {pct}', { tokens: formatTokens(top.tokens), pct: formatPercent(top.percentage) }),
      mono: true,
    }
    : { key: 'topModel', label: t('Top 模型'), value: '—', sub: t('所选范围内还没有模型用量'), mono: false })
  return cells
}

/* ─── 板块一′：积分消耗概览 ──────────────── */

export type CreditCell = { key: string; label: string; value: string; sub: string; mono: boolean }

/**
 * 积分卡的三个读数。`mixed` 时**不出现「总积分」那一个数**——
 * 两家以上在报读数时合计没有单位意义（Cline 的 `usage.cost` 是美元，
 * 与积分共用后端同一列，见 `CREDITS_KEYS` 的说明），改显示覆盖度。
 */
export function creditCells(credits: StatsCredits | undefined): CreditCell[] {
  const data = credits || {}
  const coverage = Math.max(0, Math.min(1, Number(data.coverage) || 0))
  const coverageText = data.complete
    ? t('全部成功请求都有读数')
    : t('覆盖 {pct} 的成功请求', { pct: `${(coverage * 100).toFixed(0)}%` })

  // 混单位时单价同样是**跨单位的商**（分子混着积分与美元，分母是一个合计），
  // 它没有物理意义 —— 与「合计」一起换成下方的分家表，而不是给一个好看的假数。
  const mixedNote = t('见下方按提供商分列')
  const emptyOf = (value: number | null | undefined) => value === null || value === undefined

  return [
    data.mixed
      // 混单位：只报覆盖度，不给一个假的合计
      ? {
        key: 'coverage', label: t('积分覆盖度'), value: `${(coverage * 100).toFixed(0)}%`,
        sub: t('多个提供商使用不同计费单位，按提供商分别查看'), mono: false,
      }
      : {
        key: 'total', label: t('积分消耗'),
        // 一个读数都没有时不写 0：那是「没记录」，不是「免费」
        value: emptyOf(data.total) ? '—' : formatCredit(data.total),
        sub: coverageText, mono: false,
      },
    {
      key: 'ppm', label: t('每百万 Token 积分'),
      value: data.mixed || emptyOf(data.ppm) ? '—' : formatCreditRate(data.ppm),
      sub: data.mixed ? mixedNote : t('输入口径 {rate}', {
        rate: emptyOf(data.ppmInput) ? '—' : formatCreditRate(data.ppmInput),
      }),
      mono: false,
    },
    {
      key: 'perThree', label: t('300 万 Token 约需'),
      value: (() => {
        if (data.mixed || emptyOf(data.total)) return '—'
        // 近 7 天加权单价优先；没有就退回区间单价。两者都可能为 `null`
        // （没有用量 / 没有读数），这时不该按下式换算出「0 积分」。
        const raw = data.forecast?.weightedPpm ?? data.ppm
        if (raw === null || raw === undefined) return '—'
        const rate = Number(raw)
        if (!Number.isFinite(rate)) return '—'
        return t('{n} 积分', { n: formatCredit(rate * 3) })
      })(),
      sub: data.mixed ? mixedNote
        : t('按近 {days} 天单价估算', { days: Number(data.forecast?.windowDays) || 7 }),
      mono: false,
    },
  ]
}

/* ─── 板块二：Top 提供商 / Top 账号排行 ────── */

export type RankRowView = {
  key: string; id: string; name: string
  full: string; tip: string
  tokensText: string; percentText: string; barWidth: string
}

/**
 * `metric` 决定排行按哪个量排与显示哪一列：
 *   - `'tokens'`（默认）是既有行为，单位是 token；
 *   - `'credit'` 读同一批行的 `credit`，只保留**有读数**的行（`null` = 这家
 *     不报积分，出现在积分排行里只会是一行「—」），单位是积分。
 *
 * 两个口径的 `percent` 都按**本表自己的合计**算，所以同一张图里各行的
 * 百分比之和都是 100%（跨口径不可比是必然的：token 占比 ≠ 积分占比）。
 *
 * null 表示契约没有这一维；空数组表示这个区间没有用量。
 */
export function rankRows(list: StatsGroup[] | undefined, metric: 'tokens' | 'credit' = 'tokens'): RankRowView[] | null {
  if (!Array.isArray(list)) return null

  const credit = metric === 'credit'
  const rows = list
    .map(item => {
      // `credit` 用「原始值」判有无：`Number(null)` 是 0，拿转换后的值判
      // 会把「这家不报积分」算进排行（显示成一行 0 积分）
      const raw = item?.credit
      const amount = credit
        ? (raw === null || raw === undefined ? Number.NaN : Number(raw))
        : Number(item?.totalTokens ?? item?.tokens) || 0
      return { id: String(item?.id ?? '').trim(), label: String(item?.label ?? '').trim(), amount }
    })
    .filter(item => Number.isFinite(item.amount) && (credit ? item.amount >= 0 : item.amount > 0))

  if (!rows.length) return []

  const total = rows.reduce((sum, item) => sum + item.amount, 0)
  const share = (count: number) => (total ? (count / total) * 100 : 0)

  // token 口径下请求次数与成功率仍由概览提供；积分口径同理，排行只回答「谁花得多」。
  return [...rows].sort((left, right) => right.amount - left.amount).map((item, index) => {
    const percent = share(item.amount)
    const text = credit ? formatCredit(item.amount) : formatTokens(item.amount)
    const name = item.label || item.id || t('未知')
    const full = item.id ? t('{name}（{id}）', { name, id: item.id }) : name
    return {
      key: `${index}-${item.id || name}`,
      id: item.id,
      name,
      full,
      // token 行沿用既有文案（它的译文早已入库）；积分行才走新键，
      // 两套措辞各自独立，改一个不会牵动另一个
      tip: credit
        ? t('{name}：{amount} {unit} · 占 {pct}%', {
          name: full, amount: text, unit: t('积分'), pct: percent.toFixed(1),
        })
        : t('{name}：{tokens} tokens · 占 {pct}%', { name: full, tokens: text, pct: percent.toFixed(1) }),
      tokensText: text,
      percentText: `${percent.toFixed(1)}%`,
      barWidth: `${percent.toFixed(1)}%`,
    }
  })
}

/** 排行卡右上角的小标题：本表的合计，单位随 `metric` 走。 */
export function rankTotalText(list: StatsGroup[], metric: 'tokens' | 'credit' = 'tokens'): string {
  if (metric === 'credit') {
    const total = list.reduce((sum, item) => {
      const value = item?.credit
      return value === null || value === undefined ? sum : sum + (Number(value) || 0)
    }, 0)
    return `${formatCredit(total)} ${t('积分')}`
  }
  return `${formatTokens(list.reduce(
    (sum, item) => sum + (Number(item?.totalTokens ?? item?.tokens) || 0), 0))} tokens`
}

/* ─── 板块三：用量环形图（模型 / 提供商）──── */

export const SLICE_COLORS: readonly string[] = [
  '#f73b00', '#f7bb07', '#4aaa4d', '#1872cb', '#3444a3', '#13c2c2',
  '#722ed1', '#eb2f96', '#a0d911', '#34d399', '#d946ef', '#ff85c0',
  '#b37feb', '#9d174d', '#ffa39e', '#69b1ff',
]
export const TIP_DOT_FALLBACK = '#9aa0a6'

export type DonutDatum = {
  key: string; name: string; fill: string; tip: string
  tokens: number; requests: number; percentage: number
}
export type DonutView = { size: number; data: DonutDatum[]; totalText: string }

function usageGroups(list: StatsGroup[] | undefined) {
  return (Array.isArray(list) ? list : [])
    .map(item => ({
      name: String(item?.label ?? item?.model ?? item?.id ?? '').trim(),
      tokens: Number(item?.totalTokens ?? item?.tokens) || 0,
      requests: Number(item?.requests) || 0,
    }))
    .filter(item => Number.isFinite(item.tokens) && item.tokens > 0)
    .sort((left, right) => right.tokens - left.tokens)
}

/** 环形图、堆叠柱与悬停读数按同一份用量排名分色。 */
export function modelColorMap(list: StatsGroup[] | undefined): Map<string, string> {
  const map = new Map<string, string>()
  for (const item of usageGroups(list)) {
    if (!item.name || map.has(item.name)) continue
    map.set(item.name, SLICE_COLORS[map.size % SLICE_COLORS.length])
  }
  return map
}

export function donutView(list: StatsGroup[] | undefined, unknownLabel: string): DonutView | null {
  const rows = usageGroups(list)
  if (!rows.length) return null
  const total = rows.reduce((sum, item) => sum + item.tokens, 0)
  if (!total) return null
  const colors = modelColorMap(list)
  const data = rows.map((item, index): DonutDatum => {
    const name = item.name || unknownLabel
    const percentage = item.tokens / total
    return {
      key: `${index}-${name}`, name,
      fill: colors.get(name) ?? SLICE_COLORS[index % SLICE_COLORS.length],
      tokens: item.tokens, requests: item.requests, percentage,
      tip: t('{name} · {tokens} tokens · {n} 次请求 · {pct}', {
        name, tokens: formatTokens(item.tokens), n: formatInt(item.requests), pct: formatPercent(percentage),
      }),
    }
  })
  return { size: 220, data, totalText: formatTokens(total) }
}

/* ─── 板块四：热力图 ──────────────────────── */

const HEAT_STEPS = [1, 1.25, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10]

function heatStepFloor(value: number): number {
  if (!(value > 0)) return 1
  const pow = 10 ** Math.floor(Math.log10(value))
  const norm = value / pow
  let best = 1
  for (const item of HEAT_STEPS) if (item <= norm + 1e-9) best = item
  return best * pow
}

/** 档位由窗口峰值等分，步长向下取好读值，保留绝对用量含义。 */
export function heatLevelsFor(peak: unknown): number[] {
  const top = Number(peak) || 0
  if (top <= 0) return [0, 1, 2, 3, Infinity]
  const step = Math.max(1, heatStepFloor(top / 4))
  return [0, step, step * 2, step * 3, Infinity]
}

export function heatLevelText(level: number, thresholds: number[]): string {
  if (level === 0) return '0'
  const lower = thresholds[level - 1]
  const upper = thresholds[level]
  if (!Number.isFinite(upper)) return `>${formatTokens(lower)}`
  return lower === 0 ? `≤${formatTokens(upper)}` : `${formatTokens(lower)}–${formatTokens(upper)}`
}

export function heatThresholdsOf(days: StatsDay[] | undefined): number[] {
  const list = Array.isArray(days) ? days : []
  return heatLevelsFor(list.reduce((max, day) => Math.max(max, Number(day?.tokens) || 0), 0))
}

export type HeatmapView = {
  width: number
  height: number
  months: { key: string; x: number; text: string }[]
  weekdays: { key: string; x: number; y: number; text: string }[]
  cells: { key: string; level: number; x: number; y: number; size: number; tip: string }[]
}

/** 固定一年，格子在 7–26px 内适配宽度；极窄时交给容器横向滚动。 */
export function heatmapView(days: StatsDay[] | undefined, boxWidth: number): HeatmapView | null {
  const list = Array.isArray(days) ? days : []
  if (!list.length) return null

  const pad = (parseDay(list[0].date).getDay() + 6) % 7
  const cells: (StatsDay | null)[] = new Array(pad).fill(null).concat(list)
  const cols = Math.ceil(cells.length / 7)

  const gap = 3
  const labelW = 20
  const available = boxWidth - labelW
  const fit = Math.floor((available - gap * (cols - 1)) / cols)
  const size = Math.max(7, Math.min(26, fit))
  const topH = size + 3
  const step = size + gap
  const gridW = labelW + cols * step - gap
  const offsetX = Math.max(0, Math.round((boxWidth - gridW) / 2))
  const height = topH + 7 * step - gap + 2
  const width = Math.max(gridW, Math.round(boxWidth))

  const thresholds = heatThresholdsOf(list)
  const levelOf = (tokens: unknown) => thresholds.findIndex(limit => Number(tokens) <= limit)

  const cellsOut: HeatmapView['cells'] = []
  for (let col = 0; col < cols; col += 1) {
    for (let row = 0; row < 7; row += 1) {
      const day = cells[col * 7 + row]
      if (!day) continue
      const requests = Number(day.requests) || 0
      const tokens = Number(day.tokens) || 0
      // 失败请求也算活跃；Token 只决定颜色，不能据此说成「无请求」。
      const tip = requests
        ? t('{date} · {tokens} tokens · {n} 次请求', {
          date: dayLabel(day.date), tokens: formatTokens(tokens), n: formatInt(requests),
        })
        : t('{date} · 无请求', { date: dayLabel(day.date) })
      cellsOut.push({
        key: String(day.date),
        level: levelOf(tokens),
        x: round1(offsetX + labelW + col * step),
        y: round1(topH + row * step),
        size,
        tip,
      })
    }
  }

  const weekdays = WEEKDAY_ROWS.map(([row, text]) => ({
    key: text,
    x: offsetX + labelW - 6,
    y: round1(topH + row * step + size / 2),
    text,
  }))

  const months: HeatmapView['months'] = []
  let lastMonth = -1
  let lastLabelCol = -9
  for (let col = 0; col < cols; col += 1) {
    const head = cells.slice(col * 7, col * 7 + 7).find(Boolean)
    if (!head) continue
    const month = parseDay(head.date).getMonth()
    if (month === lastMonth) continue
    lastMonth = month
    if (col - lastLabelCol < 3) continue
    lastLabelCol = col
    months.push({
      key: `${month}-${col}`, x: round1(offsetX + labelW + col * step),
      text: t('{m}月', { m: month + 1 }),
    })
  }

  return { width, height, months, weekdays, cells: cellsOut }
}

export function heatLegendItems(thresholds: number[]): { level: number; text: string }[] {
  return [0, 1, 2, 3, 4].map(level => ({ level, text: heatLevelText(level, thresholds) }))
}

/* ─── 板块五：缓存命中率四窗口 ────────────── */

export type CacheRateCell = { key: string; label: string; value: string; sub: string }

export function cacheRateCells(rates: Record<string, CacheRateWindow> | undefined): CacheRateCell[] {
  const data = rates || {}
  const windows: [string, string][] = [
    ['last10m', t('近 10 分钟')],
    ['last1h', t('近 1 小时')],
    ['last24h', t('近 24 小时')],
    ['last7d', t('近 7 天')],
  ]
  return windows.map(([key, label]) => {
    const item = data[key] || {}
    const input = Number(item.inputTokens) || 0
    return {
      key,
      label,
      value: input ? formatPercent(item.rate) : '—',
      sub: t('命中 {hit} / 输入 {input}', {
        hit: formatTokens(item.hitTokens), input: formatTokens(input),
      }),
    }
  })
}
