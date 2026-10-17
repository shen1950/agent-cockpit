// 从本机直读客户端自己存的余额：具体解密在 agenthub（它已解决 v10/DPAPI 这套落点），
// 这里负责调用、换算成驾驶舱的多笔积分结构，并做"账号绑定"校验防止串号。
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 驾驶舱程序 id → agenthub 认识的产品名。只有确证能本机直读的才列进来。
pub const LOCAL_STORES: &[(&str, &str)] = &[
    ("qoder-cn", "qoder-cn"),
    ("qoder", "qoder"),
    ("workbuddy", "workbuddy"),
    ("workbuddy-intl", "workbuddy-intl"),
    ("astudio", "astudio"),
    ("minimax", "minimax"),
    ("traework-cn", "trae-cn"),
    // ZCode（智谱）：凭据 enc:v1 是 AES-256-GCM，密钥纯本地派生；登录态就在实例目录自己的
    // v2/credentials.json 里（~/.zcode 整体被换号），所以按账号目录读即可、无需回落。
    ("zcode", "zcode"),
    // 2026-10-11 新登记四家（接口与落点都已实测；agenthub 侧产品名与 id 同名）：
    // 千问办公：{APPDATA}\QwenWorkCN\auth-v2.dat（os_crypt v10 信封）
    ("qwenwork", "qwenwork"),
    // DeepSeek Harness：~/.dsh/.credentials.yaml 完全明文
    ("dsh", "dsh"),
    // Marvis：.marvis（symlink→腾讯用户目录）\mobile_address.json 明文 access_token
    ("marvis", "marvis"),
    // Xiaomi MiMo：{APPDATA}\Xiaomi MiMo\Partitions\xiaomi-account\Network\Cookies（SQLite, os_crypt）
    ("xiaomi-mimo", "xiaomi-mimo"),
    // 2026-10-18 新登记两家（都只能认出"登的是谁"、读不到额度，照 MiMo 的规矩：
    // left/total 报 None，绝不拿 0 冒充余额；登录态都是整机一份的固定位置）：
    //   antigravity  {APPDATA}\Antigravity\User\globalStorage\state.vscdb 的
    //                antigravityAuthStatus 明文 JSON（name/apiKey/email，Google OAuth 账号）。
    //                厂商没有任何余额接口——按 Google 账号计费。
    //   coze         {APPDATA}\Coze\Partitions\coze-browser…\Local Storage\leveldb 里
    //                view_mode 的 "user" 字段（数字 userId）。没有昵称/邮箱/余额接口。
    ("antigravity", "antigravity"),
    ("coze", "coze"),
];

pub fn store_for(app_id: &str) -> Option<&'static str> {
    LOCAL_STORES.iter().find(|(id, _)| *id == app_id).map(|(_, s)| *s)
}

/// 反查：agenthub 的 store 名 → 驾驶舱的 app id。LOCAL_STORES 是 1:1 的，所以反查唯一。
/// 签到页要按账号展开时，手上只有 store（agenthub 报的），得换回 app id 才能找到账号卡。
pub fn app_for_store(store: &str) -> Option<&'static str> {
    LOCAL_STORES.iter().find(|(_, s)| *s == store).map(|(id, _)| *id)
}

