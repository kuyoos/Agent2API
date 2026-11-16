//! 报表计算 —— 对内存快照的**纯函数**变换，不碰磁盘、不持锁。
//!
//! 为什么单独一层：报表口径是本切片里最容易出细节错的部分（零除、补零、
//! 连续天数、并列比较、时区边界），集中在一起才能一眼看清「同一天/同一小时」
//! 的口径是否处处一致；也便于后续改动时只碰这一处。
//!
//! ── 数据来源的分工 ──────────────────────────────────────────
//!   - `overview` / `dailyTrend` / `heatmap` / `streak` 走**聚合**：
//!     这些口径要跨年（热力图固定 365 天、`all` 可能一年以上），
//!     而明细只留 30 天，靠明细算会越算越少。
//!   - `cacheRates` 走**SQL 侧聚合**（`sql::sum_cache_windows` 一次算出四档
//!     窗口的合计，本层只做零除与百分比）：四档窗口最长 7 天，而「按窗口求和」
//!     没有精度损失 —— 把窗口内的明细搬回内存再遍历四遍，算出来的是一样的数。
//!   - `cacheTrend24h` 走**明细**：要精确到本地整点，按天的聚合行给不出这种
//!     精度；也只取近 24 小时的窄列（`sql::select_summary_rows`）。

use std::collections::{BTreeMap, HashMap};

use chrono::{Datelike, Duration as ChronoDuration, NaiveDate};
use serde_json::{json, Value};

use super::clock::{date_key, hour_floor, hour_key, ms_to_local, now_ms};
use super::record::{AccountAccum, DailyEntry, ModelAccum, ProviderAccum, RequestEntry};

/// 未知 provider 的展示名（`id` 为空串的那一组：旧版本写出的明细、
/// 以及「一次都没发出去就失败」的请求）。前端也会用同样的文案做降级显示。
pub(super) const UNKNOWN_PROVIDER_LABEL: &str = "未知";

/// 未知账号的展示名（没有账号身份的那一组）。
///
/// 与 provider 那条是**两个独立的常量**而不是共用一个：两者的「未知」
/// 成因不同（前者是没记下承载的家，后者是没走账号列表），文案将来若分叉，
/// 共用会让改一处等于改两处。
pub(super) const UNKNOWN_ACCOUNT_LABEL: &str = "未知账号";

/// 热力图固定返回的天数（与 range 解耦）
pub(super) const HEATMAP_DAYS: i64 = 365;

/// 报表要读的**明细窄列**（`sql::SUMMARY_COLUMNS` 的结构形态）。
///
/// 为什么要有这个独立类型（不复用 `RequestEntry`）：报表对明细的消费只有
/// 「按整点归桶 + 按模型累计」两件事，用到的字段就是这 6 个。带上完整
/// `RequestEntry` 意味着读取侧要按 25 列去取行（`route` / `attempt_details`
/// 都在里面），而这份数据一个字段都用不上 —— 类型窄一分，读取那一侧就没法
/// 「顺手多取几列」（见 `sql::select_summary_rows` 与 `db::schema` 的 v15 段）。
#[derive(Clone, Debug)]
pub(super) struct SummaryRow {
    pub ts: i64,
    pub model: String,
    /// 上游实际发出的模型名（空串 = 一次都没发出去），统计键的回落见
    /// [`model_stat_key`]
    pub upstream_model: String,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub prompt_tokens: i64,
}

/// 四档缓存命中率窗口：**JSON 键名 + 窗口毫秒**。
///
/// 两件事绑在一处是必须的：`cache_rates` 按这个数组的顺序取出 SQL 侧那四组
/// 合计（`sql::sum_cache_windows` 的返回顺序与入参一一对应），键名又直接是
/// 前端读的 `cacheRates.last10m` 等字段 —— 分成两个数组（一个放键名、一个放
/// 毫秒值）迟早会漂成「第三档的键配了第二档的窗口」，且极难从界面上看出来。
pub(super) const CACHE_WINDOWS: [(&str, i64); 4] = [
    ("last10m", 10 * 60_000),
    ("last1h", 60 * 60_000),
    ("last24h", 24 * 60 * 60_000),
    ("last7d", 7 * 24 * 60 * 60_000),
];

/// 逐天序列 / 连续天数的循环上限（兜底手改出来的超长区间）
const MAX_TREND_DAYS: i64 = 4000;

/// 合法的时间区间取值
const RANGE_TODAY: &str = "today";
/// 「昨天」：本地自然日的昨天 0 点至 24 点（与 `today` 共用同一套日界线）
const RANGE_YESTERDAY: &str = "yesterday";
const RANGE_7: &str = "7";
const RANGE_30: &str = "30";
const RANGE_MONTH: &str = "month";
const RANGE_ALL: &str = "all";
/// 自定义区间：起止日由查询串的 `startDate` / `endDate` 给出（路由层已校验），
/// 存储层只负责把这一档与那两个日期对上。
pub(super) const RANGE_CUSTOM: &str = "custom";

/// range 归一：非法值按 `"7"`（取舍见 `usage_summary` 的注释）
pub(super) fn normalize_range(range: &str) -> &'static str {
    match range.trim() {
        RANGE_TODAY => RANGE_TODAY,
        RANGE_YESTERDAY => RANGE_YESTERDAY,
        RANGE_30 => RANGE_30,
        RANGE_MONTH => RANGE_MONTH,
        RANGE_ALL => RANGE_ALL,
        RANGE_CUSTOM => RANGE_CUSTOM,
        _ => RANGE_7,
    }
}

/// 区间边界（闭合的本地日期键 `[startDate, endDate]`）。
///
/// - `today` 就是今天；`yesterday` 是昨天一整天（昨天 0 点 ~ 昨天 24 点，
///   与 `today` 共用同一套本地日界线）
/// - `7` / `30` 从今天往前推（含今天，所以减 days-1）
/// - `month` 为本月 1 号到今天（本地时区）
/// - `all` 取聚合里最早的一天；**完全没有数据**时给「今天往前 29 天」，
///   让前端拿到一个合法区间而不是空串或 null
/// - `custom` 用调用方给出的两个日期（路由层已解析校验 `YYYY-MM-DD`）；
///   `custom` 却没带日期（不该出现，路由层会先拦）时按 `7` 兜底 ——
///   宁可给一个能显示的口径，也不要让整页报表 500
///
/// 归一的两条防御（都只兜手改查询串 / 时钟回拨这类外部输入）：
///   - `start > end` 时交换：反向区间会让前端画出负长度时间轴；
///   - `end` 夹到今天：未来的日期没有任何数据，如实回显一个「比今天晚」的
///     区间只会让图例与读数看起来像坏了（聚合里本来就不会有未来的行）。
pub(super) fn range_bounds(
    range: &str,
    daily: &BTreeMap<String, DailyEntry>,
    today: NaiveDate,
    custom: Option<(NaiveDate, NaiveDate)>,
) -> (String, String) {
    if range == RANGE_CUSTOM {
        if let Some((custom_start, custom_end)) = custom {
            let (custom_start, custom_end) = if custom_start > custom_end {
                (custom_end, custom_start)
            } else {
                (custom_start, custom_end)
            };
            let end = custom_end.min(today);
            let start = custom_start.min(end);
            return (date_key(start), date_key(end));
        }
    }
    // 「昨天」是唯一 end 不是今天的档位：单日区间 = 昨天 0 点到昨天 24 点。
    // 先于 `end = today` 处理，免得把今天的行算进来。
    if range == RANGE_YESTERDAY {
        let yesterday = today - ChronoDuration::days(1);
        return (date_key(yesterday), date_key(yesterday));
    }
    let end = today;
    let start = match range {
        RANGE_TODAY => today,
        RANGE_30 => today - ChronoDuration::days(29),
        RANGE_MONTH => today.with_day(1).unwrap_or(today),
        RANGE_ALL => daily
            .keys()
            .next()
            .and_then(|key| NaiveDate::parse_from_str(key, "%Y-%m-%d").ok())
            .unwrap_or_else(|| today - ChronoDuration::days(29)),
        // RANGE_7 及任何落到这里的值
        _ => today - ChronoDuration::days(6),
    };
    // 极端情况：聚合里有「未来」的行（系统时钟被往前调过），start 不能晚于 end，
    // 否则区间是反向的，前端画图会出现负长度时间轴
    let start = if start > end { end } else { start };
    (date_key(start), date_key(end))
}

