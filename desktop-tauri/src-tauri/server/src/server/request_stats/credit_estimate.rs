//! 按**账号余额差**估算没有上游积分读数的请求（schema v26）。
//!
//! ── 为什么需要 ────────────────────────────────────────────────
//! 多数渠道不在响应里报积分（`usage.credit` / `points_*` 都不给），v23 起
//! 明细里的 `credit` 对它们恒为 `NULL`，报表只能显示「未记录」。但余额查询
//! 循环一直在跑（`core::usage_records`），每次成功读数都是一条事实：
//! **两次读数之间的余额差，就是这一段时间里真实发生的消耗**。
//! 把它减掉区间内已经直报的部分，剩下的按 token 权重分摊给没有读数的
//! 成功请求，就能把「未记录」补成「估算」。
//!
//! ── 三条硬口径 ──────────────────────────────────────────────
//!   1. **上游直报优先**：只给 `credit IS NULL` 的行写估算，绝不覆盖直报值；
//!   2. **估算不冒充直报**：写入时 `credit_source = 'balance'`，报表的覆盖率
//!      仍只数 `upstream`（否则估算铺开后覆盖率会假性变成 100%）；
//!   3. **拿不准就不动**：余额上升（充值）、单位不同、区间内已有读数把差额
//!      吃光、明细条数少于聚合请求数 —— 这几种情况一律跳过，不写一个偏小的数；
//!      充值区间还会把该段里上一轮写下的估算一并清掉（见「已知损失」）。
//!
//! ── 已知损失（如实记录，不试图用魔法补齐）─────────────────────
//!   - 充值所在的区间 `delta <= 0` 不产生估算，只清掉该段里残留的旧估算；
//!   - 最后一次快照之后的请求要等下一次余额变化才会被估算；
//!   - 余额查询间隔（默认 60 秒 / 账号）大于请求间隔时只能近似；
//!   - 并发请求按 token 权重分摊，不是精确归属；
//!   - 跨账号重试时 token 记在最终账号上，可能带来偏差；
//!   - 左端点早于**明细保留期下界**的区间不再估算：那些请求已有部分被删，
//!     余额差却仍含着它们的消耗，只摊给剩下的行会明显偏大（见 `settle` 的
//!     `requests_from`）；
//!   - 按**条数**裁剪（而非按天）删掉的行不受这层保护：它没有可用的时间边界，
//!     命中时那一小段仍会偏大。默认保留条数远高于日常用量，顺序是时间先生效；
//!   - 发起于裁剪线之前、跨越数天才完成的长请求同样会被裁掉，它所属的区间
//!     因此偏小（实际耗时以天计，正常请求不会命中这一档）；
//!   - v26 之前的历史请求无法回补（没有历史余额快照）。

use std::collections::BTreeSet;

use chrono::{Duration as ChronoDuration, NaiveDate};
use rusqlite::{params, Connection, OptionalExtension};

use super::aggregate::rebuild_day;
use super::clock::{date_key, day_of, local_midnight_ms};
use super::record::CREDIT_SOURCE_BALANCE;
use crate::server::core::usage_records::is_credit_unit;

/// 单轮 settle 的结果（调用方只在有变化时打日志）。
#[derive(Debug, Default)]
pub(super) struct SettleOutcome {
    /// 本轮写入估算值的请求条数
    pub estimated_rows: i64,
    /// 本轮因差额已被直报吃满而清除的旧估算条数
    pub cleared_rows: i64,
    /// 已重算聚合的日期
    pub rebuilt_days: Vec<String>,
    /// 因明细不完整而跳过的日期
    pub skipped_days: Vec<String>,
    /// 因为单位不是积分而整段跳过的单位（美元 / `%` / `quota` 都不能拿来
    /// 相减推断消耗）。只用于**锁外**记一条 verbose —— 「估算一直不出数」
    /// 必须能查到原因，而不是只能看到「未记录」。
    pub foreign_units: BTreeSet<String>,
}

