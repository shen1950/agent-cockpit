// Qoder CN 的本地桥：把「官方 CN 客户端运行时」包装成一个 OpenAI 兼容端点。
//
// 为什么必须这样：国内网关的推理通道要官方客户端用 WASM 机器身份签出来的签名，
// 那个密钥来自「登录后的本地凭据」，从 auth.v1.dat 里的 dt- 令牌派生不出来。
// 所以这里不重放协议，而是**跑它自己的 CLI**（<Qoder CN 安装目录>/.qoder-versions/<v>/
// resources/app.asar.unpacked/node_modules/@qoder-ai/qoder-cn-agent-sdk/dist/_worker/
// qoder-worker-runtime.obf.mjs），签名由它完成，我们只做 OpenAI ↔ CLI 的翻译。
//
// 环境变量（全部由驾驶舱设置，手工起也行）：
//   CN_CONFIG_DIR  该账号的 CLI 配置目录（登录态在 <它>/.auth/user）
//   CN_NODE        Qoder CN.exe（用 ELECTRON_RUN_AS_NODE=1 当 node 跑，免装 Node）
//   CN_RUNTIME     qoder-worker-runtime.obf.mjs 的绝对路径
//   CNBRIDGE_PORT  监听端口，默认 18211
//   CNBRIDGE_KEY   调用方要带的 Bearer，默认 qcn-bridge
//   CN_MODEL       默认 auto
//   CN_TIMEOUT_MS  单次超时，默认 180000
import http from "node:http";
import { spawn } from "node:child_process";

const PORT = Number(process.env.CNBRIDGE_PORT || 18211);
const KEY = process.env.CNBRIDGE_KEY || "qcn-bridge";
const NODE_EXE = process.env.CN_NODE || "";
const RUNTIME = process.env.CN_RUNTIME || "";
const CONFIG_DIR = process.env.CN_CONFIG_DIR || "";
const MODEL = process.env.CN_MODEL || "auto";
const TIMEOUT_MS = Number(process.env.CN_TIMEOUT_MS || 180000);
const MAX_CONCURRENT = 2;
const MAX_QUEUE = 24;

let running = 0;
const queue = [];
let seq = 0;

const log = (...a) => console.log(new Date().toISOString(), ...a);

function flatten(messages) {
  if (!Array.isArray(messages) || messages.length === 0) return "";
  const text = (c) => {
    if (typeof c === "string") return c;
    if (Array.isArray(c)) {
      return c
        .map((p) => (p && p.type === "text" ? p.text : p && p.type === "image_url" ? "[图片]" : ""))
        .join("");
    }
    return "";
  };
  if (messages.length === 1 && messages[0].role === "user") return text(messages[0].content);
  const parts = [];
  for (const m of messages) {
    const t = text(m.content);
    if (!t.trim()) continue;
    if (m.role === "system") parts.push(`系统要求：${t}`);
    else if (m.role === "assistant") parts.push(`助手：${t}`);
    else if (m.role === "tool") parts.push(`工具返回：${t}`);
    else parts.push(`用户：${t}`);
  }
  return parts.join("\n\n");
}

function runOnce(prompt) {
  return new Promise((resolve) => {
    if (!NODE_EXE || !RUNTIME) {
      return resolve({ ok: false, error: "桥没配好：CN_NODE / CN_RUNTIME 为空" });
    }
    const env = { ...process.env, ELECTRON_RUN_AS_NODE: "1", QODERCN_CONFIG_DIR: CONFIG_DIR };
    const args = [
      RUNTIME,
      "-p",
      prompt,
      "--output-format",
      "json",
      "--model",
      MODEL,
      "--disallowed-tools",
      "*",
      "--no-session-persistence",
    ];
    const child = spawn(NODE_EXE, args, { env, windowsHide: true });
    let out = "";
    let err = "";
    let done = false;
    const timer = setTimeout(() => {
      if (!done) {
        try {
          child.kill();
        } catch {}
        done = true;
        resolve({ ok: false, error: `上游超时（${Math.round(TIMEOUT_MS / 1000)}s）` });
      }
    }, TIMEOUT_MS);
    child.stdout.on("data", (d) => (out += d.toString("utf8")));
    child.stderr.on("data", (d) => (err += d.toString("utf8")));
    child.on("error", (e) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      resolve({ ok: false, error: `起不了客户端：${e.message}` });
    });
    child.on("close", () => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      // stdout 里最后一条能解析的 JSON 才是结果；前面可能有日志行
      const lines = out.split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
      for (let i = lines.length - 1; i >= 0; i--) {
        if (!lines[i].startsWith("{")) continue;
        try {
          const j = JSON.parse(lines[i]);
          if (j && typeof j === "object" && ("result" in j || "is_error" in j)) {
            if (j.is_error || j.subtype === "error") {
              return resolve({ ok: false, error: String(j.result || "上游返回错误") });
            }
            return resolve({
              ok: true,
              text: String(j.result || ""),
              credits: j.total_credits || 0,
              usage: j.usage || null,
            });
          }
        } catch {}
      }
      resolve({ ok: false, error: `没读到结果${err ? "：" + err.trim().slice(-200) : ""}` });
    });
  });
}

