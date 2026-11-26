import { t } from "./locale";
import { useEffect, useState } from "react";
import { EuiButton, EuiButtonEmpty } from "@elastic/eui";
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
        if (status.status === "complete") { setResult(t("GitHub 授权已完成。")); setDevice(undefined); }
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
  return <section className="operation"><div><h2>{t("GitHub 设备授权")}</h2><p>{t("使用浏览器连接 GitHub 账号，授权 StarStack 读取收藏。")}</p></div>
    {device ? <div className="device-code"><span>{t("设备授权码")}</span><strong>{device.user_code}</strong>
      <p>{t("在 GitHub 验证页面输入此码，完成后本页会自动更新。")}</p>
      <div className="row-controls">{/^https:\/\//i.test(device.verification_uri) && <EuiButton fill href={device.verification_uri} target="_blank" rel="noreferrer">{t("打开 GitHub 验证页")}</EuiButton>}
        <EuiButtonEmpty onClick={() => { setDevice(undefined); setResult(t("已停止等待授权。")); }}>{t("取消等待")}</EuiButtonEmpty></div></div>
      : <EuiButton onClick={start} isLoading={busy}>{t("连接 GitHub")}</EuiButton>}
    {result && <p role="status">{result}</p>}{error && <Failure error={error} />}
  </section>;
}

export type Job = "sync" | "index" | "readmes";
export function Maintenance({ stats, admin, job, progress, error, run }: {
  stats: Stats; admin: boolean; job: Job | null; progress: string; error: string; run: (job: Job, full: boolean, requestId?: string) => Promise<void>;
}) {
  return <>
    <div className="page-heading"><div><h1>{t("同步与索引")}</h1><p>{t("连接 GitHub、更新收藏并维护本地语义索引。")}</p></div></div>
    {progress && <p role="status" className="progress-note">{progress}</p>}
    {error && <Failure error={error} />}
    <div className="operations"><DeviceAuth />
      <section className="operation"><div><h2>{t("同步收藏")}</h2><p>{t("增量同步读取最近收藏。全量同步重新核对 GitHub 上的完整 Stars 列表。")}</p></div>
        <dl className="inline-fact"><dt>{t("上次同步")}</dt><dd>{date(stats.last_sync, true)}</dd></dl>
        <div className="row-controls"><EuiButton fill isDisabled={!!job} onClick={() => run("sync", false)}>{t("增量同步")}</EuiButton>
          <EuiButton isDisabled={!!job} onClick={() => run("sync", true)}>{t("全量同步")}</EuiButton></div></section>
      <EmbeddingModels admin={admin} jobActive={!!job} index={rebuild => run("index", rebuild)} fallbackModel={stats.model} />
      <ChatSettings admin={admin} />
      <Schedule jobActive={!!job} fetchReadmes={() => run("readmes", false)} confirm={id => run("sync", false, id)} />
    </div>
  </>;
}