/// 已登记直读通道的产品显示名（用 registry 的名字，没登记 id 的退回原样）。
/// 提示文案**别再写死一串名字**——那是第二份真值，加一家忘一家（这次修的就是它过期了）。
pub fn locally_readable_names() -> String {
    LOCAL_STORES
        .iter()
        .map(|(id, _)| {
            crate::registry::app_def(id).map(|a| a.name).unwrap_or_else(|| id.to_string())
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

#[derive(Debug, Clone)]
pub struct Quota {
    pub user_id: String,
    pub account: String,
    pub user_type: String,
    /// 厂商没给余额就是没给：这里必须是 Option，`unwrap_or(0.0)` 会把
    /// 「读不到额度」画成「剩 0 积分」——那是假话（Xiaomi MiMo 只有登录态可读）。
    pub left: Option<f64>,
    pub total: Option<f64>,
    pub expires_on: String,
    pub exceeded: bool,
    pub portions: Vec<Value>,
    /// agenthub 按 WorkBuddy 官方那三档归好的大组（套餐基础积分 / 平台奖励积分 / 购买积分），
    /// 每组带 items[] 逐笔明细与到期时间（精确到分）。界面照这层画，不再让用户自己拼。
    pub groups: Vec<Value>,
    /// 官方用量弹窗顶部那个套餐名（体验版 / 专业版），空 = 该产品没给
    pub plan_name: String,
    /// 计量单位跟着厂商口径走：Qoder/WorkBuddy 是 credits、AStudio 是积分、
    /// ZCode 与 Marvis 按 token 计数、DeepSeek 平台是人民币元。界面写死"积分"会把
    /// 3 亿 token 说成 3 亿积分，量级完全不是一回事。
    pub unit: String,
    /// "full" = 余额/到期日一应俱全；"usage_only" = 只有本机累计已用，余额读不到
    pub kind: String,
    pub used: f64,
    pub sessions: i64,
    pub last_active: String,
    /// "server" = 服务端实时数；其余（如 "local-cache"）= 客户端上次联网时的缓存
    pub source: String,
    /// 服务端那次读取失败的原因（空 = 实时读成功）。含 TOKEN_EXPIRE = 登录令牌已过期
    pub live_error: String,
    /// 登录态实际是从哪个文件读到的（Qoder 0.4.x 换壳后这是唯一能看出"读错档案"的线索）
    pub auth_from: String,
    /// 按天消耗序列（波形图与热力图的数据源）：`{unit,days[],total,peak,peak_date,from,to}`，
    /// 读不出来时是 `{error:"…"}`，agenthub 那个产品压根没有逐笔时刻可聚时为 `Null`。
    /// 只有 WorkBuddy 系有这份数——它的本地库逐会话记了各资源包扣掉多少 credits。
    pub trend: Value,
}

impl Quota {
    pub fn is_usage_only(&self) -> bool {
        self.kind == "usage_only"
    }

    /// 只认到账号、没有任何额度数字的产品（MiMo / Antigravity / Coze）：
    /// left/total/逐笔明细全空。话术上必须避开"剩 未给出 / 未给出"——那不是
    /// "没读到"，是"厂商根本没有可显示的余额"（拿 0 冒充更是撒谎）。
    pub fn is_account_only(&self) -> bool {
        self.left.is_none() && self.total.is_none() && self.portions.is_empty() && !self.is_usage_only()
    }

    /// 界面话术用的单位词：`元` 直接显示，其余（credits/积分/token）保持厂商原词。
    pub fn unit(&self) -> &str {
        if self.unit.is_empty() { "credits" } else { self.unit.as_str() }
    }

    /// 只拿到「客户端上次联网时的缓存」、且服务端明确报了错：这份数不能当实时余额用。
    /// Qoder 的登录令牌过期后就是这样——服务端 401，本地只剩 8 月的旧快照，
    /// 若照原样显示，用户看到的是一个早已作废的「剩 500/500」。
    pub fn is_stale(&self) -> bool {
        // account_only 也排除：它的 live_error 说的是"厂商没有余额接口"（解释），
        // 不是"服务端没读成、只剩旧缓存"（警告）——把解释当警告会弹出一句
        // "上面的数字是缓存"对着一个根本没有数字的画面
        !self.is_usage_only() && !self.is_account_only()
            && self.source != "server" && !self.live_error.trim().is_empty()
    }

    /// 令牌过期（要重新登录，或打开一次产品让它自动续期）
    pub fn token_expired(&self) -> bool {
        let e = self.live_error.to_ascii_uppercase();
        e.contains("TOKEN_EXPIRE") || e.contains("UNAUTHORIZED") || e.contains("401")
    }

    /// 说人话地解释「为什么这份数不能信、该怎么办」
    pub fn stale_hint(&self) -> String {
        if !self.is_stale() {
            return String::new();
        }
        if self.token_expired() {
            "读不到服务端实时余额：登录令牌已过期。上面这些是客户端上次联网时的缓存（可能已经很旧），\
             别当实时数用——打开一次该产品让它自动续期，或重新登录后再读。"
                .to_string()
        } else {
            format!(
                "读不到服务端实时余额（{}）。上面这些是客户端上次联网时的缓存，可能已经过期。",
                short_err(&self.live_error)
            )
        }
    }
}

/// 把 agenthub 带回来的多行报错压成一行短句，用于塞进界面提示
fn short_err(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(140).collect()
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_i64().map(|x| x as f64))
}

/// 这次按账号读会不会落到"本机当前登录"而不是实例目录。
/// Qoder 系的登录态存在产品自己的档案里（新壳 `%APPDATA%\com.<bundle>.app.stable\auth.v1.dat`
/// 或旧壳 `%APPDATA%\<产品>\User\globalStorage\state.vscdb`），junction 换号只搬账号目录，
/// 实例目录里压根没有它自己的登录数据——按目录读只会报"没登录过"。
/// AStudio（明文 astron-session.json）与 MiniMax（mcode OAuth 令牌）同理：登录是整机一份，
/// 账号目录里没有时也回落到默认档案读，写不写得进这张卡仍由 userId 绑定把关。
pub fn falls_back_to_default(store: &str, data_dir: &str) -> bool {
    // Trae 登录态在 %APPDATA% 下的产品 userData（2026-10-11 升级后是 Trae CN，
    // 旧安装是 TRAE SOLO CN），实例目录里的 storage.json 是 launch 多开时
    // 留下的旧快照，不能当实时登录态读——始终回落读 APPDATA 那份正在用的。
    if store == "trae-cn" {
        return true;
    }
    // WorkBuddy（含国际版）登录态在 %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\*.info，
    // 是整机一份、不在实例目录——始终回落，由 agenthub 读固定位置。
    if store == "workbuddy" || store == "workbuddy-intl" {
        return true;
    }
    // 2026-10-11 新登记四家，登录态全在**整机一份的固定位置**（照 WorkBuddy 先例始终回落，
    // 由 agenthub 端按产品名读固定位置；按账号实例目录读只会误报"这个账号没登录过"）：
    //   qwenwork     {APPDATA}\QwenWorkCN\auth-v2.dat ——产品没有按实例隔离的登录数据，
    //                实例目录里永远不会有它；
    //   dsh          ~/.dsh\.credentials.yaml ——整机就这一份明文凭据，
    //                实例目录（哪怕是原地账号）里的也只是产品自己的运行时数据；
    //   marvis       .marvis 本身是 symlink→%APPDATA%\Tencent\Marvis\User\<openid>，
    //                openid 编在链接目标里、由产品自己重指——它不是"账号各一份"的布局，
    //                按账号目录挂链接这条路不成立，只能整机一份地读；
    //   xiaomi-mimo  {APPDATA}\Xiaomi MiMo\Partitions\xiaomi-account\Network\Cookies，
    //                Electron 默认档案位置，整机一份。
    // 四家能不能把读数写进某张卡，仍由 commands.rs 的 userId 绑定守卫把关。
    if matches!(store, "qwenwork" | "dsh" | "marvis" | "xiaomi-mimo") {
        return true;
    }
    // 2026-10-18 两家同理（登录态整机一份：antigravity 的 state.vscdb 在
    // {APPDATA}\Antigravity\User\globalStorage、coze 的分区 localStorage 在
    // {APPDATA}\Coze\Partitions——实例目录里都不会有，按目录读只会误报"没登录过"）。
    // 换号后 junction 把 {APPDATA}\<产品> 指向当前账号的实例目录，agenthub 按
    // default_profile 给的固定路径读，拿到的就是当前挂着的那个号；能不能写进某张卡
    // 仍由 commands.rs 的 userId 绑定守卫把关。
    if matches!(store, "antigravity" | "coze") {
        return true;
    }
    (store.starts_with("qoder") || store == "astudio" || store == "minimax")
        && !crate::registry::has_login_data(Path::new(data_dir))
}

/// 回落时该读哪个目录：交给 registry 按"哪个真有登录数据"挑（Qoder 0.4.x 换壳后，
/// 登录档案有两个可能位置）。早先这里自己拼 `%APPDATA%\<产品>`，于是读到的是一份 8 月的旧账。
/// Trae 例外：登录态在 %APPDATA% 下的产品 userData（新版 Trae CN 优先、老安装
/// TRAE SOLO CN 回落），实例目录里的 storage.json 是 launch 多开时留下的旧快照、
/// 不是实时登录态——直接指 APPDATA，别去读实例目录那份旧 token。
/// WorkBuddy 例外：登录态在 %LOCALAPPDATA% 固定位置，agenthub 端忽略 --profile，直接返回 None 即可。
fn default_profile(store: &str) -> Option<String> {
    if store == "trae-cn" {
        if let Ok(appdata) = std::env::var("APPDATA") {
            // 新目录优先：旧目录只是迁移残留，新客户端登录后它永远不再更新，
            // 读它拿到的是冻结的登录快照——恰好是"串号"最隐蔽的来源。
            for name in ["Trae CN", "TRAE SOLO CN"] {
                let p = std::path::Path::new(&appdata).join(name);
                if p.exists() {
                    return Some(p.to_string_lossy().into_owned());
                }
            }
        }
        return None;
    }
    if store == "workbuddy" || store == "workbuddy-intl" {
        return None;
    }
    // 新四家（qwenwork / dsh / marvis / xiaomi-mimo）走默认分支就对了：
    // 它们的 app id 与 agenthub 产品名同名，而整机一份的登录态恰好就落在各自的
    // profile 目录里（auth-v2.dat 在 {APPDATA}\QwenWorkCN、.credentials.yaml 在 ~/.dsh、
    // mobile_address.json 在 .marvis 链接目标、Cookies 在 {APPDATA}\Xiaomi MiMo\Partitions），
    // profile_dir(store) 展开候选后返回的就是那个固定位置——把 --profile 交给 agenthub，
    // 读的还是整机一份，但落点由 registry 这一侧说了算，candidates 换了壳也不用两头改。
    // 2026-10-18 的 antigravity / coze 同理：profile_dir 给的是 {APPDATA}\Antigravity 与
    // {APPDATA}\Coze，agenthub 端各自往下找 User\globalStorage\state.vscdb /
    // Partitions\*\Local Storage\leveldb（换号后这条路径跟着 junction 走，读到的是当前账号）。
    crate::registry::profile_dir(store).map(|p| p.to_string_lossy().into_owned())
}

/// 按账号读余额：实例目录里有登录数据就读它，否则读本机当前登录。
/// 调用方仍要用 userId 校验决定这份数能不能写进这张卡（见 commands.rs 的 mismatch 守卫）。
pub fn fetch_for_account(store: &str, data_dir: &str) -> Result<Quota, String> {
    if !falls_back_to_default(store, data_dir) {
        return fetch(store, Some(data_dir));
    }
    match default_profile(store) {
        Some(p) => fetch(store, Some(p.as_str())),
        None => fetch(store, None),
    }
}

/// 跑 `agenthub quota --json <store>` 并把结果结构化。
/// agenthub 是 .cmd 包装，CreateProcess 直接执行不了，必须经 cmd.exe 转一手。
/// profile = 该账号自己的档案目录（按账号分别读的关键），None 则读产品当前默认档案。
pub fn fetch(store: &str, profile: Option<&str>) -> Result<Quota, String> {
    let mut c = Command::new("cmd.exe");
    // profile 给了就读那个账号自己的档案目录——单实例产品靠 junction 换号，
    // 每个账号的数据就在它自己的 data_dir 里，这样才能一账号一读数。
    c.args(["/d", "/c", "agenthub", "quota", "--json", store]);
    if let Some(dir) = profile {
        if !dir.is_empty() {
            c.arg("--profile").arg(dir);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let out = c
        .output()
        .map_err(|e| format!("调不到 agenthub（{e}）：本机需要先装 agent-memory-suite"))?;
    if !out.status.success() {
        let s = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "agenthub quota 失败：{}",
            s.trim().chars().take(180).collect::<String>()
        ));
    }
    let parsed: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("agenthub 输出不是 JSON：{e}"))?;
    let d = parsed.get(0).ok_or_else(|| "agenthub 没返回任何条目".to_string())?;
    if let Some(e) = d.get("error").and_then(|v| v.as_str()) {
        return Err(format!("读不到余额：{e}"));
    }
    let expires = d["expires"].as_str().unwrap_or("").to_string();
    let unit = "credits";
    let buckets = d["buckets"].as_array().cloned().unwrap_or_default();
    let portions = buckets
        .iter()
        .map(|b| {
            // 逐笔到期日：资源包各自带 expires，没有才退回顶层的汇总日期
            let b_exp = b["expires"].as_str().unwrap_or("").to_string();
            json!({
                "label": b["pool"].as_str().unwrap_or("积分"),
                "left": num(&b["remaining"]),
                "total": num(&b["total"]),
                "used": num(&b["used"]),
                "expires_on": if b_exp.is_empty() { expires.clone() } else { b_exp },
                "category": b["category"].as_str().unwrap_or(""),
                "unit": b["unit"].as_str().unwrap_or(unit),
                // 厂商给的理由/口径原样带上去：界面靠它说明"这一档只能花在什么上"
                "note": b["note"].as_str().unwrap_or(""),
                "scope": b["scope"].as_str().unwrap_or(""),
            })
        })
        .collect();
    // 大组 + 逐笔明细：agenthub 已经算好合计与"有效期内/已过期"的笔数，这里原样透传。
    // 老版本 agenthub 没这个键时留空，界面退回按 buckets 画（不许自己编一组）。
    let groups = d["groups"].as_array().cloned().unwrap_or_default();
    Ok(Quota {
        user_id: d["userId"].as_str().unwrap_or("").to_string(),
        account: d["account"].as_str().unwrap_or("").to_string(),
        user_type: d["userType"].as_str().unwrap_or("").to_string(),
        left: num(&d["left"]),
        total: num(&d["total"]),
        exceeded: d["exceeded"].as_bool().unwrap_or(false),
        expires_on: expires,
        portions,
        groups,
        plan_name: d["plan"].as_str().unwrap_or("").to_string(),
        // 单位取第一笔实际给的：逐桶单位不一致的产品目前没见到，取第一笔即够用
        unit: buckets
            .first()
            .and_then(|b| b["unit"].as_str())
            .unwrap_or("credits")
            .to_string(),
        kind: d["kind"].as_str().unwrap_or("full").to_string(),
        used: num(&d["used_credits"]).unwrap_or(0.0),
        sessions: d["sessions"].as_i64().unwrap_or(0),
        last_active: d["last_active"].as_str().unwrap_or("").to_string(),
        source: d["source"].as_str().unwrap_or("server").to_string(),
        live_error: d["liveError"].as_str().unwrap_or("").to_string(),
        auth_from: d["authFrom"].as_str().unwrap_or("").to_string(),
        // 按天序列与它的"为什么没有"合并成一个值：界面只认一种形状，
        // 有 days 就画图、只有 error 就把那句话原样显示出来，不许自己编一组数。
        trend: match d.get("trend") {
            Some(t) if !t.is_null() => t.clone(),
            _ => match d["trendError"].as_str() {
                Some(e) if !e.is_empty() => json!({ "error": e }),
                _ => Value::Null,
            },
        },
    })
}

/// 把读回来的明细写进档案（含"明细不许被粗读数盖回去"那道守卫）用的公共小工具
fn arr(m: &Value, k: &str) -> Vec<Value> {
    m.get(k).and_then(|v| v.as_array()).cloned().unwrap_or_default()
}

/// 厂商"这一笔没给到期日"的三态写法（与 agenthub、界面 `EXP_NONE` 同一套）：
/// `未给出` = 接口根本没给，`无限期` = 9999 年哨兵，两者都不算"真日期"。
fn is_real_date(s: &str) -> bool {
    !s.is_empty() && s != "未给出" && s != "无限期" && s != "—"
}

/// 一份明细"细"到什么程度：(逐笔条数, 其中真给了到期日的条数)。
///
/// 判据必须放在细度这一层，不能放在"哪个产品/哪个来源"那一层：同一条本机接口
/// 对 WorkBuddy 能摊出 65 笔逐包，对 Qoder 只有「主额度 + 附加包」两个汇总数。
/// 让粗的那份盖掉细的那份，界面就会出现"21 个资源包蜷成一行、到期时间未给出"，
/// 而官网那份逐包读数错过就再也补不回来（本机接口根本不含这些字段）。
pub fn detail_rank(groups: &[Value], portions: &[Value]) -> (usize, usize) {
    let mut n = 0usize;
    let mut dated = 0usize;
    if !groups.is_empty() {
        for g in groups {
            let its = arr(g, "items");
            if its.is_empty() {
                n += 1;
                if is_real_date(g["expires"].as_str().unwrap_or("")) {
                    dated += 1;
                }
                continue;
            }
            for it in &its {
                n += 1;
                if is_real_date(it["expires_on"].as_str().unwrap_or("")) {
                    dated += 1;
                }
            }
        }
    } else {
        for p in portions {
            n += 1;
            if is_real_date(p["expires_on"].as_str().unwrap_or("")) {
                dated += 1;
            }
        }
    }
    (n, dated)
}

/// 档案里现在存着的那份明细有多细
pub fn stored_rank(m: &Value) -> (usize, usize) {
    detail_rank(&arr(m, "quota_groups"), &arr(m, "credits"))
}

/// 更细的那份留在档案里，同时把"汇总数比明细新"这件事标出来，交给界面如实说。
fn mark_detail_fresh(obj: &mut serde_json::Map<String, Value>, source: &str) {
    obj.insert("quota_detail_source".into(), json!(source));
    obj.insert("quota_detail_stale".into(), json!(false));
    obj.insert(
        "quota_detail_at".into(),
        json!(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)),
    );
}

