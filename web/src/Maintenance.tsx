import { t } from "./locale";
import { useEffect, useState } from "react";
import { EuiButton, EuiButtonEmpty, EuiConfirmModal, EuiTab, EuiTabs } from "@elastic/eui";
import { api, date, message, type Stats } from "./api";
import { EmbeddingModels } from "./EmbeddingModels";
import { ChatSettings } from "./ChatSettings";
import { Schedule } from "./Schedule";
import { Failure } from "./shared";

interface Device { code: string; user_code: string; verification_uri: string; interval: number; expires_in: number }

function DeviceAuth() {
  const [device, setDevice] = useState<Device>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState("");
  const [source, setSource] = useState<"device" | "gh" | "environment" | null>();
  const [revision, setRevision] = useState(0);
  const [confirm, setConfirm] = useState(false);

  useEffect(() => {
    const controller = new AbortController();
    api<{ source: typeof source }>("github", undefined, controller.signal)
      .then(data => { if (!controller.signal.aborted) setSource(data.source); })
      .catch(e => { if (!controller.signal.aborted) setError(message(e)); });
    return () => controller.abort();
  }, [revision]);

  useEffect(() => {
    if (!device) return;
    const controller = new AbortController();
    let timer: number;
    const expiry = window.setTimeout(() => {
      controller.abort();
      setError(t("设备授权码已过期，请重新连接。"));
      setDevice(undefined);
    }, device.expires_in * 1000);
    const poll = async () => {
      try {
        const status = await api<{ status: "pending" | "complete"; interval: number }>("device/poll", { code: device.code }, controller.signal);
        if (controller.signal.aborted) return;
        if (status.status === "complete") { setResult(t("GitHub 授权已完成。")); setSource("device"); setDevice(undefined); }
        else timer = window.setTimeout(poll, Math.max(1, status.interval || device.interval) * 1000);
      } catch (e) {
        if (!controller.signal.aborted) { setError(message(e)); setDevice(undefined); }
      }
    };
    timer = window.setTimeout(poll, Math.max(1, device.interval) * 1000);
    return () => { controller.abort(); clearTimeout(timer); clearTimeout(expiry); };
  }, [device]);

  async function start() {
    setBusy(true); setError(""); setResult("");
    try { setDevice(await api<Device>("device/start", {})); }
    catch (e) { setError(message(e)); } finally { setBusy(false); }
  }
  async function cancel() {
    if (!device) return;
    const code = device.code;
    setDevice(undefined);
    setBusy(true);
    try {
      await api("device/cancel", { code });
      setResult(t("已停止等待授权。"));
      setRevision(n => n + 1);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }
  async function disconnect() {
    setBusy(true); setError(""); setResult("");
    try {
      await api("github/disconnect", {});
      setSource(null); setConfirm(false);
      setResult(t("已断开 GitHub。"));
    } catch (e) { setError(message(e)); setConfirm(false); } finally { setBusy(false); }
  }
  return <section className="operation"><div><h2>{t("GitHub 设备授权")}</h2><p role="status">{source === "environment" ? t("已通过部署环境中的 GitHub 令牌连接。") :
    source === "gh" ? t("已通过本机 GitHub CLI 连接。") :
    source === "device" ? t("已连接 GitHub。") :
    source === null ? t("使用浏览器连接 GitHub 账号，授权 StarStack 读取收藏。") : t("正在读取 GitHub 连接状态…")}</p></div>
    {device ? <div className="device-code"><span>{t("设备授权码")}</span><strong>{device.user_code}</strong>
      <p>{t("在 GitHub 验证页面输入此码，完成后本页会自动更新。")}</p>
      <div className="row-controls">{/^https:\/\//i.test(device.verification_uri) && <EuiButton fill href={device.verification_uri} target="_blank" rel="noreferrer">{t("打开 GitHub 验证页")}</EuiButton>}
        <EuiButtonEmpty onClick={cancel}>{t("取消等待")}</EuiButtonEmpty></div></div>
      : source !== undefined && source !== "environment" && <div className="row-controls">
        <EuiButton onClick={start} isLoading={busy}>{t(source ? "重新授权" : "连接 GitHub")}</EuiButton>
        {source && <EuiButtonEmpty isDisabled={busy} onClick={() => setConfirm(true)}>{t("断开 GitHub")}</EuiButtonEmpty>}
      </div>}
    {confirm && <EuiConfirmModal title={t("断开 GitHub？")} onCancel={() => { if (!busy) setConfirm(false); }}
      onConfirm={disconnect} cancelButtonText={t("取消")} confirmButtonText={t("断开 GitHub")} buttonColor="danger" isLoading={busy}>
      <p>{t("本地收藏与批注会保留。重新连接后可继续同步。")}</p>
    </EuiConfirmModal>}
    {result && <p role="status">{result}</p>}{error && <Failure error={error} retry={source === undefined ? () => { setError(""); setRevision(n => n + 1); } : undefined} />}
  </section>;
}

const tabs = [
  ["sync", "收藏同步"], ["schedule", "计划与额度"], ["index", "语义索引"], ["market", "模型市场"], ["chat", "AI 聊天"],
] as const;

export interface JobReport { message: string; warnings: string[]; details: string[] }
export type Job = "sync" | "index" | "readmes";
export function Maintenance({ stats, admin, job, progress, report, error, run }: {
  stats: Stats; admin: boolean; job: Job | null; progress: string; report: JobReport | undefined; error: string; run: (job: Job, full: boolean, requestId?: string) => Promise<void>;
}) {
  const [active, setActive] = useState<(typeof tabs)[number][0]>("sync");
  return <>
    <div className="page-heading"><div><h1>{t("同步与索引")}</h1><p>{t("连接 GitHub、更新收藏并维护本地语义索引。")}</p></div></div>
    {progress && <div className={`job-notice${report?.warnings.length ? " job-notice--warning" : ""}`}>
      <div role="status"><strong>{progress}</strong>{report?.warnings.map((warning, i) => <p key={i}>{t(warning)}</p>)}</div>
      {!!report?.details.length && <details><summary>{t("任务日志")}</summary><ul>{report.details.map((detail, i) =>
        <li key={i}>{t(detail)}</li>)}</ul></details>}
    </div>}
    {error && !report?.warnings.length && <Failure error={error} />}
    <EuiTabs className="maintenance-tabs" aria-label={t("同步与索引")} onKeyDown={event => {
      const current = tabs.findIndex(([id]) => id === active);
      const next = event.key === "ArrowRight" ? (current + 1) % tabs.length :
        event.key === "ArrowLeft" ? (current + tabs.length - 1) % tabs.length :
        event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1 : -1;
      if (next < 0) return;
      event.preventDefault();
      const [id] = tabs[next];
      setActive(id);
      document.getElementById(`maintenance-tab-${id}`)?.focus();
    }}>
      {tabs.map(([id, label]) => <EuiTab key={id} id={`maintenance-tab-${id}`} aria-controls={`maintenance-panel-${id}`}
        tabIndex={active === id ? 0 : -1} isSelected={active === id} onClick={() => setActive(id)}>{t(label)}</EuiTab>)}
    </EuiTabs>
    <div className="operations" hidden={active !== "sync"} role="tabpanel" id="maintenance-panel-sync" aria-labelledby="maintenance-tab-sync"><DeviceAuth />
      <section className="operation"><div><h2>{t("同步收藏")}</h2><p>{t("增量同步读取最近收藏。全量同步重新核对 GitHub 上的完整 Stars 列表。")}</p></div>
        <dl className="inline-fact"><dt>{t("上次同步")}</dt><dd>{date(stats.last_sync, true)}</dd></dl>
        <div className="row-controls"><EuiButton fill isDisabled={!!job} onClick={() => run("sync", false)}>{t("增量同步")}</EuiButton>
          <EuiButton isDisabled={!!job} onClick={() => run("sync", true)}>{t("全量同步")}</EuiButton></div></section>
    </div>
    <EmbeddingModels active={active} openMarket={() => {
      setActive("market");
      document.getElementById("maintenance-tab-market")?.focus();
    }} admin={admin} jobActive={!!job} index={rebuild => run("index", rebuild)} fallbackModel={stats.model} />
    <div className="operations" hidden={active !== "chat"} role="tabpanel" id="maintenance-panel-chat" aria-labelledby="maintenance-tab-chat">
      <ChatSettings admin={admin} active={active === "chat"} />
    </div>
    <div className="operations" hidden={active !== "schedule"} role="tabpanel" id="maintenance-panel-schedule" aria-labelledby="maintenance-tab-schedule">
      <Schedule jobActive={!!job} fetchReadmes={() => run("readmes", false)} confirm={id => run("sync", false, id)} />
    </div>
  </>;
}