#[derive(Debug)]
/// 一条余额快照（只取估算需要的五列）。
struct Snapshot {
    remaining: f64,
    unit: String,
    at: i64,
    /// 这条读数属于哪一家：账号改绑 provider 时，跨过边界的两条快照
    /// 余额没有可比性（新家的余额不是从旧家的余额扣出来的）
    provider: String,
}

/// 区间内一条成功的明细（估算候选）。
struct Candidate {
    row_id: i64,
    /// 发起时刻：**归档日**按它算（`rebuild_day` / `fold_into_daily` 都按 `ts`
    /// 分天，用完成时刻会出现「重算的那天不包含这条」的错位）
    ts: i64,
    /// 完成时刻（`ts + duration_ms`）：归属区间的判据
    completed_at: i64,
    weight: i64,
    credit: Option<f64>,
    /// 已有读数是不是估算值：估算行可以重平衡，直报行不参与分摊
    estimated: bool,
    /// 模型测试发起的行：**参与分摊**（测试流量同样消耗余额），但不折进
    /// 日报聚合（`fold_into_daily` 一律跳过 `is_test`），所以它的日期不进
    /// `affected_days` —— 重算那天的聚合也看不到这一行，没必要触发。
    is_test: bool,
}

/// 单账号单轮最多处理的相邻区间数（纯内存计算，只防病态数据撑爆）。
const MAX_INTERVALS_PER_ACCOUNT: usize = 20_000;

