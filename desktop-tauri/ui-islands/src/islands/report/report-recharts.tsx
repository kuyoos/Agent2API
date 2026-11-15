import * as React from 'react'
import { Empty } from '@ui'
import {
  Bar, CartesianGrid, Cell, ComposedChart, Line, LineChart, Pie, PieChart,
  ResponsiveContainer, Tooltip as RechartsTooltip, XAxis, YAxis,
} from 'recharts'
import { axisText, formatCreditRate, formatInt, formatPercent, formatTokens, hourText, type DonutView, type StatsDay, type StatsHour } from '../report-charts'
import { t } from '../../i18n'
import { cacheTrendData, chartAxis, creditTrendData, dailyModelLegend, dailyTrendData, dateTick } from './chart-data'
import { CachePointLabels, DailyPointLabels, renderDonutLabel } from './chart-labels'
import { CacheTooltip, CreditTooltip, DailyTooltip, DonutTooltip } from './chart-tooltips'

const tickStyle = { fill: 'var(--text-3)', fontSize: 12 }
const rateColor = 'var(--primary)'
const tokenColor = 'var(--chart-tokens)'
const gridColor = 'var(--border)'

export function UsageDonut({ view, ariaLabel }: { view: DonutView; ariaLabel: string }) {
  const container = React.useRef<HTMLDivElement>(null)
  return (
    <div className='donut-layout'>
      <div className='donut-wrap' ref={container}>
        <ResponsiveContainer width='100%' height={view.size} minWidth={0}>
          <PieChart aria-label={ariaLabel}>
            <Pie data={view.data} dataKey='tokens' nameKey='name' innerRadius='62%' outerRadius='88%'
              paddingAngle={Math.min(2, 180 / view.data.length)} stroke='none'
              label={renderDonutLabel} labelLine={false} isAnimationActive={false}>
              {view.data.map(row => <Cell key={row.key} fill={row.fill} />)}
            </Pie>
            <RechartsTooltip isAnimationActive={false}
              content={props => <DonutTooltip {...props} container={container} />} />
          </PieChart>
        </ResponsiveContainer>
        <div className='donut-center'>
          <div className='donut-center-value'>{view.totalText}</div>
          <div className='donut-center-label'>{t('总 Token')}</div>
        </div>
      </div>
      <div className='donut-legend'>
        {view.data.map(row => (
          <div className='donut-legend-row' data-tip={row.tip} key={row.key}>
            <span className='donut-dot' style={{ background: row.fill }} />
            <span className='donut-legend-name' title={row.name}>{row.name}</span>
            <span className='donut-legend-tokens'>{formatTokens(row.tokens)}</span>
            <span className='donut-legend-percent'>{formatPercent(row.percentage)}</span>
          </div>
        ))}
      </div>
    </div>
  )
}