/// 区间内的总量与按模型 / 按 provider / 按账号累计（overview 的原料）
pub(super) struct RangeTotals {
    pub requests: i64,
    pub successful: i64,
    pub tokens: i64,
    pub active_days: i64,
    /// 区间内的积分消耗之和（`None` = 这段时间一条积分读数都没有，见
    /// `DailyEntry::credit`）。逐天累加而不是从三张维度表求和：维度表里的
    /// 组可能被手改的库改坏，日总量才是明细折算的权威投影。
    pub credit: Option<f64>,
    /// 区间内**上报了积分**的请求条数（覆盖度的分子）
    pub credit_rows: i64,
    /// 区间内的输入 token 之和（`prompt_tokens`，即 `cache_input_tokens` 那一列）。
    /// 积分单价（每百万输入 token）的分母；`tokens` 是另一档口径的分母。
    pub input_tokens: i64,
    pub model_totals: Vec<ModelAccum>,
    pub provider_totals: Vec<ProviderAccum>,
    pub account_totals: Vec<AccountAccum>,
}

/// 按区间累计聚合行。`BTreeMap::range` 直接按日期键取闭区间 ——
/// 定长日期串的字典序即时间序，这是选 `BTreeMap` 的直接收益。
pub(super) fn range_totals(
    daily: &BTreeMap<String, DailyEntry>,
    start: &str,
    end: &str,
) -> RangeTotals {
    let mut totals = RangeTotals {
        requests: 0,
        successful: 0,
        tokens: 0,
        active_days: 0,
        credit: None,
        credit_rows: 0,
        input_tokens: 0,
        model_totals: Vec::new(),
        provider_totals: Vec::new(),
        account_totals: Vec::new(),
    };
    // 闭区间 [start, end]：定长日期串的字典序即时间序，所以直接按键取范围。
    // 这里构造两个 String 边界是有意的取舍 —— 报表调用频率是「用户点一下」级别，
    // 两次小分配远不如让 `range` 的边界类型一目了然重要。
    for (_, day) in daily.range(start.to_string()..=end.to_string()) {
        totals.requests += day.requests;
        totals.successful += day.successful;
        totals.tokens += day.tokens;
        total_credit(&mut totals.credit, day.credit);
        totals.credit_rows += day.credit_rows;
        totals.input_tokens += day.cache_input_tokens;
        // activeDays 只数**区间内**有请求的天数（补零的日子不算「活跃」）
        if day.requests > 0 {
            totals.active_days += 1;
        }
        for acc in &day.model_tokens {
            push_model_accum(
                &mut totals.model_totals,
                &acc.model,
                acc.tokens,
                acc.requests,
                acc.credit,
            );
        }
        for acc in &day.provider_stats {
            push_provider_accum(
                &mut totals.provider_totals,
                &acc.provider,
                acc.requests,
                acc.successful,
                acc.tokens,
                acc.credit,
                acc.credit_rows,
            );
        }
        for acc in &day.account_stats {
            push_account_accum(
                &mut totals.account_totals,
                &acc.account_id,
                &acc.account_name,
                acc.requests,
                acc.successful,
                acc.tokens,
                acc.credit,
                acc.credit_rows,
            );
        }
    }

    // ── 旧聚合行的余量归属 ──────────────────────────────────────
    // 升级前写出的聚合行没有 `providerStats` 键（读入后是空表），那些天的请求
    // 因此没有任何 provider 组覆盖。这里把「未被任何组覆盖的余量」补进空 id 组：
    //   ① 各 provider 之和恒等于区间总量 —— 报表之间能对账，前端把
    //      `providers` 与 `overview.requests` 并排显示时不会出现对不上的数；
    //   ② 旧数据落进「未知」组而不是被静默丢掉 —— 契约要求不得给旧数据
    //      **猜**值（不许补 workbuddy），但「未知」正是它的真实归属，不是猜。
    // 先求和再借用 `&mut`：闭包持着不可变借用时不能同时改这张表。
    let covered_requests: i64 = totals.provider_totals.iter().map(|item| item.requests).sum();
    let covered_successful: i64 = totals.provider_totals.iter().map(|item| item.successful).sum();
    let covered_tokens: i64 = totals.provider_totals.iter().map(|item| item.tokens).sum();
    // 积分余量同理（旧行没有这一维）：差值落进「未知」组的积分槽。
    // `None` 表示整段区间都没有读数，此时余量也只能是 `None` —— 不能因为
    // 日总量有值就给未知组补一个 0，那会伪造出一组「花了 0 积分的未知承载者」。
    let covered_credit: f64 = totals.provider_totals.iter().filter_map(|item| item.credit).sum();
    let covered_credit_rows: i64 = totals.provider_totals.iter().map(|item| item.credit_rows).sum();
    let rest_credit_rows = (totals.credit_rows - covered_credit_rows).max(0);
    let rest_credit = if rest_credit_rows > 0 {
        Some((totals.credit.unwrap_or(0.0) - covered_credit).max(0.0))
    } else {
        None
    };
    // `max(0)` 兜住手改文件造成的「拆分比总量还多」；余量全零时这里仍会建出
    // 一个空组，但 `build_providers` 会把全零的组滤掉（见那边的 `requests > 0
    // || tokens > 0`），所以不会凭空冒出空的「未知」项
    push_provider_accum(
        &mut totals.provider_totals,
        "",
        (totals.requests - covered_requests).max(0),
        (totals.successful - covered_successful).max(0),
        (totals.tokens - covered_tokens).max(0),
        rest_credit,
        rest_credit_rows,
    );

    // ── 账号维度同理 ────────────────────────────────────────────
    // `accountStats` 是本次新增的键，**所有**已有聚合行都没有它，所以这一段
    // 在这里比 provider 那一段更常命中：升级后第一次打开报表，区间内几乎全部
    // 请求都会落进「未知账号」组。这是如实反映「那些行没记账号身份」，
    // 而不是我们丢数据 —— 明细里其实有，但聚合要跨过年份，只能按聚合算。
    let covered_requests: i64 = totals.account_totals.iter().map(|item| item.requests).sum();
    let covered_successful: i64 = totals.account_totals.iter().map(|item| item.successful).sum();
    let covered_tokens: i64 = totals.account_totals.iter().map(|item| item.tokens).sum();
    let covered_credit: f64 = totals.account_totals.iter().filter_map(|item| item.credit).sum();
    let covered_credit_rows: i64 = totals.account_totals.iter().map(|item| item.credit_rows).sum();
    let rest_credit_rows = (totals.credit_rows - covered_credit_rows).max(0);
    let rest_credit = if rest_credit_rows > 0 {
        Some((totals.credit.unwrap_or(0.0) - covered_credit).max(0.0))
    } else {
        None
    };
    push_account_accum(
        &mut totals.account_totals,
        "",
        "",
        (totals.requests - covered_requests).max(0),
        (totals.successful - covered_successful).max(0),
        (totals.tokens - covered_tokens).max(0),
        rest_credit,
        rest_credit_rows,
    );
    totals
}