/// 估算的主体：读快照 → 逐区间分摊 → 写明细 → 重算受影响日期。
///
/// `requests_from` 是**明细保留期的毫秒下界**（调用方从 `RetentionBounds` 取，
/// 与 `delete_expired_requests` 同一个值）。早于它的明细已被裁掉，而余额差
/// 仍包含那部分消耗 —— 这种区间一旦分摊，每条剩下的行都会背上不属于自己的
/// 那一截，且「合计 = 余额差」照样成立，从报表上完全看不出错。所以只估算
/// 左端点落在保留窗口内的区间。
///
/// 调用方负责串行化（`RequestStats` 的互斥锁）与日志（锁外）。
pub(super) fn settle(
    conn: &mut Connection,
    requests_from: i64,
) -> rusqlite::Result<SettleOutcome> {
    // `transaction()` 而不是 `unchecked_transaction()`：`db` 模块的约定是
    // 「需要事务的批量写用 `with_mut` + `transaction()`」，把「事务期间不得
    // 再借用连接」这条规则留在类型系统里。这里已经拿到 `&mut Connection`，
    // 没有理由退回运行期检查。
    //
    // 例外是 `aggregate.rs` / `legacy.rs` 里的几处：它们的入参签名是
    // `&Connection`（迁移框架 / 回填调用点给的形状），改不到 `&mut` 才用
    // `unchecked_transaction`。本函数不在此列。
    let tx = conn.transaction()?;
    let mut outcome = SettleOutcome::default();
    let mut affected_days: BTreeSet<NaiveDate> = BTreeSet::new();

    for (account_id, snapshots) in load_snapshots(&tx)? {
        // 这个账号用过的非空单位（空串不算：那是「没给」，settle 里也没有可比性）
        let units: BTreeSet<&str> = snapshots
            .iter()
            .map(|snapshot| snapshot.unit.as_str())
            .filter(|unit| !unit.is_empty())
            .collect();
        // **从最新往回**取最近 N 个区间：快照按时间升序，正着取会永远停在
        // 最早的 N 个，之后的余额变化一辈子轮不到估算。
        //
        // 之所以不需要游标：每次 settle 都从最新区间往回看，尚未处理过的旧区间
        // 在它当初还是「最新」的那几轮里已经算过了。只有进程停了很久、期间积累
        // 超过 N 个区间的快照时才会丢掉最老的一段 —— 与「v26 之前的历史本来
        // 就无法回补」同一个已知损失。
        let intervals: Vec<(&Snapshot, &Snapshot)> = snapshots
            .windows(2)
            .rev()
            // 跨单位 / 金钱 / 百分比的差值没有可比性；`unit` 为空同样跳过。
            // 跨 provider 的差值同样没有可比性：账号切 provider 时正好会留下
            // 相邻两条不同 provider 的快照，不能拿来相减。
            //
            // **余额上升（充值）也留在这里**：差额 <= 0 不代表没有区间，只代表
            // 这段不是消耗。把它在过滤阶段丢掉，上一轮写进这段的估算就再也没人
            // 清 —— 那一行会永久留着，让「明细合计 = 余额差」在这段失效。所以
            // 判据只留「能不能相减」，`delta` 的正负交给下面的分摊 / 清除分支。
            .filter(|pair| {
                pair[0].unit == pair[1].unit
                    && pair[0].provider == pair[1].provider
                    && is_credit_unit(&pair[0].unit)
                    && pair[1].at > pair[0].at
                    // 左端点早于明细保留期下界 → 这段的明细已被裁过，不能分摊
                    && pair[0].at >= requests_from
            })
            // 先滤掉无效区间再取配额：否则无效区间会占满 `take` 的名额，
            // 最新的有效区间反而轮不到
            .take(MAX_INTERVALS_PER_ACCOUNT)
            .map(|pair| (&pair[0], &pair[1]))
            .collect();
        if intervals.is_empty() {
            // 一段都算不上：把非积分单位带出去，让「为什么没有估算值」可查。
            // 只有整段都算不出来时才记（有积分区间时不打扰日志）。
            for unit in &units {
                if !is_credit_unit(unit) {
                    outcome.foreign_units.insert((*unit).to_string());
                }
            }
            continue;
        }
        // **每账号只查一次明细**：旧写法对每个区间各发一条 SQL，30 秒一轮、
        // 几个月后每天有上千个区间，光查询就把写锁占满了。
        let from = intervals.iter().map(|(previous, _)| previous.at).min().unwrap_or(0);
        let to = intervals.iter().map(|(_, next)| next.at).max().unwrap_or(0);
        let candidates = load_candidates(&tx, &account_id, from, to)?;
        if candidates.is_empty() {
            continue;
        }
        for (previous, next) in intervals {
            // candidates 按完成时刻升序：区间 (previous.at, next.at] 直接切一段
            let start = candidates.partition_point(|item| item.completed_at <= previous.at);
            let end = candidates.partition_point(|item| item.completed_at <= next.at);
            let window = &candidates[start..end];
            if window.is_empty() {
                continue;
            }
            // 充值 / 退款时 `delta <= 0`：这段不产生消耗，下面的清除分支会把
            // 上一轮的估算清掉（`allocatable` 一定 < 0）。
            let delta = previous.remaining - next.remaining;
            // 上游直报先扣：直报是多少就是多少，**不参与分摊**
            let direct: f64 = window
                .iter()
                .filter(|item| !item.estimated)
                .filter_map(|item| item.credit)
                .sum();
            let allocatable = delta - direct;
            // 可分摊的行：没读数，或上一轮写下的估算（估算行允许重平衡 ——
            // 晚完成的请求在上一轮还是 status=0，那时分不到；不重平衡的话
            // 它会永久显示「—」，而先完成的几条一直背着偏大的值）。
            // 直报行不参与分摊。
            let targets: Vec<&Candidate> = window
                .iter()
                .filter(|item| !(item.credit.is_some() && !item.estimated))
                .collect();

            if allocatable <= 1e-9 {
                // 差额被直报吃满，或这段是充值（`delta <= 0`）：旧估算必须清掉，
                // 否则这一段会显示成「花了比余额差还多」，而且再也没人回来清它
                for item in &targets {
                    if !item.estimated || item.credit.is_none() {
                        continue;
                    }
                    let cleared = tx.execute(
                        "UPDATE requests SET credit = NULL, credit_source = NULL
                         WHERE row_id = ?1 AND credit_source = ?2",
                        params![item.row_id, CREDIT_SOURCE_BALANCE],
                    )?;
                    if cleared > 0 {
                        outcome.cleared_rows += 1;
                        if !item.is_test {
                            affected_days.insert(day_of(item.ts));
                        }
                    }
                }
                continue;
            }
            let weights: Vec<i64> = targets.iter().map(|item| item.weight).collect();
            let shares = allocate(allocatable, &weights);
            for (item, share) in targets.iter().zip(shares) {
                if share <= 0.0 {
                    // 这一行走不到分摊（权重不足 / 浮点残差为 0）：上一轮写下的
                    // 估算已经不代表这段余额差，必须清掉，否则它会一直留在明细
                    // 与聚合里，把「合计 = 余额差」这条自证关系破坏掉
                    if item.estimated && item.credit.is_some() {
                        let cleared = tx.execute(
                            "UPDATE requests SET credit = NULL, credit_source = NULL
                             WHERE row_id = ?1 AND credit_source = ?2",
                            params![item.row_id, CREDIT_SOURCE_BALANCE],
                        )?;
                        if cleared > 0 {
                            outcome.cleared_rows += 1;
                            if !item.is_test {
                                affected_days.insert(day_of(item.ts));
                            }
                        }
                    }
                    continue;
                }
                // 重平衡时同样的输入要得到同样的值：差得太小就不写，避免每轮
                // 30 秒都把整段行重写一遍
                if item.estimated && item.credit.is_some_and(|value| (value - share).abs() <= 1e-9) {
                    continue;
                }
                let written = tx.execute(
                    "UPDATE requests SET credit = ?2, credit_source = ?3
                     WHERE row_id = ?1 AND (credit IS NULL OR credit_source = ?3)",
                    params![item.row_id, share, CREDIT_SOURCE_BALANCE],
                )?;
                // 0 行 = 这条在 load 与 UPDATE 之间被上游直报抢先写入了：计数与
                // 受影响日都该跟着实际写入走，否则日志会报一个没发生的估算
                if written > 0 {
                    outcome.estimated_rows += 1;
                    // 测试行不进聚合，重算那一天也读不到它 —— 不把它记成受影响日
                    if !item.is_test {
                        affected_days.insert(day_of(item.ts));
                    }
                }
            }
        }
    }

    // 写明细后必须重算聚合行，否则报表（只读 `request_daily`）看不到估算值。
    // 安全闸与 `backfill` / `backfill_credit_days` 同一条：明细条数 ≥ 聚合请求数
    // 才说明明细足以代表当天，拿残缺明细重算会让当天的数字凭空缩水。
    for day in affected_days {
        let key = date_key(day);
        let start = local_midnight_ms(day);
        let end = local_midnight_ms(day + ChronoDuration::days(1)).saturating_sub(1);
        let detail_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM requests
             WHERE ts >= ?1 AND ts <= ?2 AND is_test = 0 AND status <> 0",
            params![start, end],
            |row| row.get(0),
        )?;
        let aggregate_requests: i64 = tx
            .query_row(
                "SELECT requests FROM request_daily WHERE date = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if detail_count >= aggregate_requests {
            rebuild_day(&tx, day)?;
            outcome.rebuilt_days.push(key);
        } else {
            outcome.skipped_days.push(key);
        }
    }

    tx.commit()?;
    Ok(outcome)
}

