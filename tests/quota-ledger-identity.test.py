"""台账身份与过期留档回归（离线，不读不写真实 quota-ledger.json）。

2026-11-17 审阅用隔离数据复现了三个缺陷：
  ① 同一包已用 0→20 再过期 → 被拆成两笔历史；
  ② 两个不同 ID、同额同到期同用量 → 被合成一笔；
  ③ 整组全部过期 → 汇总仍保留余额。
本文件钉住修复后的行为，并覆盖旧键迁移、活笔转过期、改名不造新条目。
运行：python tests/quota-ledger-identity.test.py
"""
import ast
import datetime as dt
import sys
import time
from pathlib import Path

SRC = Path(__file__).resolve().parents[1] / "src-tauri" / "vendor" / "agenthub.py"
NAMES = {"_trae_shape", "doubao_quota_overview", "_doubao_num", "_f", "_exp_is_past", "_pack_sig", "_pack_ident", "_quota_ledger_migrate",
         "quota_ledger_merge", "_ledger_item", "credit_groups"}
ns = {"dt": dt, "_QUOTA_LEDGER_SEP": "\x1f", "EXPIRY_UNKNOWN": "未给出",
      "QUOTA_LEDGER_VERSION": 2,
      "GROUP_ORDER": ["套餐基础积分", "平台奖励积分", "购买积分", "其他"],
      "TRAE_GROUP_ALIAS": {"签到奖励": "每日签到", "原速通次数兑换": "原速通次数兑换"}}
tree = ast.parse(SRC.read_text(encoding="utf-8-sig"))
for node in tree.body:
    if isinstance(node, ast.FunctionDef) and node.name in NAMES:
        exec(compile(ast.Module(body=[node], type_ignores=[]), str(SRC), "exec"), ns)

merge = ns["quota_ledger_merge"]
credit_groups = ns["credit_groups"]
FAILS = []


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail and not cond else ""))
    if not cond:
        FAILS.append(name)


def item(used=0, rid=None, exp="2026-12-06 12:00", total=100, label="积分包"):
    it = dict(label=label, total=total, used=used, left=total - used,
              expires_on=exp, unit="积分")
    if rid:
        it["resource_id"] = rid
    return it


def group(items, name="平台奖励积分"):
    return [dict(name=name, total=100, left=100, used=0, items=items)]


# ① 同一包消费变化再过期：只有一笔历史，已用是最近读数
led = {"packs": {}}
merge("doubao", "t1", group([item(0, rid="A")]), dt.datetime(2026, 11, 17), led)
merge("doubao", "t1", group([item(20, rid="A")]), dt.datetime(2026, 11, 18), led)
after = merge("doubao", "t1", [], dt.datetime(2026, 12, 7), led)
restored = [i for g in after for i in g["items"]]
check("same_pack_used_changes_one_record", len(led["packs"]) == 1, str(len(led["packs"])))
check("same_pack_used_changes_one_restored", len(restored) == 1, str(len(restored)))
check("same_pack_latest_used_wins", restored and restored[0]["used"] == 20, str(restored))

# ①-b 没有资源 ID 的产品：消费变化也不许拆笔（签名不含 used）
led = {"packs": {}}
merge("doubao", "t1b", group([item(0)]), dt.datetime(2026, 11, 17), led)
merge("doubao", "t1b", group([item(20)]), dt.datetime(2026, 11, 18), led)
after = merge("doubao", "t1b", [], dt.datetime(2026, 12, 7), led)
check("no_rid_used_changes_one_record", len(led["packs"]) == 1, str(len(led["packs"])))
check("no_rid_used_changes_one_restored",
      sum(len(g["items"]) for g in after) == 1)

# ② 两个不同 ID 同额同到期同用量：两笔都要在
led = {"packs": {}}
merge("doubao", "t2", group([item(0, rid="A"), item(0, rid="B")]),
      dt.datetime(2026, 11, 17), led)
after = merge("doubao", "t2", [], dt.datetime(2026, 12, 7), led)
check("twins_two_records", len(led["packs"]) == 2, str(len(led["packs"])))
check("twins_two_restored", sum(len(g["items"]) for g in after) == 2)

# ③ 整组全部过期：可用余额归零
bucket = [{"category": "平台奖励积分", "pool": "平台奖励积分",
           "total": 100, "used": 20, "remaining": 80, "items": [item(20)]}]
gs = credit_groups(bucket, dt.datetime(2026, 12, 7))
check("all_expired_left_zero", gs[0]["left"] == 0, str(gs[0]["left"]))
check("all_expired_live_count_zero", gs[0]["live_count"] == 0)

