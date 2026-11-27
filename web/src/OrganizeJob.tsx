import { useEffect, useRef, useState } from "react";
import { EuiButton } from "@elastic/eui";
import { api, message, type Filters } from "./api";
import { t } from "./locale";
import { Failure } from "./shared";

export type Target = "tags" | "github_lists";
interface Job {
  status: "idle" | "retry_wait" | "running" | "paused" | "failed" | "done";
  retry?: { next_at: number; attempts: number } | null;
  total: number; processed: number; added: number; error: string | null; target: Target; pause_requested?: boolean;
}
const labels = { idle: "", running: "正在整理", retry_wait: "等待自动重试", paused: "已暂停", failed: "整理失败", done: "整理完成" };

export function OrganizeJob({ filter, target, filtered, refresh }: { filter: Filters; target: Target; filtered: boolean; refresh: () => void }) {
  const [job, setJob] = useState<Job>();
  const [readError, setReadError] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [version, setVersion] = useState(0);
  const previous = useRef<Job>();
  useEffect(() => {
    const controller = new AbortController();
    let timer: number;
    async function poll() {
      let wait = 10000;
      try {
        const latest = await api<Job>("organize/job", undefined, controller.signal);
        if (controller.signal.aborted) return;
        if (latest.status === "running") wait = 1300;
        if (previous.current && latest.added !== previous.current.added) refresh();
        previous.current = latest; setJob(latest); setReadError("");
      } catch (e) { if (!controller.signal.aborted) setReadError(message(e)); }
      if (!controller.signal.aborted) timer = window.setTimeout(poll, wait);
    }
    void poll();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [version, refresh]);

  async function act(action: "start" | "pause" | "resume") {
    if (busy) return;
    setBusy(true); setError("");
    try {
      await api(`organize/${action}`, action === "start" ? { filter, target, confirmed: true } : {});
      const latest = await api<Job>("organize/job");
      setJob(latest);
      setVersion(n => n + 1);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }

  const active = job?.status === "retry_wait" || job?.status === "running" || job?.status === "paused" || job?.status === "failed";
  const destination = target === "tags" ? t("本地标签") : "GitHub Lists";
  return <section className="organize-job"><h3>{t("后台自动整理")}</h3>
    {job && job.status !== "idle" && <div className="organize-progress">
      <strong>{t(job.pause_requested ? "正在暂停…" : labels[job.status])} · {job.target === "github_lists" ? "GitHub Lists" : t("本地标签")}</strong>
      <p>{t("已处理 {0} / {1} 个仓库，已添加 {2} 项。", job.processed, job.total, job.added)}</p>
      {job.total > 0 && <progress aria-label={t("自动整理进度")} max={job.total} value={job.processed} />}
      {job.status === "retry_wait" && job.retry && <p role="status">{t("将在 {0} 之后进行第 {1} 次自动重试。", new Date(job.retry.next_at * 1000).toLocaleString(), job.retry.attempts)}</p>}
      {job.error && <Failure error={t(job.error)} />}
      {job.target === "github_lists" && target !== job.target && (job.status === "paused" || job.status === "failed") &&
        <p>{t("此任务将继续向已有的 GitHub 收藏分类添加仓库。")}</p>}
    </div>}
    {job && !active && <>
      <p>{t("确认后，AI 将逐批整理{0}并自动添加到{1}。关闭页面后会继续运行。", filtered ? t("符合筛选条件的所有仓库") : t("全部收藏"), destination)}</p>
      <EuiButton size="s" isDisabled={busy} onClick={() => act("start")}>{target === "tags" ? t("确认自动添加本地标签") : t("确认自动更新 GitHub Lists")}</EuiButton>
    </>}
    {(job?.status === "running" || job?.status === "retry_wait") && <EuiButton size="s" isDisabled={busy || job.pause_requested} onClick={() => act("pause")}>{t("暂停自动整理")}</EuiButton>}
    {(job?.status === "paused" || job?.status === "failed") && <EuiButton size="s" isDisabled={busy} onClick={() => act("resume")}>{t("继续自动整理")}</EuiButton>}
    {error && <Failure error={error} />}
    {readError && <Failure error={readError} retry={() => setVersion(n => n + 1)} />}
  </section>;
}