/// 把一次请求（或一天的累计）并进按模型的累计表。
/// 线性查找即可：模型数量是个位到十位级，建 HashMap 反而更慢。
pub(super) fn push_model_accum(
    list: &mut Vec<ModelAccum>,
    model: &str,
    tokens: i64,
    requests: i64,
    credit: Option<f64>,
) {
    match list.iter_mut().find(|item| item.model == model) {
        Some(item) => {
            item.tokens += tokens;
            item.requests += requests;
            add_credit(&mut item.credit, credit);
        }
        None => list.push(ModelAccum {
            model: model.to_string(),
            requests,
            tokens,
            credit,
        }),
    }
}

/// 把一条（或一天的）积分读数并进可空的累计槽。
///
/// `Option` 的三态在这里是**唯一**的处理点：`None` 表示这一维还没出现过
/// 读数（保持 `None`），出现过一个就读数相加。明细侧的 `None`（没上报）由
/// 调用方跳过，不会走到这里 —— 所以这里只需处理「有 / 没有」两态。
fn add_credit(slot: &mut Option<f64>, value: Option<f64>) {
    if let Some(value) = value {
        *slot = Some(slot.unwrap_or(0.0) + value);
    }
}

/// [`range_totals`] 逐天累加用的同名动作 —— 与 [`add_credit`] 是同一个
/// 「`None` 不参与求和」的规则，但语义落在**区间**而不是**分组**上。
///
/// 单独包一层是为了让区间那处的调用点读起来是「累加这一天的积分」，
/// 而不是一段就地解包（那一行勾着 `DailyEntry::credit` 的三态说明）。
fn total_credit(slot: &mut Option<f64>, value: Option<f64>) {
    add_credit(slot, value);
}

/// 报表按模型聚合用的**统计键**：上游真名优先，请求名回落。
///
/// ── 为什么不能直接用 `model` ────────────────────────────────
/// 映射语义重做后（见 `pipeline::resolve_model` 的说明），请求名全程保持
/// 客户端原值，改写下沉到发送侧按家进行（`payload::send_body` →
/// `catalog::wire_target_for_provider`）。于是 `model`（请求侧解析名）在
/// 客户端点名映射别名时就是**别名本身** —— 报表若按它聚合，同一个上游模型
/// 会被拆成「真名 + 各家别名」好几行。而映射只是代名，实际请求的仍是上游
/// 那一个模型：`upstream_model`（telemetry 在发送侧采集的最终下发名）才是
/// 「用量花在哪个模型上」的正确答案。
///
/// ── 回落 ──────────────────────────────────────────────────
/// 转发前就失败的请求一次都没发出去，`upstream_model` 为空串 —— 请求名是
/// 此时我们所知的最好信息（token 已清零，只影响请求数维度）。还没有
/// `upstreamModel` 键的旧明细同样走回落：这不是「给旧数据猜值」，空串回落
/// 保留的是明细里本来就有的信息。
///
/// 口径的落点有三个（各维求和 = 当天总量的前提就在这里）：日报折算
/// （`aggregate::fold_into_daily`）、近 24 小时趋势（本文件的 `cache_trend_24h`）
/// 与整行重算（`aggregate::rebuild_day` 经前者），三处**必须调同一个函数**。
///
/// ── 为什么接两个 &str 而不是 &RequestEntry ───────────────────
/// 同一口径现在要服务两种行：完整的 `RequestEntry`（按天重算）与报表的窄行
/// [`SummaryRow`]（近 24 小时趋势，只读 6 列，见 `sql::select_summary_rows`）。
/// 若签名收 `&RequestEntry`，窄行那一侧就只能自己再写一遍 `if upstream 空就
/// 回落` —— 那正是这个函数要防的「同一口径两处维护」。取字段而不是取整行，
/// 两种调用方都能直接用。
pub(super) fn model_stat_key<'a>(model: &'a str, upstream_model: &'a str) -> &'a str {
    if upstream_model.is_empty() {
        model
    } else {
        upstream_model
    }
}

/// 模型分解 → 报表 JSON 数组（按天柱状图与 24 小时折线的悬停读数用）。
///
/// 形状 `[{model, label, tokens, requests}]`，按 tokens 降序（并列比 requests，
/// 再并列比名字，保证同一份数据多次调用顺序一致）。
///
/// 与 `build_models` 的两处口径差异：
///   - 全零组剔除的判据是 `tokens > 0`（这里只给悬停读数用，一条 token 为 0
///     的模型没有可读的量；请求数维度它是过程量，见 `fold_into_daily` 的说明）；
///   - 空模型名照常返回（label 为「未知模型」）：它代表的真实用量不该从读数里
///     消失，否则各行之和与当天总量对不上。
pub(super) fn model_breakdown_json(list: &[ModelAccum]) -> Vec<Value> {
    let mut rows: Vec<&ModelAccum> = list.iter().filter(|item| item.tokens > 0).collect();
    rows.sort_by(|left, right| {
        right
            .tokens
            .cmp(&left.tokens)
            .then(right.requests.cmp(&left.requests))
            .then(left.model.cmp(&right.model))
    });
    rows.into_iter()
        .map(|item| {
            json!({
                "model": item.model,
                "label": if item.model.is_empty() {
                    UNKNOWN_MODEL_LABEL.to_string()
                } else {
                    item.model.clone()
                },
                "tokens": item.tokens,
                "requests": item.requests,
                // 悬停读数里的积分同样三态：`null` = 这一格没有读数，
                // 前端按「未记录」渲染而不是 0（与主卡片同一口径）
                "credit": round_credit(item.credit),
            })
        })
        .collect()
}

/// 把一批（一条或一天的）请求并进按 provider 的累计表。
///
/// 与 `push_model_accum` 同一取舍：provider 数量是个位数，线性查找比 HashMap
/// 更快也更省（不必分配键）。`provider` 为空串时**照常建组**，不丢弃 ——
/// 旧数据与「转发前失败」的请求都落在这一组里，丢掉它们会让
/// 「各 provider 之和」对不上区间总量。
pub(super) fn push_provider_accum(
    list: &mut Vec<ProviderAccum>,
    provider: &str,
    requests: i64,
    successful: i64,
    tokens: i64,
    credit: Option<f64>,
    credit_rows: i64,
) {
    match list.iter_mut().find(|item| item.provider == provider) {
        Some(item) => {
            item.requests += requests;
            item.successful += successful;
            item.tokens += tokens;
            add_credit(&mut item.credit, credit);
            item.credit_rows += credit_rows;
        }
        None => list.push(ProviderAccum {
            provider: provider.to_string(),
            requests,
            successful,
            tokens,
            credit,
            credit_rows,
        }),
    }
}

