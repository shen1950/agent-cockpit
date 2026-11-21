"""WorkBuddy 逐笔明细「到期时间 / 耗尽时间」两列的回归（离线，不联网、不读写台账）。

2026-11-21 用户发现同一笔积分：WorkBuddy 客户端显示 11-21 06:03 到期，
驾驶舱明细显示 11-20 21:20。查下来是厂商 ExpiredTime 的语义被用错了——
它是「这一笔结束的时刻」：自然到期时等于周期结束，期内被用光时就是用光那一刻。
全量 233 笔历史里 204 笔是用光的，提前量 0～425 小时不等，拿它当到期日
会让每一笔用光的积分都显示成"提前作废"。

现在：到期日只认 CycleEndTime（名义有效期截止），ExpiredTime 单独作 ended_on，
没提前耗尽就给空串（界面写 ——），与「自然到期」区分开。
运行：python tests/workbuddy-expiry-split.test.py
"""
import importlib.util
from pathlib import Path

SRC = Path(__file__).resolve().parents[1] / "src-tauri" / "vendor" / "agenthub.py"
_spec = importlib.util.spec_from_file_location("wbhub_vendor", SRC)
V = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(V)

FAILS = []


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail and not cond else ""))
    if not cond:
        FAILS.append(name)


def acct(cycle_end, expired, create="2026-10-21 06:03:07", size=100, used=100,
         cycle_used=None, remain=0):
    """一笔资源的原始字段，形状照 get-user-resource-*-packages 的真实返回。"""
    a = {
        "ResourceId": "codebuddy-test-res",
        "PackageCode": "TCACA_code_007_nzdH5h4Nl0",
        "PackageName": "CodeBuddy个人版国内运营裂变包",
        "Status": 3,
        "CreateTime": 1792533787000,
        "CycleStartTime": create,
        "CycleEndTime": cycle_end,
        "ExpiredTime": expired,
        "CapacitySizePrecise": str(size),
        "ActualCycleCapacityUsed": str(used),
        "CycleCapacitySize": size,
        "CycleCapacityRemain": remain,
        "CapacityUnit": "credits",
        "AccountAttributes": [{"Key": "grantReason", "Value": "Buddy 加油站签到"}],
    }
    if cycle_used is not None:
        a["CycleCapacityUsed"] = cycle_used
    return a


# ① 真实案例：到账 10-21 06:03:07，名义到期 11-21 06:03:06，11-20 21:20:04 被扣光
real = acct("2026-11-21 06:03:06", "2026-11-20 21:20:04")
check("到期日 = 周期结束（不再被 ExpiredTime 顶掉）",
      V.dt_text(V._wb_cycle_end(real)) == "2026-11-21 06:03",
      V.dt_text(V._wb_cycle_end(real)))
check("耗尽时间 = ExpiredTime",
      V._wb_ended_on(real, V._wb_cycle_end(real)) == "2026-11-20 21:20",
      V._wb_ended_on(real, V._wb_cycle_end(real)))

items = V._wb_expired_items([real])
row = items["TCACA_code_007_nzdH5h4Nl0"][0]
check("历史笔 expires_on 用周期结束", row["expires_on"] == "2026-11-21 06:03", row["expires_on"])
check("历史笔 ended_on 单列给出", row["ended_on"] == "2026-11-20 21:20", row["ended_on"])

# ② 自然到期：ExpiredTime 与周期结束同一刻（实测那 4 笔没用完的就是这样）
natural = acct("2026-08-01 14:51:20", "2026-08-01 14:51:20", size=100, used=30)
check("自然到期不给耗尽时间", V._wb_ended_on(natural, V._wb_cycle_end(natural)) == "",
      V._wb_ended_on(natural, V._wb_cycle_end(natural)))
check("自然到期 expires_on 仍是周期结束",
      V._wb_pack_items([natural])["TCACA_code_007_nzdH5h4Nl0"][0]["expires_on"] == "2026-08-01 14:51")

# ③ 相差 30 秒：属于同一刻，按自然到期处理（阈值 60 秒）
near = acct("2026-11-21 06:03:06", "2026-11-21 06:02:36")
check("一分钟内不当作提前耗尽", V._wb_ended_on(near, V._wb_cycle_end(near)) == "",
      V._wb_ended_on(near, V._wb_cycle_end(near)))

# ④ 厂商没给 ExpiredTime（历史里 25 笔）
none = acct("2026-11-21 06:03:06", "")
check("没给 ExpiredTime 就是空", V._wb_ended_on(none, V._wb_cycle_end(none)) == "")

# ⑤ 活接口里 Status=3 的笔也要分两列（活接口同样回已用完的笔）
live = acct("2026-11-22 15:48:15", "2026-11-22 10:00:00", size=100, used=100, cycle_used=39, remain=60)
lrow = V._wb_pack_items([live])["TCACA_code_007_nzdH5h4Nl0"][0]
check("活笔 expires_on 用周期结束", lrow["expires_on"] == "2026-11-22 15:48", lrow["expires_on"])
check("活笔 ended_on 给出耗尽时刻", lrow["ended_on"] == "2026-11-22 10:00", lrow["ended_on"])

# ⑥ 没扣光的活笔：两列都要有，而不是整段缺失
alive = acct("2026-11-22 15:48:15", "", size=500, used=0, remain=500)
arow = V._wb_pack_items([alive])["TCACA_code_007_nzdH5h4Nl0"][0]
check("未用尽的活笔 ended_on 为空（界面写 ——）", arow["ended_on"] == "", arow["ended_on"])

print()
# ⑦ 台账往返：写进去要保住、补回来要带出（2026-11-21 第一次部署就栽在这——
#    只改了 Rust 的 merge，真正写台账的是 Python 这一份，界面有列但台账没字段）
led = {"version": 2, "packs": {}}
V.quota_ledger_merge("workbuddy", "测试号", [{"name": "平台奖励积分", "items": [row]}], led=led)
recs = [r for r in led["packs"].values() if r.get("resource_id") == "codebuddy-test-res"]
check("台账写入保住耗尽时间",
      bool(recs) and recs[0].get("ended_on") == "2026-11-20 21:20",
      str([r.get("ended_on") for r in recs]))
back = V._ledger_item(recs[0], "平台奖励积分") if recs else {}
check("台账补回带出耗尽时间", back.get("ended_on") == "2026-11-20 21:20", str(back.get("ended_on")))
check("台账补回到期日仍是名义到期", back.get("expires_on") == "2026-11-21 06:03", str(back.get("expires_on")))

# 自然到期那笔进台账：不许残留上一轮的耗尽时间
led2 = {"version": 2, "packs": {}}
V.quota_ledger_merge("workbuddy", "测试号", [{"name": "平台奖励积分", "items": [row]}], led=led2)
V.quota_ledger_merge("workbuddy", "测试号", [{"name": "平台奖励积分", "items": [
    dict(row, ended_on="")]}], led=led2)
keep = [r for r in led2["packs"].values() if r.get("resource_id") == "codebuddy-test-res"]
check("本轮不给耗尽时间就清掉旧值", bool(keep) and not keep[0].get("ended_on"),
      str([r.get("ended_on") for r in keep]))

print()
if FAILS:
    print(f"{len(FAILS)} 项失败：" + "、".join(FAILS))
    raise SystemExit(1)
print("全部通过")