export function CacheTrendChart({ series, colors }: { series: StatsHour[]; colors: Map<string, string> }) {
  const container = React.useRef<HTMLDivElement>(null)
  const rows = React.useMemo(() => cacheTrendData(series, colors), [series, colors])
  const rateAxis = chartAxis(rows.reduce((peak, row) => Math.max(peak, row.rate), 0), 1)
  const tokenPeak = rows.reduce((peak, row) => Math.max(peak, row.totalTokens), 0)
  const tokenAxis = chartAxis(tokenPeak)
  const hasTokens = tokenPeak > 0
  if (!rows.length) return <Empty>{t('暂无缓存趋势数据')}</Empty>
  return (
    <div className='report-chart-area' ref={container}>
      <ResponsiveContainer width='100%' height='100%' minWidth={0}>
        <LineChart data={rows} margin={{ top: 28, right: 8, left: 0, bottom: 0 }}
          aria-label={t('近 24 小时缓存命中率与总 Token 趋势')}>
          <CartesianGrid strokeDasharray='3 3' stroke={gridColor} vertical={false} />
          <XAxis dataKey='hour' tickFormatter={hourText} tick={tickStyle} tickLine={false}
            axisLine={{ stroke: gridColor }} minTickGap={24} interval='preserveStartEnd'
            padding={{ left: 4, right: 4 }} />
          <YAxis domain={[0, rateAxis.max]} ticks={rateAxis.ticks} allowDataOverflow
            tickFormatter={value => formatPercent(value)} tick={tickStyle} tickLine={false}
            axisLine={false} width={64} />
          {hasTokens ? (
            <YAxis yAxisId='tokens' orientation='right' domain={[0, tokenAxis.max]} ticks={tokenAxis.ticks}
              allowDataOverflow tickFormatter={value => axisText(value, tokenAxis.max)} tick={tickStyle}
              tickLine={false} axisLine={false} width={64} />
          ) : null}
          <RechartsTooltip cursor={{ stroke: gridColor }} isAnimationActive={false} filterNull={false}
            content={props => <CacheTooltip {...props} container={container} />} />
          <Line type='monotone' dataKey='rate' stroke={rateColor} strokeWidth={2}
            dot={{ r: 2.5, fill: rateColor, strokeWidth: 0 }} activeDot={{ r: 4 }} isAnimationActive={false} />
          {hasTokens ? (
            <Line yAxisId='tokens' type='monotone' dataKey='totalTokens' stroke={tokenColor} strokeWidth={2}
              dot={{ r: 2.5, fill: tokenColor, strokeWidth: 0 }} activeDot={{ r: 4 }} isAnimationActive={false} />
          ) : null}
          <CachePointLabels rows={rows} />
        </LineChart>
      </ResponsiveContainer>
    </div>
  )
}

/**
 * 按天积分消耗（左轴）与每百万 Token 积分（右轴）。
 *
 * 两条线各自一条轴：积分的绝对值随用量大小变化，单价才是「贵不贵」的读数，
 * 混在一根轴上会让单价压成贴着 0 的一条平线。
 */
export function CreditTrendChart({ series }: { series: StatsDay[] }) {
  const container = React.useRef<HTMLDivElement>(null)
  const rows = React.useMemo(() => creditTrendData(series), [series])
  const creditPeak = rows.reduce((peak, row) => Math.max(peak, row.credit ?? 0), 0)
  const ratePeak = rows.reduce((peak, row) => Math.max(peak, row.ppm ?? 0), 0)
  const creditAxis = chartAxis(creditPeak)
  const rateAxis = chartAxis(ratePeak, 1)
  const hasRate = ratePeak > 0
  if (!rows.length) return <Empty>{t('暂无积分数据')}</Empty>
  // 整段区间都没有读数（全 null）时折线是空的，给一句话而不是一张空网格。
  // 判「有没有读数」而不是「峰值 > 0」：一家如实报 0 积分是一条真实读数，
  // 它该画出贴着 0 的线，不该被当成「没有数据」。
  if (!rows.some(row => row.credit !== null)) {
    return <div className='report-chart-empty'><Empty>{t('所选范围内没有积分读数')}</Empty></div>
  }
  return (
    <div className='report-chart-area' ref={container}>
      <ResponsiveContainer width='100%' height='100%' minWidth={0}>
        <LineChart data={rows} margin={{ top: 28, right: 8, left: 0, bottom: 0 }}
          aria-label={t('按天积分消耗与每百万 Token 积分趋势')}>
          <CartesianGrid strokeDasharray='3 3' stroke={gridColor} vertical={false} />
          <XAxis dataKey='date' tickFormatter={dateTick} tick={tickStyle} tickLine={false}
            axisLine={{ stroke: gridColor }} minTickGap={20} interval='preserveStartEnd' />
          <YAxis domain={[0, creditAxis.max]} ticks={creditAxis.ticks} allowDataOverflow
            tickFormatter={value => axisText(value, creditAxis.max)} tick={tickStyle}
            tickLine={false} axisLine={false} width={64} />
          {hasRate ? (
            <YAxis yAxisId='ppm' orientation='right' domain={[0, rateAxis.max]} ticks={rateAxis.ticks}
              allowDataOverflow tickFormatter={value => formatCreditRate(value)} tick={tickStyle}
              tickLine={false} axisLine={false} width={64} />
          ) : null}
          <RechartsTooltip cursor={{ stroke: gridColor }} isAnimationActive={false} filterNull={false}
            content={props => <CreditTooltip {...props} container={container} />} />
          {/* connectNulls 默认 false：没有读数的那天折线断开，不假装当天花了 0。
              配色沿用既有语义：橙色 = 量（积分消耗，与 Token 同一族），
              主色 = 比率（每百万 Token 积分，与缓存命中率同一族）。 */}
          <Line type='monotone' dataKey='credit' stroke={tokenColor} strokeWidth={2} connectNulls={false}
            dot={{ r: 2.5, fill: tokenColor, strokeWidth: 0 }} activeDot={{ r: 4 }} isAnimationActive={false} />
          {hasRate ? (
            <Line yAxisId='ppm' type='monotone' dataKey='ppm' stroke={rateColor} strokeWidth={2} connectNulls={false}
              dot={{ r: 2.5, fill: rateColor, strokeWidth: 0 }} activeDot={{ r: 4 }} isAnimationActive={false} />
          ) : null}
        </LineChart>
      </ResponsiveContainer>
    </div>
  )
}