/// 把一批（一条或一天的）请求并进按账号的累计表。
///
/// ── 身份只认 account_id ─────────────────────────────────────
/// 匹配按 `account_id` 走，**名字不参与判等**：账号可以在账号页改名，
/// 按名字匹配会让同一个账号在改名前后的两条聚合行分家，报表里出现两个半份的
/// 「同一账号」。名字在这里只承担一件事：**首次建组时留下的展示名快照**。
/// 后续同一账号的累计里若带了新名字，就更新快照（账号改名后报表跟着显示新名字，
/// 这也正是账号页的观感），而不是保留旧名。
///
/// ── 空 id 的处理 ─────────────────────────────────────────────
/// 与 `push_provider_accum` 同一取舍：`account_id` 为空串时**照常建组**，不丢弃 ——
/// 走默认登录态转发的请求、以及旧版本写出的行都落在这一组里，
/// 丢掉它们会让「各账号之和」对不上区间总量。
pub(super) fn push_account_accum(
    list: &mut Vec<AccountAccum>,
    account_id: &str,
    account_name: &str,
    requests: i64,
    successful: i64,
    tokens: i64,
    credit: Option<f64>,
    credit_rows: i64,
) {
    match list.iter_mut().find(|item| item.account_id == account_id) {
        Some(item) => {
            item.requests += requests;
            item.successful += successful;
            item.tokens += tokens;
            add_credit(&mut item.credit, credit);
            item.credit_rows += credit_rows;
            // 名字只做「快照刷新」：非空才覆盖，绝不清空已有的名字
            // （某些请求拿不到名字而另一些拿到了，取有名字的那一份）
            if !account_name.is_empty() {
                item.account_name = account_name.to_string();
            }
        }
        None => list.push(AccountAccum {
            account_id: account_id.to_string(),
            account_name: account_name.to_string(),
            requests,
            successful,
            tokens,
            credit,
            credit_rows,
        }),
    }
}

/// provider id → 展示 label。
///
/// 直接委托 `providers::label_of` —— 「id → 给人看的名字」的唯一入口，报表层
/// **不得**自建换算（注册表是 provider 身份的唯一事实来源，两处映射迟早漂移，
/// 而这类漂移不会报错，只会让报表里的名字与账号页对不上）。入口统一处理两类
/// 身份：内置 provider 走注册表 `meta().label`；`custom-` 前缀的自定义提供商
/// 查存储里用户起的名字 —— 那是运行期数据（config 内存快照，不碰磁盘），
/// 只走注册表的话会被原样回显成 `custom-…`。
///
/// 未登记 / 已删除的 id 仍由入口原样回显：显示一个陌生的英文 id 好过显示空白，
/// 至少能提示「这是谁」。空串同样原样返回（空 label），由展示层决定用什么
/// 占位（当前约定是「未知」/「—」）—— 这一层只回答「id 对应什么名字」，
/// 不做展示决策。
pub(super) fn provider_label(id: &str) -> String {
    crate::server::core::providers::label_of(id)
}

/// 区间级的按 provider 汇总 → `/api/stats/summary` 的 `providers` 数组。
///
/// 排序：`requests` 降序（契约要求）；并列时按 `totalTokens` 降序、
/// 再并列按 id 升序 —— 后两级纯粹为了让结果稳定（同一份数据多次调用顺序一致），
/// 前端做「只显示前 N 家」的截断时不会每次截到不同的行。
///
/// 空 id 那一组的 label 用「未知」而不是空串：这份数组是**给图表用的**，
/// 每段都要有可显示的图例名；空 label 在图例里就是个看不见的空白项。
/// （明细行那边的空 provider 反而保持空 label，好让展示层用「—」占位 ——
/// 两边形态不同是因为用途不同，不是遗漏。）
pub(super) fn build_providers(totals: &[ProviderAccum]) -> Vec<Value> {
    // 全零的组不返回：它只会出现在手改过的文件里，展示一个「0 次请求」的
    // 图例项没有意义（正常写入路径不会造出这种组）
    let mut rows: Vec<&ProviderAccum> = totals
        .iter()
        .filter(|item| item.requests > 0 || item.tokens > 0)
        .collect();
    rows.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then(right.tokens.cmp(&left.tokens))
            .then(left.provider.cmp(&right.provider))
    });
    rows.into_iter()
        .map(|item| {
            json!({
                "id": item.provider,
                "label": if item.provider.is_empty() {
                    UNKNOWN_PROVIDER_LABEL.to_string()
                } else {
                    provider_label(&item.provider)
                },
                "requests": item.requests,
                // failures 由减法得出而不是另存一列：见 record::ProviderAccum 的注释。
                // max(0) 兜住手改文件造成的负数
                "success": item.successful,
                "failures": (item.requests - item.successful).max(0),
                "totalTokens": item.tokens,
                // 积分（**新增键**，旧前端忽略）：三态口径同主卡片 ——
                // `null` = 这一家没有读数，`0` = 上报了 0
                "credit": round_credit(item.credit),
                "creditRows": item.credit_rows,
            })
        })
        .collect()
}

/// 未知模型的展示名（模型名为空串的那一组）。
///
/// 与 provider / 账号那两条是**三个独立的常量**：三者的「未知」成因各不相同
/// （没记下承载的家 / 没走账号列表 / 没记下模型名），文案将来若分叉，
/// 共用会让改一处等于改两处。
pub(super) const UNKNOWN_MODEL_LABEL: &str = "未知模型";

/// 区间级的按模型汇总 → `/api/stats/summary` 的 `models` 数组。
///
/// 与 `build_providers` / `build_accounts` 同形同序（requests 降序 → tokens 降序
/// → 名字升序）。模型维度比那两维更早存在（`model_totals` 一直是
/// `range_totals` 的一部分，`topModel` 就是从它算出来的），但此前只对外暴露了
/// 冠军一个 —— 这里把整张表给出去，供报表的「模型用量」环形图使用。
///
/// ── 与 providers 的两处差异 ─────────────────────────────────
///   1. **空模型名的组照常返回**（label 为「未知模型」）：`build_top_model` 特意
///      把空名字排除在冠军评选之外（避免标题栏显示空白），但这里是要画一张
///      「用量都花在哪」的图，那一组代表的真实用量不该从图上消失 ——
///      它会让各扇区之和对不上 `overview.tokens`。
///   2. 没有 `label` 与 `id` 的区分：模型名既是身份也是展示名，一个字段就够。
///
/// 全零的组仍然滤掉（与另两维一致）：只可能来自手改过的数据，
/// 画出来是一段永远为 0 的扇区。
pub(super) fn build_models(totals: &[ModelAccum]) -> Vec<Value> {
    let mut rows: Vec<&ModelAccum> = totals
        .iter()
        .filter(|item| item.requests > 0 || item.tokens > 0)
        .collect();
    rows.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then(right.tokens.cmp(&left.tokens))
            .then(left.model.cmp(&right.model))
    });
    rows.into_iter()
        .map(|item| {
            json!({
                "model": item.model,
                "label": if item.model.is_empty() {
                    UNKNOWN_MODEL_LABEL.to_string()
                } else {
                    item.model.clone()
                },
                "requests": item.requests,
                "totalTokens": item.tokens,
                "credit": round_credit(item.credit),
            })
        })
        .collect()
}