function withSlot(fn) {
  return new Promise((resolve) => {
    const job = { fn, resolve };
    if (running < MAX_CONCURRENT) {
      running++;
      job.fn().then(resolve).finally(() => {
        running--;
        const next = queue.shift();
        if (next) withSlot(next.fn).then(next.resolve);
      });
    } else if (queue.length >= MAX_QUEUE) {
      resolve({ ok: false, error: "桥排队满了，稍后再试", busy: true });
    } else {
      queue.push(job);
    }
  });
}

function send(res, code, obj, extra = {}) {
  const body = JSON.stringify(obj);
  res.writeHead(code, {
    "Content-Type": "application/json; charset=utf-8",
    "Content-Length": Buffer.byteLength(body),
    "Access-Control-Allow-Origin": "*",
    ...extra,
  });
  res.end(body);
}

function openaiBody(text, model, credits) {
  return {
    id: `chatcmpl-qcn-${(++seq).toString(16)}`,
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [{ index: 0, message: { role: "assistant", content: text }, finish_reason: "stop" }],
    usage: {
      prompt_tokens: 0,
      completion_tokens: 0,
      total_tokens: 0,
      credits,
    },
  };
}

function sseChunks(text, model) {
  const id = `chatcmpl-qcn-${(++seq).toString(16)}`;
  const created = Math.floor(Date.now() / 1000);
  const frames = [];
  const push = (delta, finish) =>
    frames.push(
      `data: ${JSON.stringify({
        id,
        object: "chat.completion.chunk",
        created,
        model,
        choices: [{ index: 0, delta, finish_reason: finish }],
      })}\n\n`
    );
  push({ role: "assistant", content: "" }, null);
  const step = 24;
  for (let i = 0; i < text.length; i += step) push({ content: text.slice(i, i + step) }, null);
  push({}, "stop");
  frames.push("data: [DONE]\n\n");
  return frames;
}

const server = http.createServer((req, res) => {
  if (req.method === "OPTIONS") {
    res.writeHead(204, {
      "Access-Control-Allow-Origin": "*",
      "Access-Control-Allow-Headers": "authorization,content-type",
      "Access-Control-Allow-Methods": "POST,GET,OPTIONS",
    });
    return res.end();
  }
  const url = new URL(req.url, "http://127.0.0.1");
  if (req.method === "GET" && url.pathname === "/health") {
    return send(res, 200, { ok: true, model: MODEL, config_dir: CONFIG_DIR, running, queued: queue.length });
  }
  const auth = (req.headers.authorization || "").replace(/^Bearer\s+/i, "").trim();
  if (url.pathname === "/v1/models" && req.method === "GET") {
    if (auth !== KEY) return send(res, 401, { error: { message: "bad key" } });
    return send(res, 200, { object: "list", data: [{ id: MODEL, object: "model" }, { id: "gpt-5", object: "model" }] });
  }
  if (url.pathname !== "/v1/chat/completions" || req.method !== "POST") {
    return send(res, 404, { error: { message: "只有 /v1/chat/completions 和 /v1/models" } });
  }
  if (auth !== KEY) {
    return send(res, 401, { error: { message: "分发 Key 不对", type: "auth_error" } });
  }
  let raw = "";
  req.on("data", (d) => (raw += d));
  req.on("end", async () => {
    let body;
    try {
      body = JSON.parse(raw || "{}");
    } catch {
      return send(res, 400, { error: { message: "请求体不是 JSON" } });
    }
    const model = String(body.model || MODEL);
    const prompt = flatten(body.messages);
    if (!prompt.trim()) return send(res, 400, { error: { message: "messages 是空的" } });
    const r = await withSlot(() => runOnce(prompt));
    if (!r.ok) {
      const code = r.busy ? 429 : 502;
      return send(res, code, { error: { message: r.error, type: r.busy ? "busy" : "upstream_error" } });
    }
    if (!body.stream) return send(res, 200, openaiBody(r.text, model, r.credits));
    res.writeHead(200, {
      "Content-Type": "text/event-stream; charset=utf-8",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
      "Access-Control-Allow-Origin": "*",
    });
    for (const f of sseChunks(r.text, model)) res.write(f);
    res.end();
  });
});

server.listen(PORT, "127.0.0.1", () => {
  log(`Qoder CN 桥已就绪 http://127.0.0.1:${PORT}/v1  model=${MODEL} config=${CONFIG_DIR}`);
  if (!NODE_EXE || !RUNTIME) log("⚠ 缺 CN_NODE / CN_RUNTIME，请求会直接报错");
});

// 驾驶舱退出后桥要跟着退，别留一个孤儿进程占着端口
const PARENT = Number(process.env.CNBRIDGE_PARENT_PID || 0);
if (PARENT) {
  setInterval(() => {
    try {
      process.kill(PARENT, 0);
    } catch {
      log("驾驶舱已退出，桥也退出");
      process.exit(0);
    }
  }, 15000);
}