export function DailyTrendChart({ series, colors, metric = 'tokens', unknownLabel = t('未知模型') }: {
  series: StatsDay[]; colors: Map<string, string>; metric?: 'tokens' | 'requests'; unknownLabel?: string
}) {
  const container = React.useRef<HTMLDivElement>(null)
  const rows = React.useMemo(() => dailyTrendData(series, colors, unknownLabel), [series, colors, unknownLabel])
  const legend = React.useMemo(() => dailyModelLegend(rows, colors), [rows, colors])
  const maxSegments = rows.reduce((max, row) => Math.max(max, row.segments.length), 0)
  const tokenAxis = chartAxis(rows.reduce((peak, row) => Math.max(peak, row.total), 0))
  if (!rows.length) return <Empty>{t('暂无趋势数据')}</Empty>
  if (!rows.some(row => row.total > 0)) {
    return <div className='report-chart-empty'><Empty>{t('所选范围内暂无请求')}</Empty></div>
  }
  return (
    <>
      <div className='report-chart-area' ref={container}>
        <ResponsiveContainer width='100%' height='100%' minWidth={0}>
          <ComposedChart data={rows} margin={{ top: 28, right: 8, left: 0, bottom: 0 }}
            aria-label={metric === 'requests' ? t('按天请求趋势') : t('按天 Token 趋势')}>
            <CartesianGrid strokeDasharray='3 3' stroke={gridColor} vertical={false} />
            <XAxis dataKey='date' tickFormatter={dateTick} tick={tickStyle} tickLine={false}
              axisLine={{ stroke: gridColor }} minTickGap={20} interval='preserveStartEnd' />
            <YAxis domain={[0, tokenAxis.max]} ticks={tokenAxis.ticks} allowDataOverflow
              tickFormatter={value => metric === 'requests' ? formatInt(value) : axisText(value, tokenAxis.max)} tick={tickStyle}
              tickLine={false} axisLine={false} width={64} />
            <RechartsTooltip cursor={{ fill: 'color-mix(in srgb, var(--primary) 8%, transparent)' }}
              isAnimationActive={false} filterNull={false}
              content={props => <DailyTooltip {...props} container={container} metric={metric} />} />
            {Array.from({ length: maxSegments }, (_, index) => (
              <Bar key={`slot${index}`} dataKey={`slot${index}`} stackId='tokens' maxBarSize={36}
                radius={[2, 2, 0, 0]} isAnimationActive={false}>
                {rows.map((row, dayIndex) => (
                  <Cell key={`${dayIndex}-${row.date}`} fill={row.segments[index]?.fill || 'transparent'} />
                ))}
              </Bar>
            ))}
            <DailyPointLabels rows={rows} formatValue={metric === 'requests' ? formatInt : formatTokens} />
          </ComposedChart>
        </ResponsiveContainer>
      </div>
      <div className='report-model-legend'>
        {legend.map(row => (
          <span className='report-model-legend-item' title={row.name} key={row.name}>
            <span className='report-model-legend-dot' style={{ background: row.fill }} />
            <span className='report-model-legend-name'>{row.name}</span>
          </span>
        ))}
      </div>
    </>
  )
}