/// 读出所有账号的余额快照（按账号分组、按时间升序）。
fn load_snapshots(
    conn: &Connection,
) -> rusqlite::Result<Vec<(String, Vec<Snapshot>)>> {
    let mut stmt = conn.prepare(
        "SELECT account_id, remaining, unit, at, provider FROM account_balance_snapshots
         ORDER BY account_id ASC, at ASC, id ASC",
    )?;
    let mut rows = stmt.query([])?;
    let mut out: Vec<(String, Vec<Snapshot>)> = Vec::new();
    while let Some(row) = rows.next()? {
        let account_id: String = row.get(0)?;
        let snapshot = Snapshot {
            remaining: row.get(1)?,
            unit: row.get(2)?,
            at: row.get(3)?,
            provider: row.get(4)?,
        };
        match out.last_mut() {
            Some((last_id, list)) if *last_id == account_id => list.push(snapshot),
            _ => out.push((account_id, vec![snapshot])),
        }
    }
    Ok(out)
}

/// `(from, to]` 内该账号的成功明细（按完成时刻归属）。
///
/// 成功判据与 `RequestEntry::is_success` 同一组：`status` 2xx、无 `error`；
/// 再排除进行中行。完成时刻用 `ts + duration_ms`（spec 口径）。
///
/// **不排除 `is_test`**：余额差是账号级的真实扣费，测试流量消耗的那部分
/// 也在里头；只把它按「真实行」分摊会让真实请求背上不属于自己的消耗。
/// 测试行可以写估算（请求列表里看得到），但不进日报聚合。
///
/// 一次覆盖整段快照范围（而不是逐个区间查）—— 调用方在内存里按完成时刻
/// 分桶，运行期只发这一条 SQL。
fn load_candidates(
    conn: &Connection,
    account_id: &str,
    from: i64,
    to: i64,
) -> rusqlite::Result<Vec<Candidate>> {
    // `ts` 的上下界各放宽一天：让 `idx_requests_account_ts` 先缩范围，而真正的
    // 归属仍由 `ts + duration_ms` 决定（普通请求远短于一天）。上界放宽是必须的
    // —— 发起时刻可以早于 `from`（跨区间的长请求），也可以晚于 `to` 之前一点点
    // 才完成。
    let mut stmt = conn.prepare(
        "SELECT row_id, ts, duration_ms, total_tokens, prompt_tokens, completion_tokens, credit, credit_source, is_test
         FROM requests
         WHERE account_id = ?1
           AND ts >= ?2 - 86400000
           AND ts <= ?3
           AND ts + duration_ms > ?2
           AND ts + duration_ms <= ?3
           AND status >= 200 AND status < 300
           AND error IS NULL
         ORDER BY ts + duration_ms ASC, row_id ASC",
    )?;
    let mut rows = stmt.query(params![account_id, from, to])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(1)?;
        let duration: i64 = row.get(2)?;
        let total: i64 = row.get(3)?;
        let prompt: i64 = row.get(4)?;
        let completion: i64 = row.get(5)?;
        let source: Option<String> = row.get(7)?;
        out.push(Candidate {
            row_id: row.get(0)?,
            ts,
            completed_at: ts.saturating_add(duration.max(0)),
            // 权重口径见模块头：总 token 优先，缺失时用输入 + 输出。全 0 的行
            // **不记 1**：那会把真实余额差摊到一条没有用量的请求上（它的
            // credit 从「未知」变成「估出来一个正数」）。权重 0 在 `allocate`
            // 里得到 0，随后走清除分支 —— 没有用量就分不到积分。
            weight: total.max(prompt.saturating_add(completion)).max(0),
            credit: row.get(6)?,
            estimated: source.as_deref() == Some(CREDIT_SOURCE_BALANCE),
            is_test: row.get(8)?,
        });
    }
    Ok(out)
}