# ④ 活笔（带 ID）过期后转进不带 ID 的过期接口：认回同一笔，不造重影
led = {"packs": {}}
merge("trae-cn", "t4", group([item(0, rid="ent-1", exp="2026-11-20 00:00")], "每日签到"),
      dt.datetime(2026, 11, 17), led)
expired_back = dict(label="签到奖励", total=100, used=30, left=70,
                    expires_on="2026-11-20 00:00", unit="积分", expired=True)
after = merge("trae-cn", "t4", group([expired_back], "每日签到"),
              dt.datetime(2026, 11, 21), led)
check("live_to_expired_one_record", len(led["packs"]) == 1, str(len(led["packs"])))
check("live_to_expired_no_dup_items", sum(len(g["items"]) for g in after) == 1)

# ⑤ 改组名不造新条目（Trae 旧组名迁移只改属性）
led = {"packs": {}}
merge("trae-cn", "t5", group([item(0, exp="2026-11-20 00:00")], "签到奖励"),
      dt.datetime(2026, 11, 17), led)
merge("trae-cn", "t5", group([item(0, exp="2026-11-20 00:00")], "每日签到"),
      dt.datetime(2026, 11, 18), led)
check("rename_no_new_record", len(led["packs"]) == 1, str(len(led["packs"])))

# ⑥ 旧键迁移：同一笔的多个旧快照合并，取最近读数、最早 first_seen
old = {"packs": {}}
k1 = "doubao\x1ft6\x1f平台奖励积分\x1f2026-11-16 23:59\x1f100.000000\x1f0.000000"
k2 = "doubao\x1ft6\x1f平台奖励积分\x1f2026-11-16 23:59\x1f100.000000\x1f300.000000"
old["packs"][k1] = {"product": "doubao", "account": "t6", "group": "平台奖励积分",
                    "expires_on": "2026-11-16 23:59", "total": 100, "used": 0, "left": 100,
                    "first_seen": "2026-11-10 09:00", "last_seen": "2026-11-15 09:00"}
old["packs"][k2] = {"product": "doubao", "account": "t6", "group": "平台奖励积分",
                    "expires_on": "2026-11-16 23:59", "total": 100, "used": 300, "left": 700,
                    "first_seen": "2026-11-15 09:00", "last_seen": "2026-11-16 09:00"}
changed = ns["_quota_ledger_migrate"](old["packs"])
check("migration_changed", changed)
check("migration_collapses_snapshots", len(old["packs"]) == 1, str(len(old["packs"])))
rec = next(iter(old["packs"].values()))
check("migration_latest_used", rec["used"] == 300, str(rec.get("used")))
check("migration_earliest_first_seen", rec["first_seen"] == "2026-11-10 09:00")
after = merge("doubao", "t6", [], dt.datetime(2026, 11, 18), old)
check("migration_restores_one", sum(len(g["items"]) for g in after) == 1)

# ⑦ 厂商不给到账时间：兜底标 first_seen（首次记录），不冒充发放时刻
led = {"packs": {}}
after = merge("doubao", "t7", group([item(0)]), dt.datetime(2026, 11, 17), led)
it = after[0]["items"][0]
check("first_seen_fallback_marked", it.get("granted_src") == "first_seen", str(it))
check("first_seen_fallback_value", it.get("granted_on") == "2026-11-17 00:00:00", str(it.get("granted_on")))

# ⑧ 永久留档：两年以上不再出现的过期笔也不清理
led = {"packs": {}}
merge("doubao", "t8", group([item(0, exp="2024-01-01 00:00")]),
      dt.datetime(2024, 1, 2), led)
after = merge("doubao", "t8", [], dt.datetime(2026, 11, 18), led)
check("no_age_prune", len(led["packs"]) == 1, str(len(led["packs"])))
check("old_expired_restored", sum(len(g["items"]) for g in after) == 1)

# ⑨ 资源 ID 不同但签名相同：带 ID 的读数不会把另一笔的旧记录吞掉
led = {"packs": {}}
merge("doubao", "t9", group([item(0, rid="A"), item(0, rid="B")]),
      dt.datetime(2026, 11, 17), led)
after = merge("doubao", "t9", group([item(5, rid="A")]), dt.datetime(2026, 11, 18), led)
check("rid_match_keeps_twin", len(led["packs"]) == 2, str(len(led["packs"])))
after = merge("doubao", "t9", [], dt.datetime(2026, 12, 7), led)
check("rid_twin_both_restored", sum(len(g["items"]) for g in after) == 2)