/// 区间级的按账号汇总 → `/api/stats/summary` 的 `accounts` 数组。
///
/// 与 `build_providers` 同形同序（requests 降序 → tokens 降序 → 身份升序）：
/// 两张排行并排显示时，排序口径一致才不会被误读成「一个按请求数、一个按别的」。
///
/// ── 与 providers 的两处差异 ─────────────────────────────────
///   1. 身份字段是 `id` + `label` 两个：provider 的 `label` 由注册表**现算**，
///      而账号的展示名是**聚合时留下的快照**（账号可能已被删除，注册表里查不到）。
///      取名字的优先级是「快照名 → id」，两者都空的那一组用「未知账号」。
///   2. 没有 `providerLabel` 那种「id → 现算 label」的派生：账号名不是派生值，
///      就是数据本身，所以直接原样给出。
pub(super) fn build_accounts(totals: &[AccountAccum]) -> Vec<Value> {
    // 与 build_providers 同一取舍：全零的组不返回
    let mut rows: Vec<&AccountAccum> = totals
        .iter()
        .filter(|item| item.requests > 0 || item.tokens > 0)
        .collect();
    rows.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then(right.tokens.cmp(&left.tokens))
            .then(left.account_id.cmp(&right.account_id))
    });
    rows.into_iter()
        .map(|item| {
            // 展示名取「快照名 → id」；两者都空才是真正的未知（默认登录态转发）
            let label = if !item.account_name.is_empty() {
                item.account_name.clone()
            } else if !item.account_id.is_empty() {
                item.account_id.clone()
            } else {
                UNKNOWN_ACCOUNT_LABEL.to_string()
            };
            json!({
                "id": item.account_id,
                "label": label,
                "requests": item.requests,
                "success": item.successful,
                "failures": (item.requests - item.successful).max(0),
                "totalTokens": item.tokens,
                "credit": round_credit(item.credit),
                "creditRows": item.credit_rows,
            })
        })
        .collect()
}

/// 明细行 → 响应 JSON：在序列化结果上补两个**派生字段**
/// `providerLabel` 与 `phaseElapsedMs`。
///
/// ── providerLabel ───────────────────────────────────────────
/// 为什么不在 `RequestEntry` 上加一个字段：契约里 `providerLabel` 是给前端直接
/// 显示的（§6「记账展示的 provider 用 label」），它由 id 换算而来、会随注册表
/// 变化，而 `RequestEntry` 是**落盘格式**——把派生值写进文件会让
/// 「以后改了 label，历史行仍是旧名字」。
///
/// ── phaseElapsedMs（本次新增，与上面同一手法）────────────────
/// 「在当前阶段里已经待了多久（毫秒）」，只对**在途**行（有 `phase` 的那几行）
/// 有值；不在途的行给 `null`，前端据此不渲染状态列的第二行。
///
/// 为什么现算而不是把耗时落库：耗时的两个端点里，起点（`phase_started_at`）
/// 是库里的事实、终点（现在）在读取那一刻才知道。落库一个值意味着它从写下去的
/// 那一刻就开始过期 —— 而状态列第二行的全部意义正是「它还在走」。
/// 现算还让读数与服务端时钟同源：浏览器时钟偏了也不会算出离谱的秒数
/// （与 OmniProxy 的 `phase_elapsed_ms` 用 `julianday('now')` 现减同一取向）。
///
/// `saturating_sub` + `max(0)`：手改过的库可能给出一个未来时刻，那种值算出来
/// 的负耗时会渲染成「-5秒」，两条都挡掉。
///
/// 序列化失败理论上不可能（字段全是 i64 / String / Option<String>），但仍然
/// 不做 unwrap（panic=abort 下会带走整个应用）：退化成一个空对象，
/// 前端那一行显示空白，比进程消失好。
pub(super) fn entry_json(entry: &RequestEntry) -> Value {
    let Ok(mut value) = serde_json::to_value(entry) else {
        return Value::Object(serde_json::Map::new());
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("clientLabel".into(), json!(crate::server::core::client::display_name(
            &entry.source.client, entry.source.recorded_client_name(),
        )));
        object.insert(
            "providerLabel".to_string(),
            Value::String(provider_label(&entry.provider)),
        );
        object.insert(
            "phaseElapsedMs".to_string(),
            match entry.phase_started_at {
                Some(started) => json!((now_ms() - started).max(0)),
                None => Value::Null,
            },
        );
    }
    value
}

/// 区间级 topModel：按 `tokens` 降序，并列比 `requests`，再并列比名字
/// （名字兜底是为了结果稳定，不受聚合行内部顺序影响）。
/// 无数据或所有记录都没带模型名时返回 `null`。
pub(super) fn build_top_model(totals: &[ModelAccum], range_tokens: i64) -> Value {
    let best = totals
        .iter()
        // 空模型名不参与评选：否则会冒出个「空名字冠军」，标题栏显示成空白，
        // 比不给结果更像故障
        .filter(|item| !item.model.is_empty() && (item.tokens > 0 || item.requests > 0))
        .max_by(|left, right| {
            left.tokens
                .cmp(&right.tokens)
                .then(left.requests.cmp(&right.requests))
                // 取反：名字小的排前面，保证并列时结果确定
                .then(right.model.cmp(&left.model))
        });
    let Some(best) = best else {
        return Value::Null;
    };
    // percentage = 该模型 tokens / 区间总 tokens；总为 0 时给 0.0，
    // 绝不产生 NaN/Infinity（serde_json 序列化非有限浮点会 panic 或产出 null）
    let percentage = if range_tokens > 0 {
        best.tokens as f64 / range_tokens as f64
    } else {
        0.0
    };
    json!({
        "model": best.model,
        "tokens": best.tokens,
        "requests": best.requests,
        "percentage": percentage,
    })
}

