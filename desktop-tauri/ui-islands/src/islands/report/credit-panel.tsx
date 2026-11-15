import { Card, CardContent, CardFooter, CardHeader, MetricCard, MetricGrid } from '@ui'
import { creditCells, formatCredit, formatCreditRate, type StatsSummary } from '../report-charts'
import { t } from '../../i18n'
import { CreditTrendChart } from './report-recharts'

/**
 * 积分口径的说明。三条都是**会被误读**的地方，逐条写清：
 *  1. 积分不是网关自己记的账，而是上游在响应的 usage 里报上来的读数 ——
 *     所以「没读数」是常态，不是丢数据；
 *  2. 覆盖率的分母是成功请求（失败请求不产生用量，`normalize` 已把它们的
 *     积分与 token 清空）；
 *  3. 个别上游把 `cost`（美元）报在同一个字段里，与积分混在一列。两家以上
 *     都在报时，合计没有单位意义，因此不展示合计、改为按提供商分别看。
 */
const TIP_CREDITS = t('积分消耗读的是上游在响应 usage 里上报的数值（积分 / points_* / cost 等别名字段），不是网关自己的估算。并非所有提供商都上报，覆盖率 = 有读数的成功请求 / 成功请求；覆盖率不足 100% 时合计只含已上报的那部分，读数偏低不等于记账出错。失败请求不产生用量，不计入分母。个别提供商的 cost 是美元、与积分共用同一列，多于一家在报读数时合计没有单位意义，会改为按提供商分别列出。')

/**
 * 积分消耗面板：三个读数 + 按天折线（可选提供商分家表）。
 *
 * ── 整块按契约显隐 ────────────────────────────────────────────
 * `summary.credits` 不存在（旧后端 / 接口还没升级）直接返回 null —— 摆一张
 * 全是「—」的空卡片只会让人以为积分功能坏了。
 */
export function CreditPanel({ summary }: { summary: StatsSummary | null }) {
  const credits = summary?.credits
  if (!summary || !credits) return null

  const series = Array.isArray(summary.dailyTrend) ? summary.dailyTrend : []
  // 混单位时合计不可信，改按提供商分家；其它情况不重复展示同一笔钱。
  // 只列**有读数**的提供商（后端已滤过一轮，这里再按原始值判一次：
  // `Number(null)` 是 0，用转换后的值判会造出「0 积分」的假行）。
  const providers = (Array.isArray(credits.byProvider) ? credits.byProvider : []).filter(
    item => item?.credit !== null && item?.credit !== undefined,
  )

  return (
    <Card render={<section />} className='panel' id='report-credits-panel'>
      <CardHeader className='panel-head'>
        <h2>{t('积分消耗')}</h2>
        <span className='tip-q' data-tip={TIP_CREDITS} />
        <span className='panel-sub'>{summary.startDate || '—'} ～ {summary.endDate || '—'}</span>
      </CardHeader>
      <CardContent className='panel-body'>
        <MetricGrid className='field-grid' id='report-credits'>
          {creditCells(credits).map(cell => (
            <MetricCard className='field' key={cell.key}>
              <div className='label'>{cell.label}</div>
              <div className={cell.mono ? 'value mono' : 'value'}>
                {cell.value}
                {cell.sub ? <div className='sub'>{cell.sub}</div> : null}
              </div>
            </MetricCard>
          ))}
        </MetricGrid>
        <div className='chart-wrap' id='report-credit-trend'>
          <CreditTrendChart series={series} />
        </div>
        {credits.mixed && providers.length ? (
          <>
            <div className='report-credit-split-title'>
              {t('按提供商分列：不同提供商的计费单位可能不同，不做合计')}
            </div>
            {/* 这里的表**不做占比**：混单位时各行之和没有意义，一条横向进度条
                与百分比只会把「这家 60%」读成「这家占大头」。所以给的是每家
                自己的绝对读数 + 单价 + 覆盖条数，按提供商逐行对照着看。 */}
            <div className='report-credit-table' id='report-credit-providers'>
              <div className='report-credit-table-head'>
                <span>{t('提供商')}</span>
                <span>{t('积分消耗')}</span>
                <span>{t('每百万 Token 积分')}</span>
                <span>{t('覆盖率')}</span>
              </div>
              {providers.map(item => {
                const rows = Number(item.creditRows) || 0
                const success = Number(item.success) || 0
                const cover = success > 0 ? Math.min(1, rows / success) * 100 : 0
                return (
                  <div className='report-credit-table-row' key={item.id || item.label || 'unknown'}>
                    <span className='name' title={item.label || item.id}>{item.label || item.id || t('未知')}</span>
                    <span className='num'>{formatCredit(item.credit)}</span>
                    <span className='num'>{formatCreditRate(item.ppm)}</span>
                    <span className='pct'>{`${cover.toFixed(0)}%`}</span>
                  </div>
                )
              })}
            </div>
          </>
        ) : null}
      </CardContent>
      {/* 图例：两条线各配一条纵轴 —— 左=当天积分消耗，右=每百万 Token 积分 */}
      <CardFooter className='panel-foot'>
        <span className='chart-legend'>
          <span className='swatch tokens' />{t('积分消耗')}<span className='axis-hint'>{t('左轴')}</span>
        </span>
        <span className='chart-legend'>
          <span className='swatch rate' />{t('每百万 Token 积分')}<span className='axis-hint'>{t('右轴')}</span>
        </span>
        <div className='spacer' />
        <span>{t('没有积分读数的那天折线断开，不按 0 计')}</span>
      </CardFooter>
    </Card>
  )
}