# ⑩ 到期日口径变化（活笔周期末 → 历史真实过期日）：按 到账+总额 二级收养认回同一笔
led = {"packs": {}}
live = dict(label="周期包", total=100, used=0, left=100,
            expires_on="2026-12-01 00:00", granted_on="2026-11-01 08:00", unit="积分")
merge("workbuddy", "t10", group([live]), dt.datetime(2026, 11, 17), led)
hist = dict(label="周期包", total=100, used=30, left=70,
            expires_on="2026-11-16 23:59", granted_on="2026-11-01 08:00:00",
            unit="credits", expired=True)
after = merge("workbuddy", "t10", group([hist]), dt.datetime(2026, 11, 18), led)
check("expiry_changed_grant_adopt_one_record", len(led["packs"]) == 1, str(len(led["packs"])))
check("expiry_changed_grant_adopt_no_dup", sum(len(g["items"]) for g in after) == 1)

# ⑪ 旧分类记录 + 新分类返回笔：不许在两个组各补一份（真机 92/90 的回归）
led = {"packs": {}}
merge("trae-cn", "t11", group([item(0, exp="2026-09-30 23:59", total=500)], "积分返还"),
      dt.datetime(2026, 10, 1), led)
hist500 = dict(label="原速通次数兑换", total=500, used=209.7316, left=290.2684,
               expires_on="2026-09-30 23:59", unit="积分", expired=True)
after = merge("trae-cn", "t11",
              group([], "积分返还") + group([hist500], "原速通次数兑换"),
              dt.datetime(2026, 11, 18), led)
check("cross_group_no_dup_record", len(led["packs"]) == 1, str(len(led["packs"])))
check("cross_group_no_dup_items", sum(len(g["items"]) for g in after) == 1,
      str([(g["name"], len(g["items"])) for g in after]))
check("cross_group_new_classification",
      any(g["name"] == "原速通次数兑换" and len(g["items"]) == 1 for g in after))

led = {"packs": {}}
for day, rid in enumerate(("A", "B"), 1):
    merge("doubao", "replacement", group([item(0, rid=rid)]), dt.datetime(2026, 11, day), led)
after = merge("doubao", "replacement", [], dt.datetime(2026, 12, 7), led)
check("successive_ids_keep_both", {i["resource_id"] for g in after for i in g["items"]} == {"A", "B"})
led = {"packs": {}}
for day in (1, 2):
    merge("doubao", "twins", group([item(day), item(day+10)]), dt.datetime(2026, 11, day), led)
after = merge("doubao", "twins", [], dt.datetime(2026, 12, 7), led)
check("anonymous_twins_survive_refresh", sorted(i["used"] for g in after for i in g["items"]) == [2, 12])
led = {"packs": {}}
for attrs in ({}, {"granted_on": "2026-11-01 12:12:12"}, {"granted_on": "2026-11-01", "granted_est": True}):
    result = merge("a", "b", group([dict(item(rid="a"), **attrs)]), dt.datetime(2026, 11, 10), led)
check("official_grant_wins", result[0]["items"][0]["granted_src"] == "vendor" and result[0]["items"][0]["granted_on"].endswith("12:12:12"))
now = dt.datetime(2026, 11, 20, 8, 30)
for exp, flag, expected in [("2026-11-20", False, False), ("2026-11-20 08:29:59", False, True), ("未给出", True, True)]:
    gs = credit_groups([dict(bucket[0], items=[dict(item(exp=exp), expired=flag)])], now)
    check("expiry_" + exp, gs[0]["items"][0]["expired"] == expected)
ns.update(time=time, TRAE_CANON_ORDER=[], _trae_pack_expiry=lambda d: 0,
          _trae_group_of=lambda name, *unused: name,
          DOUBAO_QUOTA_OVERVIEW="fixture", dt_text=lambda ms: "2026-11-01 10:00")
entry = dict(name="原速通次数兑换", credits_limit=500, used_credits_amount=100, expire_time_ms=1)
ns["_trae_expired"] = lambda *args: ([entry, entry.copy()], None)
shaped = ns["_trae_shape"]({}, {}, 0, "", "")["vendorGroups"][0]
check("trae_history_twins_survive_adapter", len(shaped["items"]) == 2 and shaped["remaining"] == 0
      and all(not i.get("granted_on") for i in shaped["items"]))
for payload, valid in [({}, False), ({"incentive_quota_entitlements":[]}, True)]:
    ns["_doubao_req"] = lambda *args, **kw: (payload, None)
    data, error = ns["doubao_quota_overview"]("test")
    check("doubao_empty_contract_" + str(valid), (data is not None) == valid and bool(error) != valid)
print()
if FAILS:
    print(f"{len(FAILS)} FAILED: {FAILS}")
    sys.exit(1)
print("ALL PASS")