/// 按权重把 `delta` 分给各行；权重为 0 的行（已有读数、或用量为 0）不参与。
///
/// 保留 6 位小数（与明细的 REAL 精度匹配），**最后一条吃浮点残差** ——
/// 逐条四舍五入会让分摊之和对不上余额差，而「合计 = 余额差」正是估算
/// 唯一的自证方式。没有可分摊的行时返回全 0。
pub(super) fn allocate(delta: f64, weights: &[i64]) -> Vec<f64> {
    let mut out = vec![0.0; weights.len()];
    if !delta.is_finite() || delta <= 0.0 {
        return out;
    }
    let positive: Vec<usize> = weights
        .iter()
        .enumerate()
        .filter(|(_, weight)| **weight > 0)
        .map(|(index, _)| index)
        .collect();
    if positive.is_empty() {
        return out;
    }
    let total: i128 = positive.iter().map(|index| weights[*index] as i128).sum();
    let mut assigned = 0.0;
    for (position, index) in positive.iter().enumerate() {
        let share = if position + 1 == positive.len() {
            // 最后一条吃残差：保证 Σshare == delta。残差本身也要过一遍
            // 同一个刻度 —— 前面几条各自四舍五入后，`delta - assigned` 会带上
            // 1 ulp 级的尾巴（`0.9 - 0.3` 在 f64 里是 0.6000000000000001），
            // 直接写库就成了「合计对得上、单条多出一串小数」的脏数。
            round_credit(delta - assigned)
        } else {
            round_credit(delta * weights[*index] as f64 / total as f64)
        };
        // 逐条四舍五入后，最后一条的残差理论上可能落到 -1e-6 量级；
        // 负的「消耗」没有意义，夹到 0。夹完再累加，`assigned` 才等于真正
        // 写出去的那几条之和（否则合计会比余额差多出被夹掉的那一截）。
        let share = share.max(0.0);
        out[*index] = share;
        assigned += share;
    }
    out
}