/// 范围内的逐天序列（缺失日期补零，升序）。
///
/// 前端按天画柱状图，缺的那天必须占位（补 0）而不是不返回 ——
/// 少一天会让整条曲线在视觉上被压缩，趋势看起来是错的。
///
/// 每天额外带 `models`（`[{model, label, tokens, requests}]`，按 tokens 降序）：
/// 柱状图的悬停读数要给出「这一天总用量是多少、分别花在哪些模型上」，
/// 而按模型的分解只存在于聚合行（`model_tokens` 列）—— 明细只在报表读明细的
/// 24 小时窗口里才被读，这里拿不到也不会回算（见 [`model_breakdown_json`]）。
pub(super) fn daily_trend(daily: &BTreeMap<String, DailyEntry>, start: &str, end: &str) -> Vec<Value> {
    let (Some(start_date), Some(end_date)) = (parse_key(start), parse_key(end)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = start_date;
    // 上限兜底：手改文件把区间撑到十年也不会把响应撑爆
    while cursor <= end_date && out.len() < MAX_TREND_DAYS as usize {
        let key = date_key(cursor);
        let day = daily.get(&key);
        let (tokens, requests) = day.map(|item| (item.tokens, item.requests)).unwrap_or((0, 0));
        let models = day.map(|item| super::source_report::all_model_rows(&item.model_tokens)).unwrap_or_default();
        let (clients, api_keys) = day.map(super::source_report::source_breakdown).unwrap_or_default();
        // 积分（**新增键**）：`credit` 为 `null` 表示这天没有读数（前端画线时
        // 留断点而不是画到 0），`ppm` 同口径也是 `null`
        let credit = day.and_then(|item| item.credit);
        let credit_rows = day.map(|item| item.credit_rows).unwrap_or(0);
        out.push(json!({
            "date": key,
            "tokens": tokens,
            "requests": requests,
            "models": models,
            "clients": clients,
            "apiKeys": api_keys,
            "credit": round_credit(credit),
            "creditRows": credit_rows,
            "ppm": credit_rate(credit, Some(tokens)),
        }));
        cursor += ChronoDuration::days(1);
    }
    out
}

/// 热力图：**固定 365 天**（含今天），与 range 解耦，无数据的日期补零。
/// 升序返回，前端按「一年 = 若干列周」排布。
pub(super) fn heatmap(daily: &BTreeMap<String, DailyEntry>, today: NaiveDate) -> Vec<Value> {
    let start = today - ChronoDuration::days(HEATMAP_DAYS - 1);
    let mut out = Vec::with_capacity(HEATMAP_DAYS as usize);
    let mut cursor = start;
    while cursor <= today {
        let key = date_key(cursor);
        let (requests, tokens) = daily
            .get(&key)
            .map(|day| (day.requests, day.tokens))
            .unwrap_or((0, 0));
        out.push(json!({ "date": key, "requests": requests, "tokens": tokens }));
        cursor += ChronoDuration::days(1);
    }
    out
}

/// 连续有请求的天数：从**本地今天**往前数。
///
/// 「今天还没有请求不算断」—— 凌晨打开报表时今天通常还是空的，
/// 若把今天算成断，连续天数每天都要先归零一次，与用户直觉相反。
/// 所以今天没记录时从昨天起算。
pub(super) fn streak(daily: &BTreeMap<String, DailyEntry>, today: NaiveDate) -> i64 {
    let has = |day: NaiveDate| -> bool {
        daily
            .get(&date_key(day))
            .is_some_and(|item| item.requests > 0)
    };
    let mut cursor = today;
    if !has(cursor) {
        cursor -= ChronoDuration::days(1);
    }
    let mut count = 0i64;
    // 上限兜底：手改数据造出「万年连续」也不会死循环
    while count < MAX_TREND_DAYS && has(cursor) {
        count += 1;
        cursor -= ChronoDuration::days(1);
    }
    count
}

// ─── 缓存命中率（窗口都很短，必须从明细算）────────────────────

/// 四档缓存命中率
pub(super) fn cache_rates(totals: &[(i64, i64)]) -> Value {
    let mut out = serde_json::Map::new();
    for (index, (key, _window)) in CACHE_WINDOWS.iter().enumerate() {
        // 缺项按「这个窗口没有数据」补零（不该发生：SQL 侧每个窗口都有一组
        // 合计，见 `sql::sum_cache_windows`）—— 与 safe_rate 的零除口径一致
        let (hit, input) = totals.get(index).copied().unwrap_or((0, 0));
        out.insert(
            key.to_string(),
            json!({ "hitTokens": hit, "inputTokens": input, "rate": safe_rate(hit, input) }),
        );
    }
    Value::Object(out)
}

/// 积分读数的小数位归一：保留 4 位。
///
/// 上游的 `credit` 是浮点数（WorkBuddy 实测 0.03～0.31），几十条相加之后会
/// 出现 `0.30000000000000004` 这类尾数。它不影响计算，但会原样印在界面上；
/// 在这里收一次口，比让每个前端调用点各写一遍 `toFixed` 更可靠。
///
/// `None` 原样返回 —— 「没有读数」不能被归一成一个数。
fn round_credit(value: Option<f64>) -> Option<f64> {
    value
        .filter(|value| value.is_finite())
        .map(|value| (value * 10_000.0).round() / 10_000.0)
}

/// 「每百万 token 多少积分」。
///
/// 分母为 0（区间里没有用量）或分子没有读数时返回 `None`：前端显示「—」，
/// 而不是 0 或 NaN。**不夹取**（与 `safe_rate` 同一取向）：积分与 token
/// 是两个独立读数，上游可能给出「有积分、没报 token」这类组合，如实返回是
/// 唯一不误导的做法。
fn credit_rate(credit: Option<f64>, tokens: Option<i64>) -> Option<f64> {
    let credit = credit.filter(|value| value.is_finite() && *value >= 0.0)?;
    let tokens = tokens?;
    if tokens <= 0 {
        return None;
    }
    let rate = credit / (tokens as f64 / 1_000_000.0);
    rate.is_finite().then_some((rate * 100.0).round() / 100.0)
}

/// 区间级的积分维度 → `/api/stats/summary` 的 `credits` 键（**新增键**）。
///
/// ── 它回答的三个问题 ────────────────────────────────────────
///   1. 这段时间一共花了多少积分（`total`）；
///   2. 平均每百万 token 要多少积分（`ppm` / `ppmInput`）——
///      用户口径的「换算」就是这一条；
///   3. 按 provider / 账号 / 模型分别是多少（三张排行）。
///
/// ── `coverage` 与 `complete` ────────────────────────────────
/// 积分是**逐家自愿上报**的读数（WorkBuddy 的 `usage.credit` 有，别家没有）。
/// 请求数减去「上报了积分的条数」就是这段时间里读不到的那一截。`coverage`
/// 让界面能说「这个合计只覆盖了 81% 的成功请求」，而不是把一个偏小的数
/// 当成全部消耗。`complete` 是 `coverage == 1` 的布尔投影，省得前端比浮点。
///
/// ── `mixed` ────────────────────────────────────────────────
/// `core::upstream::usage::CREDIT_KEYS` 把 Cline 的 `usage.cost`（**美元**）
/// 与积分共用同一列。多于一家都在报读数时，`total` 是一个没有单位的和 ——
/// 界面据此把「总积分」换成「分家表格」展示（见 `report-page` 的说明）。
///
/// ── `forecast` ─────────────────────────────────────────────
/// 近 7 天（含今天）的逐日积分与加权单价，以及「按日均消耗，剩余额度还能
/// 跑几天」。`daysLeft` 需要两侧同源（余额与消耗是同一单位），只有
/// `account_usage_records.remaining` 有值时才可能算出来 —— 见
/// `days_left` 的说明；这里只把原料交给前端，除法留给前端做：它知道
/// 当前账号页读到的余额，不必让报表接口再去查一次账号表。
///
/// 入参是 `range_totals` 的结果（区间总量与三张维度表），本函数**纯函数**、
/// 不碰库。
pub(super) fn credit_summary(
    totals: &RangeTotals,
    daily: &BTreeMap<String, DailyEntry>,
    start: &str,
    end: &str,
    today: NaiveDate,
) -> Value {
    let credit = round_credit(totals.credit);
    // coverage 的分母用 successful 而不是 requests：失败的请求不产生用量
    // （`normalize` 已把失败的 credit/token 清空），拿它们当分母只会把覆盖度
    // 无端拉低。
    let coverage = if totals.successful > 0 {
        (totals.credit_rows as f64 / totals.successful as f64).min(1.0)
    } else {
        0.0
    };
    let providers = build_credit_groups(&totals.provider_totals.iter().map(|item| CreditGroup {
        id: item.provider.clone(),
        label: if item.provider.is_empty() {
            UNKNOWN_PROVIDER_LABEL.to_string()
        } else {
            provider_label(&item.provider)
        },
        requests: item.requests,
        successful: item.successful,
        credit: item.credit,
        credit_rows: item.credit_rows,
        tokens: item.tokens,
    }).collect::<Vec<_>>());
    let accounts = build_credit_groups(&totals.account_totals.iter().map(|item| CreditGroup {
        id: item.account_id.clone(),
        // 与 `build_accounts` 同一取法：快照名 → id → 「未知账号」
        label: if !item.account_name.is_empty() {
            item.account_name.clone()
        } else if !item.account_id.is_empty() {
            item.account_id.clone()
        } else {
            UNKNOWN_ACCOUNT_LABEL.to_string()
        },
        requests: item.requests,
        successful: item.successful,
        credit: item.credit,
        credit_rows: item.credit_rows,
        tokens: item.tokens,
    }).collect::<Vec<_>>());
    let models = build_credit_groups(&totals.model_totals.iter().map(|item| CreditGroup {
        id: item.model.clone(),
        label: if item.model.is_empty() {
            UNKNOWN_MODEL_LABEL.to_string()
        } else {
            item.model.clone()
        },
        requests: item.requests,
        // 模型累计没有成功 / 失败之分（`ModelAccum` 只记条数），
        // 覆盖度在这里退化成「有读数 / 请求数」，与其它两维同口径
        successful: item.requests,
        credit: item.credit,
        // 模型累计没有单列 credit_rows（它的口径就是「这一天有几条」）：
        // 有读数就按该组的请求数算，没有就是 0
        credit_rows: if item.credit.is_some() { item.requests } else { 0 },
        tokens: item.tokens,
    }).collect::<Vec<_>>());

    // ── 混单位的判据 ──────────────────────────────────────────
    // 只数**有积分读数且身份非空**的提供商：
    //   - 空 id 那一组是「未知承载者」的余量（旧聚合行没有 provider 维度），
    //     它不是一个独立的计费单位，把它算进去会让「只有一家在报积分」的用户
    //     被误判成混单位；
    //   - 没有读数的家不贡献任何数值，自然也谈不上单位。
    // 只有一家时合计是可信的；两家以上时合计没有单位意义。
    let reporting = totals
        .provider_totals
        .iter()
        .filter(|item| item.credit.is_some() && !item.provider.is_empty())
        .count();
    let mixed = reporting > 1;

    let forecast = credit_forecast(daily, today);

    json!({
        "total": credit,
        "rows": totals.credit_rows,
        "requests": totals.requests,
        "successful": totals.successful,
        "coverage": (coverage * 10_000.0).round() / 10_000.0,
        "complete": totals.successful > 0 && totals.credit_rows >= totals.successful,
        "mixed": mixed,
        "tokens": totals.tokens,
        "inputTokens": totals.input_tokens,
        "ppm": credit_rate(totals.credit, Some(totals.tokens)),
        "ppmInput": credit_rate(totals.credit, Some(totals.input_tokens)),
        "byProvider": providers,
        "byAccount": accounts,
        "byModel": models,
        // 区间起止随读返回：预测卡的文案要写「近 7 天」到底是哪 7 天，
        // 让前端自己算日期会在跨年 / 跨时区时与后端错开
        "range": { "start": start, "end": end },
        "forecast": forecast,
    })
}

/// 积分维度的一行（三个排行共用）。`id` 在模型维度上是模型名 ——
/// 与 `build_models` 同一取舍（模型名既是身份也是展示名）。
struct CreditGroup {
    id: String,
    label: String,
    requests: i64,
    /// 成功请求数（覆盖度的分母；与 `credit_rows` 同一口径）。
    /// 模型累计没有这一列，直接用 `requests`（模型维度的每一条都是定局行）。
    successful: i64,
    credit: Option<f64>,
    credit_rows: i64,
    tokens: i64,
}

/// 积分排行：按积分降序，并列比 tokens、再并列比 id。
///
/// ── 只保留**有读数**的组 ─────────────────────────────────────
/// 与 `build_providers` 的「全零组不返回」取向一致，但判据换成「有没有读数」：
/// 这家跑了 100 次请求却一条积分都不报，出现在一张叫「积分消耗排行」的表里
/// 只会是一行「—」。它们没有贡献任何数值，滤掉之后这张表之和仍然等于
/// `total`（见 `credit_summary` 的 coverage 说明 —— 覆盖不到的请求由覆盖率
/// 这个读数表达，不由空行表达）。
///
/// **有读数但为 0 的组照常返回**：一家报 0 积分是一条真实信息。
fn build_credit_groups(groups: &[CreditGroup]) -> Vec<Value> {
    let mut rows: Vec<&CreditGroup> = groups
        .iter()
        .filter(|item| item.credit.is_some())
        .collect();
    rows.sort_by(|left, right| {
        let credit = |row: &CreditGroup| row.credit.unwrap_or(f64::NEG_INFINITY);
        credit(right)
            .partial_cmp(&credit(left))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(right.tokens.cmp(&left.tokens))
            .then(left.id.cmp(&right.id))
    });
    rows.into_iter()
        .map(|item| {
            json!({
                "id": item.id,
                "label": item.label,
                "requests": item.requests,
                // `success` 与 `build_providers` / `build_accounts` 同名同义：
                // 积分卡的「覆盖率」列要用它当分母（失败请求不产生用量）
                "success": item.successful,
                "credit": round_credit(item.credit),
                "creditRows": item.credit_rows,
                "tokens": item.tokens,
                "ppm": credit_rate(item.credit, Some(item.tokens)),
            })
        })
        .collect()
}

/// 近 7 天（含今天）的积分走势与加权单价。
///
/// ── 为什么固定 7 天、与 range 解耦 ────────────────────────────
/// 预测要的是「最近的消耗速度」。把区间切到「今天」时近 7 天仍然要看得到
/// （否则只有一两天的数据，预测会剧烈跳动）；切到「全部」时也只取最近一周
/// （一年前的价格对现在的预测没有意义）。这与热力图固定 365 天是同一取舍：
/// 预测卡有自己的时间窗口，不跟筛选器走。
///
/// ── 加权为什么用 0.75^k 而不是等权 ──────────────────────────
/// 积分单价随模型与缓存命中率漂移。等权平均会让 7 天前的一次性大单一直
/// 拖住预测值；指数衰减让「昨天」比「6 天前」重一倍多（0.75^-1 ≈ 1.33），
/// 又不至于像「只看最近两天」那样被一次异常请求带偏。
///
/// 返回的 `dailyCredit` 是**逐日原值**（含 0 与 `null`），前端折线直接用；
/// 加权只在单价`weightedPpm` 里做。两者分开是刻意的：图上要给人看真实读数，
/// 加权是模型内部的中间量。
fn credit_forecast(daily: &BTreeMap<String, DailyEntry>, today: NaiveDate) -> Value {
    const WINDOW: i64 = 7;
    let mut points = Vec::with_capacity(WINDOW as usize);
    let mut weight_credit = 0.0;
    let mut weight_tokens = 0.0;
    let mut credit_days = 0i64;
    for offset in (0..WINDOW).rev() {
        let day = today - ChronoDuration::days(offset);
        let key = date_key(day);
        let row = daily.get(&key);
        let credit = row.and_then(|item| item.credit);
        // 权重：今天 = 1，昨天 = 0.75，依此类推
        let weight = 0.75f64.powi(offset as i32);
        if let Some(value) = credit {
            credit_days += 1;
            weight_credit += value * weight;
            // 单价的分母同样按权重收：这天没报 token 时它就是 0 贡献，
            // 不会凭空放大单价（分子权重与分母权重同步）
            weight_tokens += row.map(|item| item.tokens).unwrap_or(0) as f64 * weight;
        }
        points.push(json!({
            "date": key,
            // `round_credit(None)` 序列化成 `null` —— 折线据此留断点
            "credit": round_credit(credit),
            "tokens": row.map(|item| item.tokens).unwrap_or(0),
            "requests": row.map(|item| item.requests).unwrap_or(0),
        }));
    }
    let weighted_ppm = if weight_tokens > 0.0 && weight_credit >= 0.0 {
        let rate = weight_credit / (weight_tokens / 1_000_000.0);
        rate.is_finite().then(|| (rate * 100.0).round() / 100.0)
    } else {
        None
    };
    // 日均：只按**有读数的天数**平均，否则「不报积分的那几天」会把均值稀释成 0，
    // 从而让「还能用几天」被高估。
    let daily_avg = if credit_days > 0 {
        let sum: f64 = (0..WINDOW)
            .filter_map(|offset| {
                let key = date_key(today - ChronoDuration::days(offset));
                daily.get(&key).and_then(|item| item.credit)
            })
            .sum();
        Some(((sum / credit_days as f64) * 10_000.0).round() / 10_000.0)
    } else {
        None
    };
    json!({
        "windowDays": WINDOW,
        "creditDays": credit_days,
        "points": points,
        "weightedPpm": weighted_ppm,
        // 「百万 token 要多少积分」的直接答案（未加权版本用区间单价，
        // 这里给近 7 天口径，预测卡两处都展示时不会互相矛盾）
        "perMillion": weighted_ppm,
        "perThreeMillion": weighted_ppm.map(|rate| (rate * 3.0 * 100.0).round() / 100.0),
        "dailyAvg": daily_avg,
        // 注意：`daysLeft` 不在这里算 —— 它需要余额，而余额属于账号域。
        // 前端已知当前账号余额时用 `dailyAvg` 除一下即可（见 report-page）。
    })
}

/// 近 24 个**本地整点**的命中率与用量趋势（无数据的整点补零）。
///
/// 按本地时区切整点：先把时间戳还原成 `DateTime<Local>` 再取 `%H`，
/// 于是「14 点」就是用户时钟上的 14 点，不是 UTC 的 14 点。
///
/// 每个整点同时给出 `totalTokens`：前端那一张图要画**两条线**
/// （命中率 + 总 Token），两段数据必须同源同桶 —— 分成两次请求会让两条线
/// 在「这一小时算不算」上出现分歧，看起来像其中一条错位了一格。
///
/// 每个整点还带 `models`（`[{model, label, tokens, requests}]`，按 tokens 降序）：
/// 折线图的悬停读数要给出「这一小时总用量是多少、分别花在哪些模型上」。
/// 模型统计键走 [`model_stat_key`]（与日报折算同一口径），于是同一小时里
/// 「各模型之和」必然等于该小时的 `totalTokens`（当份数据之内）。
///
/// 入参是**报表窄行**（[`SummaryRow`]，只 6 列）而不是完整明细：本函数是改造后
/// 唯一还要逐条读明细的地方，而它需要的字段全在窄行的列集里（见
/// `sql::select_summary_rows`）。窗口外（`ts` 早于 23 个整点前）的行照旧跳过 ——
/// 调用方给的可能是整个 `request_stats::SUMMARY_WINDOW_MS` 窗口，两者在边界上
/// 不完全重合（本地时区的整点切法与「now 往前推 N 小时」差几十分钟）。
pub(super) fn cache_trend_24h(entries: &[SummaryRow], now: i64) -> Vec<Value> {
    /// 单个整点的桶：三项总量 + 按模型的累计（模型数是个位到十位级，
    /// 线性查找即可 —— 与 `push_model_accum` 同一取舍）
    #[derive(Default)]
    struct HourBucket {
        hit: i64,
        input: i64,
        total: i64,
        models: Vec<ModelAccum>,
    }

    let start = hour_floor(ms_to_local(now)) - ChronoDuration::hours(23);
    let start_ms = start.timestamp_millis();

    // 先把窗口内的条目按整点键归桶，再按 24 个整点取值 ——
    // 逐个整点扫一遍明细是 24×N，这样只需一次遍历
    let mut buckets: HashMap<String, HourBucket> = HashMap::new();
    for item in entries {
        if item.ts < start_ms || item.ts > now {
            continue;
        }
        let slot = buckets.entry(hour_key(ms_to_local(item.ts))).or_default();
        slot.hit += item.cache_read_tokens;
        slot.input += item.prompt_tokens;
        slot.total += item.total_tokens;
        push_model_accum(
            &mut slot.models,
            model_stat_key(&item.model, &item.upstream_model),
            item.total_tokens,
            1,
            // 近 24 小时这一档**不带积分**：`SummaryRow` 只有 6 列（含
            // `idx_requests_summary` 的纯索引扫描），加一列 `credit` 会让那条
            // 查询退化成回表。积分的时间维度由 `dailyTrend`（按天，来自聚合行）
            // 承担，逐小时的积分不是本次要回答的问题。
            None,
        );
    }

    let mut out = Vec::with_capacity(24);
    for step in 0..24 {
        let key = hour_key(start + ChronoDuration::hours(step));
        let slot = buckets.remove(&key).unwrap_or_default();
        out.push(json!({
            "hour": key,
            "hitTokens": slot.hit,
            "inputTokens": slot.input,
            "totalTokens": slot.total,
            "rate": safe_rate(slot.hit, slot.input),
            "models": model_breakdown_json(&slot.models),
        }));
    }
    out
}

/// 命中率：`hit / input`，分母为 0（或无命中）时返回 0.0。
///
/// **必须挡住零除**：0/0 在浮点下是 NaN，`serde_json` 序列化 NaN 时
/// 要么 panic 要么产出 `null`，前端拿到非数字后图表直接空白。
/// 这里也挡住负数（手改文件可能塞进负值）——「命中」为负没有物理意义，
/// 直接按 0 处理。
///
/// 注意**不把结果夹到 1.0**：上游有些接口把 cacheRead 与 promptTokens
/// 分开报（promptTokens 不含缓存部分），此时命中率合法地会超过 100%。
/// 夹一下确实更好看，但那是拿数据真实性换观感 —— 报表要反映真实比值，
/// 越界与否交给前端展示层决定。
pub(super) fn safe_rate(hit: i64, input: i64) -> f64 {
    if input <= 0 || hit <= 0 {
        return 0.0;
    }
    let rate = hit as f64 / input as f64;
    if rate.is_finite() {
        rate
    } else {
        0.0
    }
}

/// status 过滤归一的**结果**：三种取值各自对应一个 SQL 条件（见
/// `sql::FilterPlan::of` 里逐个的写法）。
///
/// 为什么从「`Option<bool>`」扩成枚举：原来只有 ok / error 两个值，bool 够用；
/// 「进行中」（running，status=0）加入后是第三种互斥状态 —— 硬塞 bool 要么
/// 让调用方先判字符串再判 bool，要么把 running 混进 error 的取反里。
/// 三分支枚举让「漏处理新分支」在编译期就暴露。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StatusFilter {
    /// 只看成功（2xx 且无错误摘要）
    Ok,
    /// 只看失败（成功条件的取反；**不含**进行中行，见 `sql::FilterPlan::of`）
    Error,
    /// 只看进行中（status = 0，见 `request_stats::record_started`）
    Running,
}

/// status 过滤归一：`"ok"` → 成功，`"error"` → 失败，`"running"` → 进行中，
/// 其余（含 None 与拼错的值）不过滤。大小写不敏感、容忍前后空白。
pub(super) fn normalize_status_filter(value: Option<&str>) -> Option<StatusFilter> {
    match value.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("ok") => Some(StatusFilter::Ok),
        Some("error") => Some(StatusFilter::Error),
        Some("running") => Some(StatusFilter::Running),
        _ => None,
    }
}

/// `YYYY-MM-DD` → NaiveDate（手改文件里的坏值返回 None，调用方各自兜底）
fn parse_key(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}
