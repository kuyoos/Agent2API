import { Card, CardContent, CardFooter, CardHeader, MetricCard, MetricGrid } from '@ui'
import { creditCells, formatCredit, formatCreditRate, formatInt, type StatsSummary } from '../report-charts'
import { t } from '../../i18n'
import { CreditTrendChart } from './report-recharts'

/**
 * 积分口径的说明。四条都是**会被误读**的地方，逐条写清：
 *  1. 上游直报优先 —— 响应 usage 里有读数就直接用，不改写；
 *  2. 没直报的请求由网关按「账号余额差 + Token 权重」事后估算，标 `≈`；
 *  3. 覆盖率只数上游直报（估算铺满后不能把覆盖度抬到 100%）；
 *  4. 个别上游把 `cost`（美元）报在同一个字段里，与积分混在一列。两家以上
 *     都在报时，合计没有单位意义，因此不展示合计、改为按提供商分别看。
 */
const TIP_CREDITS = t('上游在响应 usage 里直报的积分（积分 / points_* / cost 等别名字段）优先采用。多数渠道不返回单条积分，网关用账号余额的相邻两次读数之差，扣除区间内已直报的部分，再按 Token 权重分摊给没有读数的成功请求 —— 这些值标 ≈，是估算而不是上游原值。覆盖率和「有读数条数」只统计上游直报，估算条数单列。余额查询间隔越稀疏、区间内并发请求越多，估算越粗；充值所在的区间、最后一次余额读数之后的请求，以及没有余额快照的历史请求都不会有估算。失败请求不产生用量，不计入分母。个别提供商的 cost 是美元、与积分共用同一列，多于一家在报读数时合计没有单位意义，会改为按提供商分别列出。')

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
                const estimatedRows = Number(item.creditEstimatedRows) || 0
                const success = Number(item.success) || 0
                const cover = success > 0 ? Math.min(1, rows / success) * 100 : 0
                // `≈` 只加在**真有数**的格子上：`formatCreditRate(null)` 返回 `—`，
                // 无条件拼前缀会显示成 `≈ —` —— 那个破折号的语义是「算不出」，
                // 加个约等号反而像「估出来一个大约未知的数」。credit 同理
                // （虽然上一层的过滤通常已把 null 挡掉，这里不依赖那个不变量）。
                const creditText = formatCredit(item.credit)
                const rateText = formatCreditRate(item.ppm)
                const approxOf = (text: string) => (estimatedRows > 0 && text !== '—' ? '≈ ' : '')
                // 覆盖率只数直报：整行都靠估算时不给 0%（那会被读成「没查」），
                // 明确写「估算」说明这一家的数全是推出来的
                const coverText = rows > 0
                  ? `${cover.toFixed(0)}%`
                  : estimatedRows > 0 ? t('估算') : '—'
                return (
                  <div className='report-credit-table-row' key={item.id || item.label || 'unknown'}>
                    <span className='name' title={item.label || item.id}>{item.label || item.id || t('未知')}</span>
                    <span className='num'>{`${approxOf(creditText)}${creditText}`}</span>
                    <span className='num'>{`${approxOf(rateText)}${rateText}`}</span>
                    <span className='pct' title={estimatedRows > 0
                      ? t('其中 {n} 条按余额差估算', { n: formatInt(estimatedRows) })
                      : undefined}>
                      {coverText}
                    </span>
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
        <span>{t('≈ 为按账号余额差估算的值，覆盖率只统计上游直报')}</span>
      </CardFooter>
    </Card>
  )
}