/// 明细的 `REAL` 精度按 6 位小数使用（见 [`allocate`] 的说明）。
///
/// 分摊的每一条都必须过同一个刻度：否则相邻两条会一边是 `0.3`、一边是
/// `0.30000000000000004`，报表按 6 位小数排版时看着正常，导出的原始值却
/// 对不上，排查时白绕一圈。
fn round_credit(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_request_takes_the_whole_delta() {
        assert_eq!(allocate(0.5, &[42]), vec![0.5]);
    }

    #[test]
    fn weights_split_proportionally() {
        // 权重 100 / 200，差额 0.9 → 0.3 / 0.6
        assert_eq!(allocate(0.9, &[100, 200]), vec![0.3, 0.6]);
    }

    #[test]
    fn reported_rows_do_not_participate() {
        // 中间那条已经直报（权重给 0）→ 只有两侧分摊
        assert_eq!(allocate(0.9, &[100, 0, 200]), vec![0.3, 0.0, 0.6]);
    }

    #[test]
    fn non_positive_delta_allocates_nothing() {
        assert_eq!(allocate(0.0, &[1, 2]), vec![0.0, 0.0]);
        assert_eq!(allocate(-1.0, &[1, 2]), vec![0.0, 0.0]);
    }

    #[test]
    fn rounding_residual_goes_to_the_last_row() {
        let shares = allocate(1.0, &[1, 1, 1]);
        let sum: f64 = shares.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "sum = {sum}");
        // 用容差而不是 `assert_eq!`：残差那一条的写法是 `1.0 - 0.333333 -
        // 0.333333`，与字面量 `0.333334` 在 f64 上未必逐位相同 —— 那不是
        // 这里的语义（要的是「残差落在最后一条」），逐位比较只会让用例
        // 对无关的浮点细节敏感。
        assert!((shares[0] - 0.333333).abs() < 1e-9);
        assert!((shares[2] - 0.333334).abs() < 1e-9, "shares = {shares:?}");
    }

    // ─── settle 的端到端用例 ────────────────────────────────────
    //
    // 用真实 schema（`schema::migrate` 就是启动时跑的那一条）在内存库里跑完整
    // 链路：快照 → 区间切分 → 分摊 → 写明细 → 重算日报。不 mock SQL ——
    // 估算的历史 bug（列序、单位过滤、覆盖度口径）正是出现在 SQL 与口径的
    // 接缝上，mock 掉就看不出「合计 = 余额差」这条自证关系还成不成立。

    use crate::server::db::schema;
    use crate::server::request_stats::clock::{date_key, local_midnight_ms, today};
    use crate::server::request_stats::record::{CREDIT_SOURCE_BALANCE, CREDIT_SOURCE_UPSTREAM};
    use rusqlite::Connection;

    /// 演示账号：两条余额快照 + 若干成功请求，每条给 `(token 权重, 上游直报)`。
    ///
    /// 返回这批请求归属的**本地自然日键**（断言用它，避免跨零点跑时明细与
    /// 聚合被判成两天）。
    fn seed(
        conn: &Connection,
        unit: &str,
        from: f64,
        to: f64,
        rows: &[(i64, Option<f64>)],
    ) -> String {
        let day = today();
        let day_start = local_midnight_ms(day);
        // 快照落在当天 1 点 / 2 点，请求完成于 1:10 —— 同一本地自然日、
        // 同一区间，日期归属不会随测试机的时区漂移
        let first = day_start + 3_600_000;
        let second = day_start + 7_200_000;
        conn.execute(
            "INSERT INTO account_balance_snapshots (account_id, remaining, unit, at, provider)
             VALUES ('a1', ?1, ?2, ?3, 'workbuddy'), ('a1', ?4, ?2, ?5, 'workbuddy')",
            params![from, unit, first, to, second],
        )
        .unwrap();
        for (weight, credit) in rows {
            let source = if credit.is_some() { CREDIT_SOURCE_UPSTREAM } else { "" };
            conn.execute(
                "INSERT INTO requests
                   (ts, model, account_id, status, duration_ms, total_tokens, credit, credit_source, provider)
                 VALUES (?1, 'm', 'a1', 200, 0, ?2, ?3, ?4, 'workbuddy')",
                params![first + 600_000, weight, credit, source],
            )
            .unwrap();
        }
        date_key(day)
    }

    fn daily(conn: &Connection, key: &str) -> (Option<f64>, i64, Option<f64>, i64) {
        conn.query_row(
            "SELECT credit, credit_rows, credit_estimated, credit_estimated_rows
             FROM request_daily WHERE date = ?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    /// 测试里默认「没有裁剪」：保留期下界早于所有构造出来的数据。
    fn settle_all(conn: &mut Connection) -> rusqlite::Result<SettleOutcome> {
        settle(conn, i64::MIN)
    }

    #[test]
    fn balance_gap_fills_requests_without_a_reading() {
        let mut conn = db();
        // 直报 4.5，余额差 10 → 可分摊 5.5；两条无读数的权重 100 / 300
        // 分成 1.375 / 4.125
        let key = seed(
            &conn,
            "积分",
            100.0,
            90.0,
            &[(100, Some(4.5)), (100, None), (300, None)],
        );
        let outcome = settle_all(&mut conn).unwrap();
        assert_eq!(outcome.estimated_rows, 2);
        assert!(outcome.foreign_units.is_empty());
        assert_eq!(outcome.rebuilt_days.len(), 1);

        let estimated: Vec<(f64, String)> = conn
            .prepare("SELECT credit, credit_source FROM requests ORDER BY row_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(estimated[0], (4.5, CREDIT_SOURCE_UPSTREAM.to_string()));
        assert_eq!(estimated[1], (1.375, CREDIT_SOURCE_BALANCE.to_string()));
        assert_eq!(estimated[2], (4.125, CREDIT_SOURCE_BALANCE.to_string()));

        // 日报：合计 = 余额差；覆盖率的分母只数直报那一条
        let (credit, direct_rows, estimated_sum, estimated_rows) = daily(&conn, &key);
        let credit = credit.expect("当天有直报，合计不应为 None");
        assert!((credit - 10.0).abs() < 1e-9, "credit = {credit}");
        assert_eq!(direct_rows, 1);
        assert!((estimated_sum.unwrap() - 5.5).abs() < 1e-9);
        assert_eq!(estimated_rows, 2);
    }

    #[test]
    fn foreign_unit_is_skipped_and_reported() {
        let mut conn = db();
        seed(&conn, "USD", 10.0, 9.0, &[(100, None)]);
        let outcome = settle_all(&mut conn).unwrap();
        assert_eq!(outcome.estimated_rows, 0);
        assert!(outcome.foreign_units.contains("USD"));
        let credit: Option<f64> = conn
            .query_row("SELECT credit FROM requests", [], |row| row.get(0))
            .unwrap();
        assert!(credit.is_none(), "美元读数不能写成积分");
    }

    #[test]
    fn recharge_interval_is_not_allocated() {
        let mut conn = db();
        // 余额上升（充值）：差值不是消耗，整段跳过
        seed(&conn, "积分", 90.0, 100.0, &[(100, None)]);
        let outcome = settle_all(&mut conn).unwrap();
        assert_eq!(outcome.estimated_rows, 0);
        let credit: Option<f64> = conn
            .query_row("SELECT credit FROM requests", [], |row| row.get(0))
            .unwrap();
        assert!(credit.is_none());
    }

    /// 充值区间不只「不产生估算」，还要把上一轮写下的估算清掉。
    ///
    /// 这是「filter 去掉 `delta > 0`」那条改动的回归点：如果充值区间在过滤
    /// 阶段就被丢掉，这一行估算会永久留在明细与聚合里，让「明细合计 = 余额差」
    /// 在这段失效，而且再没有一轮会回来清它。
    #[test]
    fn recharge_interval_clears_old_estimate() {
        let mut conn = db();
        // 余额从 90 涨到 100（充值 10），区间内有一条上一轮写下的估算行
        let key = seed(&conn, "积分", 90.0, 100.0, &[(100, None)]);
        conn.execute(
            "INSERT INTO requests
               (ts, model, account_id, status, duration_ms, total_tokens, credit, credit_source, provider)
             VALUES ((SELECT ts FROM requests LIMIT 1), 'm', 'a1', 200, 0, 50, 3.0, 'balance', 'workbuddy')",
            [],
        )
        .unwrap();
        let outcome = settle_all(&mut conn).unwrap();
        assert_eq!(outcome.estimated_rows, 0);
        assert_eq!(outcome.cleared_rows, 1, "充值区间的旧估算必须被清除");
        let leftover: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM requests WHERE credit_source = 'balance'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0);
        // 聚合也要跟着回落：这一天不能再报「有 N 条估算」
        let (credit, _, estimated_sum, estimated_rows) = daily(&conn, &key);
        assert!(credit.is_none(), "估算清空后，当天合计应回落为 None");
        assert!(estimated_sum.is_none(), "估算合计应回落为 None");
        assert_eq!(estimated_rows, 0);
    }

    #[test]
    fn direct_report_consuming_the_gap_clears_old_estimate() {
        let mut conn = db();
        seed(&conn, "积分", 100.0, 90.0, &[(100, Some(10.0))]);
        // 先造一条上一轮的估算行（模拟「当时没有直报，后来上游补上了」）
        conn.execute(
            "INSERT INTO requests
               (ts, model, account_id, status, duration_ms, total_tokens, credit, credit_source, provider)
             VALUES ((SELECT ts FROM requests LIMIT 1), 'm', 'a1', 200, 0, 50, 3.0, 'balance', 'workbuddy')",
            [],
        )
        .unwrap();
        let outcome = settle_all(&mut conn).unwrap();
        // 直报已吃满差额：估算行必须被清回 NULL，不能留着「比余额差还多」
        assert_eq!(outcome.estimated_rows, 0);
        assert_eq!(outcome.cleared_rows, 1);
        let leftover: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM requests WHERE credit_source = 'balance'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0);
    }

    /// 区间左端点早于**明细保留期下界**时，整段跳过。
    ///
    /// 这是保留期裁剪的回归点：裁剪之后余额差里仍含着已删请求的消耗，而候选
    /// 只剩后面的几条。照旧分摊会让每一条都估多（「合计 = 余额差」自证关系
    /// 仍成立，所以从报表上看不出错），只能整段不算。
    #[test]
    fn interval_before_retention_floor_is_skipped() {
        let mut conn = db();
        seed(&conn, "积分", 100.0, 90.0, &[(100, None)]);
        // 保留期下界正好落在左端点之后：模拟「这条快照之前的明细已被裁掉」
        let cutoff: i64 = conn
            .query_row("SELECT MIN(at) FROM account_balance_snapshots", [], |row| row.get(0))
            .unwrap();
        let outcome = settle(&mut conn, cutoff + 1).unwrap();
        assert_eq!(outcome.estimated_rows, 0, "跨裁剪线的区间不该分摊");
        let credit: Option<f64> = conn
            .query_row("SELECT credit FROM requests LIMIT 1", [], |row| row.get(0))
            .unwrap();
        assert!(credit.is_none());
        // 一条都没写，也就没有任何日期需要重算
        assert!(outcome.rebuilt_days.is_empty());
        let daily_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM request_daily", [], |row| row.get(0))
            .unwrap();
        assert_eq!(daily_rows, 0);
    }
}