/// 把读到的数写进实例档案；第一次读时把 userId 绑到这个账号上，之后每次校验。
pub fn apply(meta: &Value, q: &Quota) -> Value {
    let mut m = meta.clone();
    let obj = match m.as_object_mut() {
        Some(o) => o,
        None => return m,
    };
    // 只能读"已用"的产品：别去碰用户手填的剩余额度与到期日，只记累计消耗。
    // 数是从这个账号自己的档案目录里读的（agenthub 会按 --profile 定位 workbuddy.db），
    // 但它不带 userId，所以没得绑定。
    if q.is_usage_only() {
        obj.insert("quota_used_local".into(), json!(q.used));
        obj.insert("quota_sessions".into(), json!(q.sessions));
        obj.insert("quota_last_active".into(), json!(q.last_active));
        obj.insert("quota_local".into(), json!(true));
        if !q.trend.is_null() {
            obj.insert("quota_trend".into(), q.trend.clone());
        }
        obj.insert(
            "quota_fetched_at".into(),
            json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)),
        );
        return m;
    }
    obj.insert("credits_left".into(), json!(q.left));
    obj.insert("credits_total".into(), json!(q.total));
    // 明细只许越读越细。这一次读数比档案里存的粗就不动它——典型场景：Qoder 本机那条接口
    // 只有「主额度 + 附加包」两个汇总数，而档案里躺着官网读回来的 21 笔逐包各带到期日。
    // 后台每半小时一轮自动取数，没有这道守卫的话，读一次官网存进去的明细下一轮就没了
    // （她看到的"积分明细又全都蜷到一团了"就是这么来的）。
    let keep_detail = stored_rank(meta) > detail_rank(&q.groups, &q.portions);
    if keep_detail {
        obj.insert("quota_detail_stale".into(), json!(true));
    } else {
        obj.insert("credits".into(), json!(q.portions));
        // 大组与逐笔明细整包存下来：界面那层「积分明细 → 查看明细」直接画它，
        // 不再要求用户自己一笔笔登记（她原话：那么多资源包我自己填填到什么时候去）
        obj.insert("quota_groups".into(), json!(q.groups));
        mark_detail_fresh(obj, &q.source);
    }
    if !keep_detail || !q.plan_name.is_empty() {
        obj.insert("quota_plan_name".into(), json!(q.plan_name));
    }
    obj.insert("quota_user_id".into(), json!(q.user_id));
    obj.insert("quota_account".into(), json!(q.account));
    obj.insert("quota_plan".into(), json!(q.user_type));
    obj.insert("quota_local".into(), json!(true));
    // 数据来源如实记下：服务端实时，还是"客户端上次联网时的缓存"。
    // 令牌过期时 agenthub 读不到服务端，只剩旧缓存——不记下来，界面就会把 8 月的数当今天的。
    obj.insert("quota_source".into(), json!(q.source));
    obj.insert("quota_live_error".into(), json!(q.live_error));
    if !q.auth_from.is_empty() {
        obj.insert("quota_auth_from".into(), json!(q.auth_from));
    }
    // 按天消耗序列：这一轮没读到就留着上一轮的（它是本机 sqlite 聚合，不联网，
    // 不会因为服务端掉线就没数），只有 agenthub 真给了新序列才覆盖。
    if !q.trend.is_null() {
        obj.insert("quota_trend".into(), q.trend.clone());
    }
    if !q.expires_on.is_empty() {
        obj.insert("expires_on".into(), json!(q.expires_on));
    }
    obj.insert(
        "quota_fetched_at".into(),
        json!(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只认账号、没有额度数字的店（MiMo/Antigravity/Coze）必须被认出来：
    /// 不认出来的话，界面会把"剩 未给出 / 未给出"当成没读到，更糟的复古做法是填 0
    #[test]
    fn account_only_is_detected_without_any_numbers() {
        let q = Quota {
            user_id: "u".into(), account: "昵称".into(), user_type: "Google 账号".into(),
            left: None, total: None, expires_on: String::new(), exceeded: false,
            portions: vec![], groups: vec![], plan_name: String::new(), unit: String::new(),
            kind: "full".into(), used: 0.0, sessions: 0, last_active: String::new(),
            source: "local-cache".into(),
            live_error: "没有余额接口".into(), auth_from: String::new(),
            trend: Value::Null,
        };
        assert!(q.is_account_only());
        // live_error 是"厂商没有接口"的解释，不是"服务端没读成"的警告——不算 stale
        assert!(!q.is_stale());
        // 有任何一个数字就不算 account_only
        let mut with_left = q.clone();
        with_left.left = Some(0.0);
        assert!(!with_left.is_account_only());
        // usage_only 走另一条道（累计已用），别混
        let mut usage = q.clone();
        usage.kind = "usage_only".into();
        usage.used = 12.0;
        assert!(!usage.is_account_only());
    }

    #[test]
    fn apply_writes_binding_and_portions() {        let meta = json!({ "name": "账号1", "app": "qoder-cn" });
        let q = Quota {
            user_id: "u-1".into(), account: "nick1".into(), user_type: "personal_professional".into(),
            left: Some(3100.0), total: Some(3200.0), expires_on: "2026-10-20".into(), exceeded: false,
            portions: vec![json!({ "label": "主额度", "left": 2000, "total": 2000 })],
            groups: vec![json!({"name": "套餐基础积分", "left": 2000.0, "total": 2000.0,
                                "items": [{"label": "主额度", "left": 2000.0, "total": 2000.0,
                                           "expires_on": "2026-10-20 23:59"}]})],
            plan_name: "体验版".into(), unit: "credits".into(),
            kind: "full".into(), used: 100.0, sessions: 3, last_active: "2026-10-07 18:00".into(),
            source: "server".into(), live_error: String::new(), auth_from: String::new(),
            trend: Value::Null,
        };
        let m = apply(&meta, &q);
        assert_eq!(m["quota_user_id"], "u-1");
        assert_eq!(m["quota_account"], "nick1");
        assert_eq!(m["quota_plan"], "personal_professional");
        assert_eq!(m["credits_left"], 3100.0);
        assert_eq!(m["expires_on"], "2026-10-20");
        assert_eq!(m["quota_source"], "server");
        assert_eq!(m["quota_live_error"], "");
        assert_eq!(m["credits"].as_array().unwrap().len(), 1);
        // 分组与逐笔明细必须原样落盘，界面上那层「查看明细」就是读它
        assert_eq!(m["quota_groups"][0]["name"], "套餐基础积分");
        assert_eq!(m["quota_groups"][0]["items"][0]["expires_on"], "2026-10-20 23:59");
        assert_eq!(m["quota_plan_name"], "体验版");
        // 原有字段不能被抹掉
        assert_eq!(m["name"], "账号1");
    }

    /// 官网读回来的 21 笔逐包，绝不能被本机那两个汇总数盖掉——后台每半小时一轮自动取数，
    /// 没有这道守卫的话读一次官网存进去的明细下一轮就没了。
    /// 判据是"这份读数更粗"，不是"这是 Qoder"：WorkBuddy 本机读数本身就有 65 笔，照常覆盖。
    #[test]
    fn coarser_read_keeps_the_finer_detail() {
        let rich = json!({
            "quota_detail_source": "web", "quota_plan_name": "专业版", "credits": [],
            "quota_groups": [
              {"name": "平台奖励积分", "left": 2000.0, "total": 2100.0,
               "items": (0..21).map(|i| json!({ "label": "平台赠送", "left": 100.0, "total": 100.0,
                        "expires_on": format!("2026-11-{:02} 21:00", (i % 28) + 1) })).collect::<Vec<_>>()},
              {"name": "套餐基础积分", "left": 2000.0, "total": 2000.0, "items": [
                        {"label": "套餐额度", "left": 2000.0, "total": 2000.0,
                         "expires_on": "2026-10-20 19:28"}]}
            ]});
        // mk_quota 那份只有汇总数：groups / portions 都空
        let m = apply(&rich, &mk_quota("server", ""));
        assert_eq!(
            m["quota_groups"][0]["items"].as_array().unwrap().len(),
            21,
            "21 笔逐包必须还在"
        );
        assert_eq!(m["quota_detail_stale"], true, "明细比汇总数旧，得标出来给界面看");
        assert_eq!(m["quota_detail_source"], "web", "明细的来源不许被改写成 server");
        assert_eq!(m["quota_plan_name"], "专业版", "汇总读数没有套餐名，不许把已有的抹成空");
        // 汇总数照常跟最新那次读数走
        assert_eq!(m["credits_left"], 500.0);
        assert_eq!(m["quota_source"], "server");
    }

    /// 反过来：本机读数本身就有逐笔明细（WorkBuddy 那种）时必须照常写入，
    /// 守卫不能变成"第一次读到什么就永远冻住什么"
    #[test]
    fn finer_read_still_replaces_the_detail() {
        let stored = json!({"quota_groups": [
            {"name": "平台奖励积分", "items": [
                {"label": "附加包", "left": 1751.0, "total": 2400.0, "expires_on": "未给出"}]}
        ]});
        let mut q = mk_quota("server", "");
        q.groups = vec![json!({"name": "平台奖励积分", "items": [
            {"label": "赠送包 1", "left": 100.0, "total": 100.0, "expires_on": "2026-11-08 21:09"},
            {"label": "赠送包 2", "left": 51.0, "total": 100.0, "expires_on": "2026-10-24 21:13"}]})];
        let m = apply(&stored, &q);
        assert_eq!(
            m["quota_groups"][0]["items"].as_array().unwrap().len(),
            2,
            "更细的读数该写进去"
        );
        assert_eq!(m["quota_detail_stale"], false);
        assert_eq!(m["quota_detail_source"], "server");
    }

    #[test]
    fn only_confirmed_products_are_locally_readable() {
        assert_eq!(store_for("qoder-cn"), Some("qoder-cn"));
        assert_eq!(store_for("workbuddy"), Some("workbuddy"));
        assert_eq!(store_for("workbuddy-intl"), Some("workbuddy-intl"));
        assert_eq!(store_for("astudio"), Some("astudio"));
        assert_eq!(store_for("minimax"), Some("minimax"));
        assert_eq!(store_for("traework-cn"), Some("trae-cn"));
        // ZCode：凭据 enc:v1 是本地可派的 AES-256-GCM，登录态在实例目录里，已确证能直读
        assert_eq!(store_for("zcode"), Some("zcode"));
        // 2026-10-11 新登记四家（接口与落点都已实测）：都有直读通道
        assert_eq!(store_for("qwenwork"), Some("qwenwork"));
        assert_eq!(store_for("dsh"), Some("dsh"));
        assert_eq!(store_for("marvis"), Some("marvis"));
        assert_eq!(store_for("xiaomi-mimo"), Some("xiaomi-mimo"));
        // OpenCode 故意不接：本机从未登录过官方账号（opencode.db 的 account 三表 0 行），
        // 产品自身也没有余额接口——登记了档案目录，但没有直读通道。
        assert_eq!(store_for("opencode"), None);
    }

    /// 新四家的登录态都在整机一份的固定位置：不管实例目录里有什么，按账号读必须回落，
    /// 否则只会误报"这个账号没登录过"（判据照 WorkBuddy/Trae 那条先例）。
    /// 反过来也不能把 qoder 那套"目录里有登录数据就不回落"的规则误扩到它们身上。
    #[test]
    fn new_stores_always_fall_back_to_machine_login() {
        let tmp = std::env::temp_dir().join("cockpit-new-store-fallback-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let d = tmp.to_str().unwrap();
        for store in ["qwenwork", "dsh", "marvis", "xiaomi-mimo"] {
            assert!(falls_back_to_default(store, d), "{store} 空实例目录该回落");
            assert!(
                falls_back_to_default(store, "C:\\definitely\\not\\a\\real\\dir"),
                "{store} 回落不该取决于目录内容（整机一份，固定位置）"
            );
        }
        // 目录里就算躺着一份登录标志，四家也照样回落（读的是固定位置，不是账号目录）
        std::fs::write(tmp.join("auth-v2.dat"), b"v10").unwrap();
        for store in ["qwenwork", "dsh", "marvis", "xiaomi-mimo"] {
            assert!(falls_back_to_default(store, d), "{store} 该始终回落");
        }
        // 但 auth-v2.dat 得被 has_login_data 认出来（resolve_profile 挑档案目录靠它）
        assert!(crate::registry::has_login_data(&tmp), "auth-v2.dat 该算登录标志");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 提示文案的产品清单从 LOCAL_STORES 现拼（写死名字表是第二份真值，会过期——
    /// 旧文案停在"只有 Qoder CN / … / ZCode"，新登记的四家就没了）。
    #[test]
    fn locally_readable_names_is_generated_not_hardcoded() {
        let s = locally_readable_names();
        for name in [
            "Qoder CN", "WorkBuddy", "千问办公", "DeepSeek Harness", "Marvis", "Xiaomi MiMo",
            // 2026-10-18 两家：只读账号的也登记在册，提示文案不能再把它们漏成"没通道"
            "Antigravity", "Coze",
        ] {
            assert!(s.contains(name), "清单里该有 {name}：{s}");
        }
        // OpenCode 没接直读，不该出现在清单里
        assert!(!s.contains("OpenCode"), "没登记通道的不该进清单：{s}");
    }

    /// workbuddy 只能读"已用"，绝不能覆盖用户手填的剩余额度与到期日
    #[test]
    fn usage_only_keeps_manual_credits_intact() {
        let meta = json!({
            "name": "主号", "app": "workbuddy",
            "credits_left": 2000.0, "credits_total": 5000.0, "expires_on": "2026-12-31"
        });
        let q = Quota {
            user_id: String::new(), account: "本机累计（不分账号）".into(),
            user_type: "usage_only".into(), left: None, total: None, expires_on: String::new(),
            exceeded: false, portions: vec![], groups: vec![], plan_name: String::new(), unit: "credits".into(),
            kind: "usage_only".into(), used: 9395.07, sessions: 126,
            last_active: "2026-10-07 19:12".into(),
            source: String::new(), live_error: String::new(), auth_from: String::new(),
            trend: Value::Null,
        };
        assert!(q.is_usage_only());
        assert!(!q.is_stale(), "只有累计已用的产品不算\"旧缓存\"");
        let m = apply(&meta, &q);
        assert_eq!(m["quota_used_local"], 9395.07);
        assert_eq!(m["quota_sessions"], 126);
        // 手填的三个字段必须原封不动
        assert_eq!(m["credits_left"], 2000.0);
        assert_eq!(m["credits_total"], 5000.0);
        assert_eq!(m["expires_on"], "2026-12-31");
        // 没有 userId，不做绑定
        assert!(m.get("quota_user_id").is_none());
    }

    /// Qoder 的登录态存在 %APPDATA%\<产品>，不在实例目录：空目录必须判为"回落读本机当前登录"，
    /// 否则按账号读永远只会报"没登录过"，卡片上的余额再也刷不动
    #[test]
    fn qoder_falls_back_but_only_when_dir_has_no_login() {
        let tmp = std::env::temp_dir().join("cockpit-fallback-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let d = tmp.to_str().unwrap();
        assert!(falls_back_to_default("qoder-cn", d), "空目录该回落");
        assert!(falls_back_to_default("qoder", d));
        // WorkBuddy 登录态在 %LOCALAPPDATA% 固定位置，始终回落读固定位置
        assert!(falls_back_to_default("workbuddy", d), "WorkBuddy 登录态不在实例目录，该回落");
        assert!(falls_back_to_default("workbuddy-intl", d));
        let gs = tmp.join("User").join("globalStorage");
        std::fs::create_dir_all(&gs).unwrap();
        std::fs::write(tmp.join("Local State"), "{}").unwrap();
        std::fs::write(gs.join("state.vscdb"), "").unwrap();
        assert!(!falls_back_to_default("qoder-cn", d), "目录里有登录数据就该读它自己");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 真机才跑：按账号目录读时，空目录（该账号没登录过）必须如实报错，
    /// 绝不能拿本机当前登录的号的数据顶上——这正是"两个账号显示同一个数"的根因。
    #[test]
    #[ignore]
    fn empty_profile_dir_is_reported_not_guessed() {
        let tmp = std::env::temp_dir().join("cockpit-empty-profile-test");
        std::fs::create_dir_all(&tmp).unwrap();
        let r = fetch("qoder-cn", Some(tmp.to_str().unwrap()));
        assert!(r.is_err(), "空档案目录不该读到任何数值");
        let e = r.unwrap_err();
        assert!(
            e.contains("登录数据") || e.contains("读不到"),
            "错误信息要说清原因，实际：{e}"
        );
    }

    /// 真机才跑：需要本机装着 Qoder 且已登录。cargo test -- --include-ignored
    #[test]
    #[ignore]
    fn fetch_parses_real_agenthub_output() {
        let q = fetch("qoder-cn", None).expect("本机应该能读到 Qoder CN 的积分");
        assert!(!q.user_id.is_empty(), "userId 不该为空");
        assert!(!q.account.is_empty(), "账号名不该为空");
        assert!(q.total.unwrap_or(0.0) > 0.0, "总额度该大于 0");
        assert!(q.left <= q.total);
        assert!(!q.portions.is_empty(), "至少有一笔积分");
        println!("读到 {}：{:?}/{:?}（来源 {}）", q.account, q.left, q.total, q.source);
    }

    /// 真机才跑：Qoder 国际版 0.4.x 换了壳，登录档案在
    /// `%APPDATA%\com.<bundle>.app.stable\auth.v1.dat`。只认旧的
    /// `%APPDATA%\Qoder\User\globalStorage\state.vscdb` 会读到 8 月的旧账——
    /// 那儿写着「剩 500/500」，令牌其实 2026-09-08 就过期了。
    #[test]
    #[ignore]
    fn qoder_does_not_read_the_stale_legacy_db() {
        let q = match fetch("qoder", None) {
            Ok(q) => q,
            Err(e) => {
                println!("本机读不到 Qoder 国际版，跳过：{e}");
                return;
            }
        };
        println!(
            "读到 {}：{:?}/{:?} {:?} 来源 {} 登录档案 {}",
            q.account, q.left, q.total, q.unit(), q.source, q.auth_from
        );
        assert!(
            !q.auth_from.contains("\\Qoder\\User\\globalStorage"),
            "还在认旧的 VS Code 壳档案（读到的会是 8 月的旧账）：{}",
            q.auth_from
        );
    }

    fn mk_quota(source: &str, live_error: &str) -> Quota {        Quota {
            user_id: "u".into(), account: "a".into(), user_type: "personal_standard".into(),
            left: Some(500.0), total: Some(500.0), expires_on: String::new(), exceeded: false,
            portions: vec![], groups: vec![], plan_name: String::new(), unit: "credits".into(), kind: "full".into(), used: 0.0, sessions: 0,
            last_active: String::new(),
            source: source.into(), live_error: live_error.into(), auth_from: String::new(),
            trend: Value::Null,
        }
    }

    /// 按天序列：这一轮 agenthub 没给数时**不许**把上一轮的抹掉（它是本机 sqlite 聚合，
    /// 不随服务端掉线），给了就整包覆盖；只有 error 时把 error 存下来，
    /// 界面才说得出"为什么这张卡没有图"。
    #[test]
    fn trend_survives_a_round_without_data() {
        let old = json!({ "name": "账号1", "quota_trend": {
            "unit": "credits", "total": 10.0, "days": [{"date": "2026-10-01", "credits": 10.0}] } });
        let mut q = mk_quota("server", "");
        assert!(apply(&old, &q).get("quota_trend").is_some(), "没带新序列就要留着旧的");
        q.trend = json!({ "error": "没找到 workbuddy.db" });
        assert_eq!(apply(&old, &q)["quota_trend"]["error"], "没找到 workbuddy.db");
        q.trend = json!({ "unit": "credits", "days": [{"date": "2026-10-11", "credits": 3.0}] });
        assert_eq!(apply(&old, &q)["quota_trend"]["days"][0]["date"], "2026-10-11");
    }

    /// 「服务端没读成、只剩旧缓存」必须被识别出来，否则界面会把 8 月的数当今天的余额显示
    #[test]
    fn stale_cache_is_flagged_not_silently_trusted() {
        let q = mk_quota(
            "local-cache",
            "HTTP 401 {\"code\": \"TOKEN_EXPIRE\", \"message\": \"token is not active\"}",
        );
        assert!(q.is_stale());
        assert!(q.token_expired());
        assert!(q.stale_hint().contains("令牌已过期"), "要说清是令牌过期：{}", q.stale_hint());
        let m = apply(&json!({ "name": "Qoder" }), &q);
        assert_eq!(m["quota_source"], "local-cache");
        assert!(m["quota_live_error"].as_str().unwrap().contains("TOKEN_EXPIRE"));
    }

    /// 服务端读成功（source=server）绝不能被误判成旧缓存
    #[test]
    fn server_source_is_never_stale() {
        let q = mk_quota("server", "");
        assert!(!q.is_stale());
        assert!(q.stale_hint().is_empty());
        // source 字段缺失时按"实时"处理，老版本 agenthub 不会因此全被标成旧缓存
        let q2 = mk_quota("", "");
        assert!(!q2.is_stale());
    }

    /// 非令牌类的服务端报错也要提示，但话术不能张冠李戴
    #[test]
    fn other_server_error_has_its_own_wording() {
        let q = mk_quota("local-cache", "HTTP 500 {\"message\": \"boom\"}");
        assert!(q.is_stale());
        assert!(!q.token_expired());
        let hint = q.stale_hint();
        assert!(hint.contains("500"), "要把原因带出来：{hint}");
        assert!(!hint.contains("令牌已过期"));
    }
}
